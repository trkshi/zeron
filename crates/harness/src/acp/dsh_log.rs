//! What a DeepSeek Harness prompt did, read from DSH's own session log.
//!
//! DSH's ACP adapter settles `session/prompt` with only a `stopReason` and
//! turns each reply's usage into a context-occupancy `usage_update`, so the
//! token counts its own UI shows never reach Zeron over ACP. Slash commands,
//! which Zeron's DSH plugin runs instead of sending them to the model, report
//! nothing over ACP either. DSH persists both: every `assistant/message` event
//! in `$DSH_HOME/sessions/<project>/<session id>/session.v4.jsonl.zstd`
//! carries the request's `usage` plus a timed stream to measure generation
//! speed, and every command leaves a `command/done` with its result text.
//! After each prompt this reads the events DSH logged since the last one. The
//! log format is private to DSH; anything unexpected reads as "nothing
//! reported", never as an error.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use zeron_proto::{GenerationUsage, TokenUsage};

use crate::generation;

/// How long to wait for DSH to persist a settled prompt's events.
const SETTLE_WAIT: Duration = Duration::from_secs(2);
const SETTLE_POLL: Duration = Duration::from_millis(100);

/// One DSH session's log and the newest event already reported from it.
pub(super) struct DshLog {
    session_id: String,
    path: Option<PathBuf>,
    reported_seq: Option<u64>,
}

/// What one prompt did: model turns' summed usage and slash-command results.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Activity {
    pub usage: Option<TokenUsage>,
    /// Each finished command's result text (possibly empty), in log order.
    pub commands: Vec<String>,
    last_seq: Option<u64>,
    /// Something finished and nothing is still open.
    settled: bool,
}

impl Activity {
    /// The commands' result texts as one reply, when any ran and said something.
    pub fn command_reply(&self) -> Option<String> {
        let texts: Vec<&str> = self
            .commands
            .iter()
            .map(|text| text.trim())
            .filter(|text| !text.is_empty())
            .collect();
        (!texts.is_empty()).then(|| texts.join("\n\n"))
    }
}

impl DshLog {
    /// Track `session_id`, treating everything already in its log (a resumed
    /// session's history) as reported.
    pub async fn open(session_id: &str) -> Self {
        let mut log = Self {
            session_id: session_id.to_owned(),
            path: None,
            reported_seq: None,
        };
        if let Some(activity) = log.scan().await {
            log.reported_seq = activity.last_seq;
        }
        log
    }

    /// What DSH logged since the last call, once a turn or command it
    /// started has finished (or the settle wait runs out).
    pub async fn settle(&mut self) -> Activity {
        let deadline = tokio::time::Instant::now() + SETTLE_WAIT;
        let mut activity = self.scan().await;
        while !activity.as_ref().is_some_and(|activity| activity.settled)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(SETTLE_POLL).await;
            activity = self.scan().await;
        }
        let activity = activity.unwrap_or_default();
        if activity.last_seq.is_some() {
            self.reported_seq = activity.last_seq;
        }
        activity
    }

    async fn scan(&mut self) -> Option<Activity> {
        if self.path.is_none() {
            let id = self.session_id.clone();
            self.path = tokio::task::spawn_blocking(move || find_log(&id))
                .await
                .ok()
                .flatten();
        }
        let path = self.path.clone()?;
        let after = self.reported_seq;
        tokio::task::spawn_blocking(move || Some(activity(&read_log(&path)?, after)))
            .await
            .ok()
            .flatten()
    }
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

