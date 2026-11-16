//! Token usage from the coding agents' own session logs on this device
//! (`GetTokenUsage`, Settings → Usage): every model call Claude Code, Codex
//! and Grok wrote down, whether Zeron started the session or not, added up
//! by period, agent and model and priced at public API rates
//! (`price_list`) unless the agent recorded a cost itself.
//!
//! Where the logs live:
//!   Claude Code  `{CLAUDE_CONFIG_DIR}/projects/**/*.jsonl`, one line per
//!                streamed frame: a call's frames repeat its usage, so a
//!                call counts once by message id and request id.
//!   Codex        `{CODEX_HOME}/sessions/**/*.jsonl`: `token_count` events
//!                carry the last call's usage, sometimes twice; a forked
//!                session opens with a burst of its parent's history,
//!                which isn't counted again.
//!   Grok         `{GROK_HOME}/sessions/**/updates.jsonl`: one
//!                `turn_completed` per turn, by model, with its cost.
//!
//! A file is read again only when its size or modified time changes.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Days, Local, NaiveDate, NaiveTime, TimeZone, Timelike};
use serde::Deserialize;
use serde_json::Value;
use zeron_proto::HarnessId;
use zeron_proto::token_usage::{SessionCount, TokenCounts, TokenUsage, UsageSlice};

use crate::price_list;

/// Files untouched since this long before the range are skipped unread (a
/// session file is written to as it goes, so its last write bounds it).
const QUIET_FOR: Duration = Duration::from_secs(36 * 60 * 60);
/// A forked Codex session replays its parent's calls back to back; a gap
/// this long ends the replay.
const REPLAY_GAP_MS: i64 = 1_000;
const MAX_DAYS: u32 = 366;

/// The agents' log folders to read.
#[derive(Clone, Debug, Default)]
pub struct LogDirs {
    dirs: Vec<(HarnessId, PathBuf)>,
}

impl LogDirs {
    /// The folders under each agent's home (its config dir for Claude).
    pub fn of_homes(claude: &Path, codex: &Path, grok: &Path) -> Self {
        let mut dirs = Self::default();
        dirs.add(HarnessId::ClaudeCode, claude);
        dirs.add(HarnessId::Codex, codex);
        dirs.add(HarnessId::Grok, grok);
        dirs
    }

    /// Another home of one of those agents (each read once).
    pub fn add(&mut self, harness: HarnessId, home: &Path) {
        let dir = match harness {
            HarnessId::ClaudeCode => home.join("projects"),
            HarnessId::Codex | HarnessId::Grok => home.join("sessions"),
            _ => return,
        };
        if self.dirs.iter().all(|(_, known)| *known != dir) {
            self.dirs.push((harness, dir));
        }
    }
}

/// One model call, as a log tells it.
#[derive(Clone, Debug)]
struct Call {
    at_ms: i64,
    model: Arc<str>,
    session: Arc<str>,
    tokens: TokenCounts,
    /// What the agent recorded it cost, when it did.
    recorded_usd: Option<f64>,
    /// Calls with the same id are one call written more than once.
    id: Option<u64>,
}

struct Memo {
    len: u64,
    modified: SystemTime,
    calls: Arc<[Call]>,
}

static MEMO: LazyLock<Mutex<HashMap<PathBuf, Memo>>> = LazyLock::new(Default::default);
/// One report at a time: two at once would read the same files twice.
static REPORTING: Mutex<()> = Mutex::new(());

