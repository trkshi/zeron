//! Turn token usage for DeepSeek Harness, read from DSH's own session log.
//!
//! DSH's ACP adapter settles `session/prompt` with only a `stopReason` and
//! turns each reply's usage into a context-occupancy `usage_update`, so the
//! token counts its own UI shows never reach Zeron over ACP. DSH does persist
//! them: every `assistant/message` event in
//! `$DSH_HOME/sessions/<project>/<session id>/session.v4.jsonl.zstd` carries
//! the request's `usage`, plus a timed stream to measure generation speed.
//! After each turn this reads the turns DSH finished since the last report and
//! settles them as one `TurnUsage`. The log format is private to DSH; anything
//! unexpected reads as "no usage", never as an error.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use zeron_proto::{GenerationUsage, TokenUsage};

use crate::generation;

/// How long to wait for DSH to persist a settled turn's events.
const SETTLE_WAIT: Duration = Duration::from_secs(2);
const SETTLE_POLL: Duration = Duration::from_millis(100);

/// One DSH session's log and the last turn already reported from it.
pub(super) struct DshUsageLog {
    session_id: String,
    path: Option<PathBuf>,
    reported_turn: u64,
}

impl DshUsageLog {
    /// Track `session_id`, treating every turn already in its log (a resumed
    /// session's history) as reported.
    pub async fn open(session_id: &str) -> Self {
        let mut log = Self {
            session_id: session_id.to_owned(),
            path: None,
            reported_turn: 0,
        };
        if let Some(scan) = log.scan().await {
            log.reported_turn = scan.last_turn;
        }
        log
    }

    /// Usage for the turns DSH finished since the last call, once the log
    /// shows the newest one ended (or the settle wait runs out).
    pub async fn settle(&mut self) -> Option<TokenUsage> {
        let deadline = tokio::time::Instant::now() + SETTLE_WAIT;
        let mut scan = self.scan().await;
        while !scan
            .as_ref()
            .is_some_and(|scan| scan.last_turn > self.reported_turn && scan.last_ended)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(SETTLE_POLL).await;
            scan = self.scan().await;
        }
        let scan = scan?;
        if scan.last_turn <= self.reported_turn {
            return None;
        }
        let after = std::mem::replace(&mut self.reported_turn, scan.last_turn);
        let bytes = scan.bytes;
        tokio::task::spawn_blocking(move || turn_usage(&bytes, after))
            .await
            .ok()
            .flatten()
    }

    async fn scan(&mut self) -> Option<Scan> {
        if self.path.is_none() {
            let id = self.session_id.clone();
            self.path = tokio::task::spawn_blocking(move || find_log(&id))
                .await
                .ok()
                .flatten();
        }
        let path = self.path.clone()?;
        tokio::task::spawn_blocking(move || {
            let bytes = read_log(&path)?;
            let (last_turn, last_ended) = last_turn(&bytes);
            Some(Scan {
                bytes,
                last_turn,
                last_ended,
            })
        })
        .await
        .ok()
        .flatten()
    }
}

struct Scan {
    bytes: Vec<u8>,
    last_turn: u64,
    last_ended: bool,
}

/// The session's newest log generation. Its project folder is a lossy key of
/// the session's cwd, so match the session id folder under any project
/// instead of re-deriving the key. A UUID session id is its own path segment.
fn find_log(session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return None;
    }
    let root = crate::dsh_credentials::home()?.join("sessions");
    std::fs::read_dir(root).ok()?.flatten().find_map(|project| {
        let dir = project.path().join(session_id);
        newest_generation(&dir)
    })
}

fn newest_generation(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let version = name
                .strip_prefix("session.v")?
                .split_once(".jsonl")?
                .0
                .parse::<u32>()
                .ok()?;
            Some((version, entry.path()))
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, path)| path)
}

/// The log's JSONL text, up to its last complete line. DSH appends one zstd
/// frame per batch, so a frame still being written only cuts the tail.
fn read_log(path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    if path.extension().is_some_and(|ext| ext == "zstd") {
        let mut decoder = zstd::stream::read::Decoder::new(file).ok()?;
        let mut chunk = [0u8; 64 * 1024];
        loop {
            match decoder.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            }
        }
    } else {
        std::io::BufReader::new(file).read_to_end(&mut bytes).ok()?;
    }
    let end = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    bytes.truncate(end);
    Some(bytes)
}

fn events(bytes: &[u8]) -> impl Iterator<Item = Value> + '_ {
    bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
}

fn turn_of(event: &Value) -> Option<u64> {
    event["data"]["turn"].as_u64()
}

/// The newest turn DSH started, and whether it has ended.
fn last_turn(bytes: &[u8]) -> (u64, bool) {
    let mut last = (0, false);
    for event in events(bytes) {
        match (event["type"].as_str(), turn_of(&event)) {
            (Some("turn/start"), Some(turn)) if turn >= last.0 => last = (turn, false),
            (Some("turn/end"), Some(turn)) if turn == last.0 => last.1 = true,
            _ => {}
        }
    }
    last
}