/// Everything logged after `after` (every event when `None`). Turns and
/// commands open and close; the activity is settled once at least one closed
/// and none is still open. DSH's `inputTokens` excludes cache reads and
/// writes; Zeron's input includes them.
fn activity(bytes: &[u8], after: Option<u64>) -> Activity {
    let mut activity = Activity {
        last_seq: after,
        ..Activity::default()
    };
    let mut total = TokenUsage::default();
    let mut generation_usage: Option<GenerationUsage> = None;
    let mut open_turn = false;
    let mut open_command = false;
    let mut closed = false;
    for event in bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
    {
        let Some(seq) = event["seq"].as_u64() else {
            continue;
        };
        if after.is_some_and(|after| seq <= after) {
            continue;
        }
        activity.last_seq = Some(activity.last_seq.map_or(seq, |last| last.max(seq)));
        match event["type"].as_str() {
            Some("turn/start") => open_turn = true,
            Some("turn/end") => (open_turn, closed) = (false, true),
            Some("command/run") => open_command = true,
            Some("command/done") => {
                (open_command, closed) = (false, true);
                let text = event["data"]["text"].as_str().unwrap_or_default();
                activity.commands.push(text.to_owned());
            }
            Some("assistant/message") => {
                let usage = &event["data"]["usage"];
                let count = |key: &str| usage[key].as_u64();
                let Some(output) = count("outputTokens") else {
                    continue;
                };
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
            _ => {}
        }
    }
    if total.output_tokens.is_some() {
        total.generation = generation_usage;
        activity.usage = Some(total);
    }
    activity.settled = closed && !open_turn && !open_command;
    activity
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

    fn event(seq: u64, kind: &str, data: Value) -> String {
        serde_json::json!({ "type": kind, "seq": seq, "time": 1, "data": data }).to_string()
    }

    fn reply(seq: u64, time: u64, input: u64, output: u64, cache: u64) -> String {
        serde_json::json!({
            "type": "assistant/message",
            "seq": seq,
            "time": time,
            "data": {
                "turn": 1,
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
        let mut header = vec![r#"{"type":"session","version":4,"id":"s"}"#.to_owned()];
        header.extend_from_slice(lines);
        let mut text = header.join("\n");
        text.push('\n');
        text.into_bytes()
    }

    #[test]
    fn sums_turns_after_the_reported_seq_with_cache_in_input() {
        let turn = |n: u64| serde_json::json!({ "turn": n });
        let bytes = log(&[
            event(0, "turn/start", turn(1)),
            reply(1, 10_000, 5, 7, 100),
            event(2, "turn/end", turn(1)),
            event(3, "turn/start", turn(2)),
            reply(4, 20_000, 900, 200, 12_000),
            reply(5, 30_000, 100, 100, 13_000),
            event(6, "turn/end", turn(2)),
        ]);
        let activity = activity(&bytes, Some(2));
        assert!(activity.settled);
        assert_eq!(activity.last_seq, Some(6));
        let usage = activity.usage.unwrap();
        assert_eq!(usage.input_tokens, Some(1000 + 25_000));
        assert_eq!(usage.output_tokens, Some(300));
        assert_eq!(usage.cached_input_tokens, Some(25_000));
        assert_eq!(usage.total_tokens, Some(26_300));
        // 300 tokens over two 1 s decode windows.
        assert_eq!(usage.generation.unwrap().tps(), Some(150.0));
        let nothing = super::activity(&bytes, Some(6));
        assert_eq!((nothing.usage, nothing.settled), (None, false));
    }

    #[test]
    fn an_open_turn_is_not_settled() {
        let turn = |n: u64| serde_json::json!({ "turn": n });
        let bytes = log(&[
            event(0, "turn/start", turn(1)),
            event(1, "turn/end", turn(1)),
            event(2, "turn/start", turn(2)),
        ]);
        assert!(!activity(&bytes, Some(0)).settled);
        assert!(activity(&bytes, None).last_seq == Some(2));
    }

    #[test]
    fn a_command_settles_with_its_result_text() {
        let bytes = log(&[
            event(3, "command/run", serde_json::json!({ "name": "compact" })),
            event(
                4,
                "command/done",
                serde_json::json!({ "kind": "success", "text": "No compactable history yet." }),
            ),
        ]);
        let activity = activity(&bytes, Some(2));
        assert!(activity.settled);
        assert_eq!(activity.usage, None);
        assert_eq!(
            activity.command_reply().as_deref(),
            Some("No compactable history yet.")
        );
    }

    #[test]
    fn reads_concatenated_frames_and_drops_a_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.v4.jsonl.zstd");
        let turn = serde_json::json!({ "turn": 1 });
        let first = log(&[event(0, "turn/start", turn.clone())]);
        let second = format!("{}\n", event(1, "turn/end", turn));
        let mut bytes = zstd::encode_all(&first[..], 0).unwrap();
        bytes.extend(zstd::encode_all(second.as_bytes(), 0).unwrap());
        let torn = zstd::encode_all(&b"{\"type\":\"turn/start\",\"seq\":2}\n"[..], 0).unwrap();
        bytes.extend(&torn[..torn.len() / 2]);
        std::fs::write(&path, bytes).unwrap();
        let activity = activity(&read_log(&path).unwrap(), None);
        assert!(activity.settled);
        assert_eq!(activity.last_seq, Some(1));
        assert_eq!(newest_generation(dir.path()), Some(path));
    }
}