/// The usage over the past `days` days to `now` (one: the past 24 hours, by
/// the hour; more: by local calendar day, today last).
pub fn report(dirs: &LogDirs, days: u32, now: DateTime<Local>) -> TokenUsage {
    let _one = REPORTING.lock().unwrap_or_else(PoisonError::into_inner);
    let (starts_ms, end_ms) = periods(days.clamp(1, MAX_DAYS), now);
    let since_ms = starts_ms.first().copied().unwrap_or(end_ms).max(0);
    let since = (UNIX_EPOCH + Duration::from_millis(since_ms as u64))
        .checked_sub(QUIET_FOR)
        .unwrap_or(UNIX_EPOCH);

    let mut files = Vec::new();
    for (harness, dir) in &dirs.dirs {
        find_logs(*harness, dir, since, &mut files);
    }
    // (A stable order, so which copy of a repeated call counts never varies.)
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let mut tally = Tally::new(starts_ms, end_ms);
    for (file, calls) in files.iter().zip(read_logs(&files)) {
        for call in calls.iter() {
            tally.count(file.harness, call);
        }
    }
    tally.into_usage(days <= 1)
}

/// Each period's start and where the last ends: the 24 hours to `now`, or
/// `days` local days to the end of today.
fn periods(days: u32, now: DateTime<Local>) -> (Vec<i64>, i64) {
    if days <= 1 {
        let this_hour = now
            .with_minute(0)
            .and_then(|t| t.with_second(0))
            .and_then(|t| t.with_nanosecond(0))
            .unwrap_or(now);
        let starts = (0..24i64)
            .map(|h| (this_hour - chrono::Duration::hours(23 - h)).timestamp_millis())
            .collect();
        return (starts, now.timestamp_millis());
    }
    let today = now.date_naive();
    let first = today.checked_sub_days(Days::new(u64::from(days - 1))).unwrap_or(today);
    let starts = first
        .iter_days()
        .take_while(|day| *day <= today)
        .map(midnight_ms)
        .collect();
    let end = today
        .checked_add_days(Days::new(1))
        .map_or_else(|| now.timestamp_millis(), midnight_ms);
    (starts, end)
}

/// A day's first local moment (where a clock change skips midnight, the
/// earliest that exists).
fn midnight_ms(day: NaiveDate) -> i64 {
    let at = day.and_time(NaiveTime::MIN);
    Local
        .from_local_datetime(&at)
        .earliest()
        .map_or_else(|| at.and_utc().timestamp_millis(), |t| t.timestamp_millis())
}

struct LogFile {
    harness: HarnessId,
    path: PathBuf,
    len: u64,
    modified: SystemTime,
}

fn is_log(harness: HarnessId, path: &Path) -> bool {
    match harness {
        HarnessId::Grok => path.file_name().is_some_and(|name| name == "updates.jsonl"),
        _ => path.extension().is_some_and(|ext| ext == "jsonl"),
    }
}

/// Every log under `root` written to since `since`.
fn find_logs(harness: HarnessId, root: &Path, since: SystemTime, out: &mut Vec<LogFile>) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() && is_log(harness, &path) {
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                let Ok(modified) = meta.modified() else {
                    continue;
                };
                if modified >= since {
                    out.push(LogFile { harness, path, len: meta.len(), modified });
                }
            }
        }
    }
}