/// Summed usage of every reply in turns after `after`. DSH's `inputTokens`
/// excludes cache reads and writes; Zeron's input includes them.
fn turn_usage(bytes: &[u8], after: u64) -> Option<TokenUsage> {
    let mut total = TokenUsage::default();
    let mut generation_usage: Option<GenerationUsage> = None;
    let mut seen = false;
    for event in events(bytes) {
        if event["type"].as_str() != Some("assistant/message")
            || turn_of(&event).is_none_or(|turn| turn <= after)
        {
            continue;
        }
        let usage = &event["data"]["usage"];
        let count = |key: &str| usage[key].as_u64();
        let Some(output) = count("outputTokens") else {
            continue;
        };
        seen = true;
        let cache_read = count("cacheReadTokens").unwrap_or(0);
        let cache_write = count("cacheWriteTokens").unwrap_or(0);
        let input = count("inputTokens").unwrap_or(0) + cache_read + cache_write;
        let add = |field: &mut Option<u64>, value: u64| {
            *field = Some(field.unwrap_or(0).saturating_add(value));
        };
        add(&mut total.input_tokens, input);
        add(&mut total.output_tokens, output);
        add(&mut total.cached_input_tokens, cache_read);
        add(&mut total.cache_write_input_tokens, cache_write);
        add(
            &mut total.total_tokens,
            count("totalTokens").unwrap_or(input + output),
        );
        if let Some(elapsed) = decode_window(&event) {
            generation_usage = generation::accumulate(generation_usage, output, elapsed);
        }
    }
    total.generation = generation_usage;
    seen.then_some(total)
}

/// From the reply's first streamed token to its settlement, the window DSH's
/// own tokens-per-second uses. Bursts too short to measure are left out.
fn decode_window(event: &Value) -> Option<Duration> {
    let end = event["time"].as_u64()?;
    let stream = event["data"]["stream"].as_array()?;
    let first = stream
        .iter()
        .find_map(|record| match record["type"].as_str()? {
            "chunk" => match record["chunk"]["type"].as_str()? {
                "block-start" | "block-end" | "finish" => None,
                _ => record["time"].as_u64(),
            },
            _ => record["time0"].as_u64(),
        })?;
    let elapsed = Duration::from_millis(end.checked_sub(first)?);
    let records = u32::try_from(stream.len()).unwrap_or(u32::MAX);
    generation::qualifies(records, elapsed).then_some(elapsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(turn: u64, time: u64, input: u64, output: u64, cache: u64) -> String {
        serde_json::json!({
            "type": "assistant/message",
            "time": time,
            "data": {
                "turn": turn,
                "step": 1,
                "usage": {
                    "inputTokens": input,
                    "outputTokens": output,
                    "cacheReadTokens": cache,
                    "cacheWriteTokens": 0,
                    "totalTokens": input + output + cache,
                },
                "stream": [
                    { "type": "chunk", "time": time - 2000, "chunk": { "type": "block-start" } },
                    { "type": "reasoning-chunks", "time0": time - 1000, "texts": ["a"] },
                    { "type": "text-chunks", "time0": time - 500, "texts": ["b"] },
                    { "type": "chunk", "time": time, "chunk": { "type": "finish" } },
                ],
            },
        })
        .to_string()
    }

    fn log(lines: &[String]) -> Vec<u8> {
        let mut text = lines.join("\n");
        text.push('\n');
        text.into_bytes()
    }

    #[test]
    fn sums_new_turns_with_cache_in_input() {
        let bytes = log(&[
            r#"{"type":"turn/start","time":1,"data":{"turn":1}}"#.into(),
            reply(1, 10_000, 5, 7, 100),
            r#"{"type":"turn/end","time":2,"data":{"turn":1}}"#.into(),
            r#"{"type":"turn/start","time":3,"data":{"turn":2}}"#.into(),
            reply(2, 20_000, 900, 200, 12_000),
            reply(2, 30_000, 100, 100, 13_000),
            r#"{"type":"turn/end","time":4,"data":{"turn":2}}"#.into(),
        ]);
        assert_eq!(last_turn(&bytes), (2, true));
        let usage = turn_usage(&bytes, 1).unwrap();
        assert_eq!(usage.input_tokens, Some(1000 + 25_000));
        assert_eq!(usage.output_tokens, Some(300));
        assert_eq!(usage.cached_input_tokens, Some(25_000));
        assert_eq!(usage.total_tokens, Some(26_300));
        // 300 tokens over two 1 s decode windows.
        assert_eq!(usage.generation.unwrap().tps(), Some(150.0));
        assert_eq!(turn_usage(&bytes, 2), None);
    }

    #[test]
    fn an_unfinished_turn_is_not_ended() {
        let bytes = log(&[
            r#"{"type":"turn/start","time":1,"data":{"turn":1}}"#.into(),
            r#"{"type":"turn/end","time":2,"data":{"turn":1}}"#.into(),
            r#"{"type":"turn/start","time":3,"data":{"turn":2}}"#.into(),
        ]);
        assert_eq!(last_turn(&bytes), (2, false));
    }

    #[test]
    fn reads_concatenated_frames_and_drops_a_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.v4.jsonl.zstd");
        let first = log(&[r#"{"type":"turn/start","time":1,"data":{"turn":1}}"#.into()]);
        let second = log(&[r#"{"type":"turn/end","time":2,"data":{"turn":1}}"#.into()]);
        let mut bytes = zstd::encode_all(&first[..], 0).unwrap();
        bytes.extend(zstd::encode_all(&second[..], 0).unwrap());
        let torn = zstd::encode_all(&b"{\"type\":\"turn/start\"}\n"[..], 0).unwrap();
        bytes.extend(&torn[..torn.len() / 2]);
        std::fs::write(&path, bytes).unwrap();
        let text = read_log(&path).unwrap();
        assert_eq!(last_turn(&text), (1, true));
        assert_eq!(newest_generation(dir.path()), Some(path));
    }
}