/// Each file's calls, from the memo where it hasn't changed, the rest read
/// on a few threads.
fn read_logs(files: &[LogFile]) -> Vec<Arc<[Call]>> {
    let mut calls: Vec<Option<Arc<[Call]>>> = {
        let memo = MEMO.lock().unwrap_or_else(PoisonError::into_inner);
        files
            .iter()
            .map(|file| {
                memo.get(&file.path)
                    .filter(|m| m.len == file.len && m.modified == file.modified)
                    .map(|m| m.calls.clone())
            })
            .collect()
    };
    let unread: Vec<usize> = (0..files.len()).filter(|&i| calls[i].is_none()).collect();
    if !unread.is_empty() {
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
        let per_thread = unread.len().div_ceil(threads);
        let read: Vec<(usize, Arc<[Call]>)> = std::thread::scope(|scope| {
            let workers: Vec<_> = unread
                .chunks(per_thread)
                .map(|share| {
                    scope.spawn(move || {
                        share
                            .iter()
                            .filter_map(|&i| Some((i, Arc::<[Call]>::from(read_log(&files[i])?))))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().flat_map(|w| w.join().unwrap_or_default()).collect()
        });
        let mut memo = MEMO.lock().unwrap_or_else(PoisonError::into_inner);
        for (i, read) in read {
            let file = &files[i];
            memo.insert(
                file.path.clone(),
                Memo { len: file.len, modified: file.modified, calls: read.clone() },
            );
            calls[i] = Some(read);
        }
    }
    calls.into_iter().map(|c| c.unwrap_or_else(|| Arc::<[Call]>::from(Vec::new()))).collect()
}

fn read_log(file: &LogFile) -> Option<Vec<Call>> {
    let mut reader = BufReader::with_capacity(64 * 1024, std::fs::File::open(&file.path).ok()?);
    let mut log = LogReader::new(file.harness);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        if reader.read_until(b'\n', &mut bytes).ok()? == 0 {
            break;
        }
        if let Ok(line) = std::str::from_utf8(&bytes) {
            log.line(line.trim_end());
        }
    }
    Some(log.calls)
}

/// A whole, positive count (anything else is none).
fn tokens_at(value: &Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .map_or(0, |n| n as u64)
}

fn rfc3339_ms(text: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(text).ok().map(|t| t.timestamp_millis())
}

fn id_of(parts: &[&str]) -> u64 {
    let mut h = DefaultHasher::new();
    parts.hash(&mut h);
    h.finish()
}

#[derive(Deserialize)]
struct ClaudeEntry {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    #[serde(rename = "costUSD")]
    cost_usd: Option<f64>,
    message: Option<ClaudeMessage>,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Value>,
}

/// Where a Codex log is: its session, the model its turns use, the last
/// usage it reported (repeats are the same call), and whether it is still
/// replaying a parent's history.
#[derive(Default)]
struct CodexState {
    session: Option<Arc<str>>,
    model: Option<Arc<str>>,
    last_usage: Option<Value>,
    started: bool,
    replay_until_gap_from: Option<i64>,
}

struct LogReader {
    harness: HarnessId,
    calls: Vec<Call>,
    names: HashMap<String, Arc<str>>,
    codex: CodexState,
}

impl LogReader {
    fn new(harness: HarnessId) -> Self {
        Self { harness, calls: Vec::new(), names: HashMap::new(), codex: CodexState::default() }
    }

    /// One shared copy of a model or session name per file.
    fn name(&mut self, text: &str) -> Arc<str> {
        if let Some(known) = self.names.get(text) {
            return known.clone();
        }
        let shared: Arc<str> = Arc::from(text);
        self.names.insert(text.to_owned(), shared.clone());
        shared
    }

    fn record(&mut self, call: Call) {
        if !call.tokens.is_empty() {
            self.calls.push(call);
        }
    }

    fn line(&mut self, line: &str) {
        match self.harness {
            HarnessId::ClaudeCode => self.claude(line),
            HarnessId::Codex => self.codex(line),
            HarnessId::Grok => self.grok(line),
            _ => {}
        }
    }

    fn claude(&mut self, line: &str) {
        // (Most lines are something else: skip them before parsing.)
        if !(line.contains("\"usage\"") && line.contains("\"assistant\"")) {
            return;
        }
        let Ok(entry) = serde_json::from_str::<ClaudeEntry>(line) else {
            return;
        };
        if entry.kind.as_deref() != Some("assistant") {
            return;
        }
        let Some(message) = entry.message else {
            return;
        };
        let Some(at_ms) = entry.timestamp.as_deref().and_then(rfc3339_ms) else {
            return;
        };
        let (Some(usage), Some(model)) = (
            message.usage.filter(Value::is_object),
            message.model.filter(|m| !m.is_empty()),
        ) else {
            return;
        };
        let id = (message.id.is_some() || entry.request_id.is_some()).then(|| {
            id_of(&[message.id.as_deref().unwrap_or(""), entry.request_id.as_deref().unwrap_or("")])
        });
        let session = self.name(entry.session_id.as_deref().unwrap_or(""));
        let main_model = self.name(&model);
        self.record(Call {
            at_ms,
            model: main_model,
            session: session.clone(),
            tokens: claude_tokens(&usage),
            recorded_usd: entry.cost_usd.filter(|usd| usd.is_finite()),
            id,
        });
        // Calls made inside this one to another model (an advisor's), each
        // its own; the "message" step is the call above.
        let steps = usage.get("iterations").and_then(Value::as_array);
        for (n, step) in steps.into_iter().flatten().enumerate() {
            if step.get("type").and_then(Value::as_str) == Some("message") {
                continue;
            }
            let Some(model) = step.get("model").and_then(Value::as_str) else {
                continue;
            };
            let model = self.name(model);
            self.record(Call {
                at_ms,
                model,
                session: session.clone(),
                tokens: claude_tokens(step),
                recorded_usd: None,
                id: id.map(|id| id_of(&[&id.to_string(), "step", &n.to_string()])),
            });
        }
    }

    fn codex(&mut self, line: &str) {
        if !["token_count", "turn_context", "session_meta"].iter().any(|k| line.contains(k)) {
            return;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let Some(payload) = entry.get("payload").filter(|p| p.is_object()) else {
            return;
        };
        let at_ms = entry.get("timestamp").and_then(Value::as_str).and_then(rfc3339_ms);
        match entry.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                // (Only the first: a fork's file quotes its parent's after.)
                if std::mem::replace(&mut self.codex.started, true) {
                    return;
                }
                let id = payload
                    .get("id")
                    .or_else(|| payload.get("session_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                self.codex.session = Some(self.name(id));
                let forked = payload.get("forked_from_id").is_some_and(Value::is_string)
                    || payload
                        .pointer("/source/subagent/thread_spawn/parent_thread_id")
                        .is_some_and(Value::is_string);
                if forked {
                    self.codex.replay_until_gap_from = at_ms;
                }
            }
            Some("turn_context") => {
                if let Some(model) = payload.get("model").and_then(Value::as_str) {
                    self.codex.model = Some(self.name(model));
                }
            }
            _ if payload.get("type").and_then(Value::as_str) == Some("token_count") => {
                self.codex_call(payload, at_ms);
            }
            _ => {}
        }
    }

    fn codex_call(&mut self, payload: &Value, at_ms: Option<i64>) {
        let Some(last) = payload.pointer("/info/last_token_usage").filter(|u| u.is_object()) else {
            return;
        };
        let (Some(at_ms), Some(model)) = (at_ms, self.codex.model.clone()) else {
            return;
        };
        if self.codex.last_usage.as_ref() == Some(last) {
            return;
        }
        self.codex.last_usage = Some(last.clone());
        if let Some(previous) = self.codex.replay_until_gap_from {
            if at_ms - previous < REPLAY_GAP_MS {
                self.codex.replay_until_gap_from = Some(at_ms);
                return;
            }
            self.codex.replay_until_gap_from = None;
        }
        let cache_read = tokens_at(last, "cached_input_tokens");
        let cache_write = tokens_at(last, "cache_write_input_tokens");
        let tokens = TokenCounts {
            fresh_input: tokens_at(last, "input_tokens").saturating_sub(cache_read + cache_write),
            cache_read,
            cache_write,
            output: tokens_at(last, "output_tokens"),
        };
        let session = match self.codex.session.clone() {
            Some(session) => session,
            None => self.name(""),
        };
        self.record(Call { at_ms, model, session, tokens, recorded_usd: None, id: None });
    }

    fn grok(&mut self, line: &str) {
        if !line.contains("turn_completed") {
            return;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let Some(params) = entry.get("params") else {
            return;
        };
        let Some(update) = params.get("update") else {
            return;
        };
        if update.get("sessionUpdate").and_then(Value::as_str) != Some("turn_completed") {
            return;
        }
        let Some(usage) = update.get("usage").filter(|u| u.is_object()) else {
            return;
        };
        // The agent's own clock in ms, else the line's (seconds or ms).
        let at = params.pointer("/_meta/agentTimestampMs").and_then(Value::as_f64).or_else(|| {
            let t = entry.get("timestamp")?.as_f64()?;
            Some(if t < 1e12 { t * 1000.0 } else { t })
        });
        let Some(at_ms) = at.filter(|t| t.is_finite()).map(|t| t as i64) else {
            return;
        };
        let session_id = params.get("sessionId").and_then(Value::as_str).unwrap_or("").to_owned();
        let prompt = update.get("prompt_id").and_then(Value::as_str).map(str::to_owned);
        let session = self.name(&session_id);
        let id = |model: &str| prompt.as_deref().map(|p| id_of(&[&session_id, p, model]));

        let turn_usd = grok_usd(usage);
        let per_model: Vec<(String, TokenCounts, Option<f64>)> = usage
            .get("modelUsage")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter(|(model, _)| !model.is_empty())
            .map(|(model, u)| (model.clone(), grok_tokens(u), grok_usd(u)))
            .filter(|(_, tokens, _)| !tokens.is_empty())
            .collect();
        if per_model.is_empty() {
            let model = self.name("grok");
            self.record(Call {
                at_ms,
                model,
                session,
                tokens: grok_tokens(usage),
                recorded_usd: turn_usd,
                id: id("grok"),
            });
            return;
        }
        // The turn's cost not already put on a model is shared among the
        // models without one, by their tokens.
        let put: f64 = per_model.iter().filter_map(|(_, _, usd)| *usd).sum();
        let left = turn_usd.map(|usd| (usd - put).max(0.0));
        let unpriced_tokens: u64 = per_model
            .iter()
            .filter(|(_, _, usd)| usd.is_none())
            .map(|(_, tokens, _)| tokens.total())
            .sum();
        for (model, tokens, usd) in per_model {
            let usd = usd.or_else(|| {
                left.filter(|_| unpriced_tokens > 0)
                    .map(|left| left * tokens.total() as f64 / unpriced_tokens as f64)
            });
            let call_id = id(&model);
            let model = self.name(&model);
            self.record(Call { at_ms, model, session: session.clone(), tokens, recorded_usd: usd, id: call_id });
        }
    }
}

fn claude_tokens(usage: &Value) -> TokenCounts {
    TokenCounts {
        fresh_input: tokens_at(usage, "input_tokens"),
        cache_read: tokens_at(usage, "cache_read_input_tokens"),
        cache_write: tokens_at(usage, "cache_creation_input_tokens"),
        output: tokens_at(usage, "output_tokens"),
    }
}

fn grok_tokens(usage: &Value) -> TokenCounts {
    let cache_read = tokens_at(usage, "cachedReadTokens");
    let cache_write = tokens_at(usage, "cacheCreationTokens");
    TokenCounts {
        fresh_input: tokens_at(usage, "inputTokens").saturating_sub(cache_read + cache_write),
        cache_read,
        cache_write,
        output: tokens_at(usage, "outputTokens"),
    }
}

/// Grok's cost, kept in ten-billionths of a dollar.
fn grok_usd(usage: &Value) -> Option<f64> {
    let ticks = usage.get("costUsdTicks")?.as_f64()?;
    (ticks.is_finite() && ticks >= 0.0).then(|| ticks / 1e10)
}

/// The calls added up into slices, each call once.
struct Tally {
    starts_ms: Vec<i64>,
    end_ms: i64,
    seen: HashSet<u64>,
    rates: HashMap<(HarnessId, Arc<str>), Option<price_list::Rates>>,
    slices: HashMap<(u32, HarnessId, Arc<str>), UsageSlice>,
    sessions: HashMap<HarnessId, HashSet<Arc<str>>>,
}

impl Tally {
    fn new(starts_ms: Vec<i64>, end_ms: i64) -> Self {
        Self {
            starts_ms,
            end_ms,
            seen: HashSet::new(),
            rates: HashMap::new(),
            slices: HashMap::new(),
            sessions: HashMap::new(),
        }
    }

    fn period_of(&self, at_ms: i64) -> Option<u32> {
        if at_ms >= self.end_ms {
            return None;
        }
        let after = self.starts_ms.partition_point(|&start| start <= at_ms);
        after.checked_sub(1).map(|p| p as u32)
    }

    fn count(&mut self, harness: HarnessId, call: &Call) {
        let Some(period) = self.period_of(call.at_ms) else {
            return;
        };
        if call.id.is_some_and(|id| !self.seen.insert(id)) {
            return;
        }
        let rates = *self
            .rates
            .entry((harness, call.model.clone()))
            .or_insert_with(|| price_list::rates(harness, &call.model));
        let slice = self.slices.entry((period, harness, call.model.clone())).or_insert_with(|| UsageSlice {
            period,
            harness,
            model: call.model.to_string(),
            tokens: TokenCounts::default(),
            cost_usd: 0.0,
            saved_usd: 0.0,
            calls: 0,
            unpriced_calls: 0,
        });
        let t = call.tokens;
        slice.tokens.absorb(&t);
        slice.calls += 1;
        match (call.recorded_usd, rates) {
            (Some(usd), _) => slice.cost_usd += usd,
            (None, Some(r)) => slice.cost_usd += r.dollars(t.fresh_input, t.cache_read, t.cache_write, t.output),
            (None, None) => slice.unpriced_calls += 1,
        }
        if let Some(r) = rates {
            slice.saved_usd += r.cache_saving(t.cache_read);
        }
        if !call.session.is_empty() {
            self.sessions.entry(harness).or_default().insert(call.session.clone());
        }
    }

    fn into_usage(self, hourly: bool) -> TokenUsage {
        let mut slices: Vec<UsageSlice> = self.slices.into_values().collect();
        slices.sort_by(|a, b| {
            (a.period, a.harness.as_str(), &a.model).cmp(&(b.period, b.harness.as_str(), &b.model))
        });
        let mut sessions: Vec<SessionCount> = self
            .sessions
            .into_iter()
            .map(|(harness, ids)| SessionCount { harness, sessions: ids.len() as u64 })
            .collect();
        sessions.sort_by_key(|s| s.harness.as_str());
        TokenUsage {
            hourly,
            starts_ms: self.starts_ms,
            end_ms: self.end_ms,
            slices,
            sessions,
            prices_loaded: price_list::loaded(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    fn read(harness: HarnessId, lines: &[String]) -> Vec<Call> {
        let mut log = LogReader::new(harness);
        for line in lines {
            log.line(line);
        }
        log.calls
    }

    fn tally(harness: HarnessId, calls: &[Call]) -> TokenUsage {
        let mut tally = Tally::new(vec![0], i64::MAX);
        for call in calls {
            tally.count(harness, call);
        }
        tally.into_usage(false)
    }

    #[test]
    fn a_claude_call_streamed_in_several_frames_counts_once() {
        let frame = |block: &str| {
            format!(
                r#"{{"type":"assistant","timestamp":"2026-11-01T09:00:00Z","sessionId":"a","requestId":"r9","message":{{"id":"m9","model":"claude-x","content":[{{"type":"{block}"}}],"usage":{{"input_tokens":4,"cache_read_input_tokens":60,"cache_creation_input_tokens":6,"output_tokens":30,"iterations":[{{"type":"message"}},{{"type":"advisor_message","model":"claude-y","input_tokens":200,"output_tokens":10}}]}}}}}}"#
            )
        };
        let calls = read(HarnessId::ClaudeCode, &[frame("thinking"), frame("tool_use"), frame("text")]);
        let usage = tally(HarnessId::ClaudeCode, &calls);
        let x = usage.slices.iter().find(|s| s.model == "claude-x").unwrap();
        assert_eq!(x.calls, 1);
        assert_eq!(x.tokens, TokenCounts { fresh_input: 4, cache_read: 60, cache_write: 6, output: 30 });
        let y = usage.slices.iter().find(|s| s.model == "claude-y").unwrap();
        assert_eq!((y.calls, y.tokens.total()), (1, 210));
        assert_eq!(usage.sessions, [SessionCount { harness: HarnessId::ClaudeCode, sessions: 1 }]);
    }

    #[test]
    fn codex_skips_repeats_and_a_forks_replayed_history() {
        let count = |at: &str, input: u64| {
            format!(
                r#"{{"timestamp":"{at}","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":{input},"cached_input_tokens":20,"output_tokens":3}}}}}}}}"#
            )
        };
        let lines = [
            r#"{"timestamp":"2026-11-01T10:00:00.000Z","type":"session_meta","payload":{"id":"kid","forked_from_id":"mum"}}"#.to_owned(),
            r#"{"timestamp":"2026-11-01T10:00:00.050Z","type":"turn_context","payload":{"model":"gpt-x"}}"#.to_owned(),
            count("2026-11-01T10:00:00.100Z", 50),
            count("2026-11-01T10:00:00.600Z", 60),
            count("2026-11-01T10:00:04.000Z", 70),
            count("2026-11-01T10:00:05.000Z", 70),
            count("2026-11-01T10:00:08.000Z", 80),
        ];
        let calls = read(HarnessId::Codex, &lines);
        let fresh: Vec<u64> = calls.iter().map(|c| c.tokens.fresh_input).collect();
        assert_eq!(fresh, [50, 60]);
        assert_eq!(&*calls[0].session, "kid");
        assert_eq!(&*calls[0].model, "gpt-x");
    }

    #[test]
    fn grok_shares_a_turns_unplaced_cost_by_tokens() {
        let line = r#"{"timestamp":1793000000,"params":{"sessionId":"g","update":{"sessionUpdate":"turn_completed","prompt_id":"p","usage":{"inputTokens":400,"outputTokens":0,"costUsdTicks":40000000000,"modelUsage":{"big":{"inputTokens":300,"outputTokens":0},"small":{"inputTokens":100,"outputTokens":0}}}}}}"#;
        let calls = read(HarnessId::Grok, &[line.to_owned()]);
        let usd: Vec<(&str, Option<f64>)> = calls.iter().map(|c| (&*c.model, c.recorded_usd)).collect();
        assert_eq!(usd, [("big", Some(3.0)), ("small", Some(1.0))]);
        assert_eq!(calls[0].at_ms, 1_793_000_000_000);
    }

    #[test]
    fn days_run_midnight_to_midnight_and_one_day_is_24_hours() {
        let now = Local.with_ymd_and_hms(2026, 11, 16, 14, 20, 0).unwrap();
        let (days, end) = periods(7, now);
        assert_eq!(days.len(), 7);
        let first = Local.timestamp_millis_opt(days[0]).unwrap();
        assert_eq!((first.day(), first.hour()), (10, 0));
        assert_eq!(Local.timestamp_millis_opt(end).unwrap().day(), 17);
        let (hours, end) = periods(1, now);
        assert_eq!(hours.len(), 24);
        assert_eq!(end, now.timestamp_millis());
        assert_eq!(Local.timestamp_millis_opt(hours[23]).unwrap().hour(), 14);
    }

    #[test]
    fn calls_outside_the_range_are_left_out() {
        let tally = Tally::new(vec![100, 200], 300);
        assert_eq!(tally.period_of(99), None);
        assert_eq!(tally.period_of(100), Some(0));
        assert_eq!(tally.period_of(250), Some(1));
        assert_eq!(tally.period_of(300), None);
    }
}
