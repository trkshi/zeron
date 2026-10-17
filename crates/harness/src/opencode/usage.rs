use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;
use zeron_proto::{GenerationUsage, TokenUsage};

use crate::generation;

/// Request snapshots for the active user turn, keyed by assistant message ID.
#[derive(Default)]
pub(super) struct TurnUsageTracker {
    reports: HashMap<String, Option<TokenUsage>>,
    windows: HashMap<String, RequestWindow>,
    incomplete: bool,
}

/// One request's streaming window, from OpenCode's own server timestamps
/// (epoch ms) rather than SSE arrival, which can batch. It runs from the
/// first text/reasoning start to the last text/reasoning end or tool call.
/// Tools execute after their call timestamp, so tool runs stay outside it,
/// as does the wait before the first token.
#[derive(Default, Clone, Copy)]
struct RequestWindow {
    start: Option<u64>,
    end: Option<u64>,
    chunks: u32,
    reasoning: bool,
}

impl RequestWindow {
    fn include(&mut self, at: Option<u64>) {
        let Some(at) = at else { return };
        self.start = Some(self.start.map_or(at, |start| start.min(at)));
        self.end_at(Some(at));
    }

    fn end_at(&mut self, at: Option<u64>) {
        let Some(at) = at else { return };
        self.end = Some(self.end.map_or(at, |end| end.max(at)));
    }

    /// Streamed output over this window, when it qualifies. Without a
    /// reasoning part the hidden thinking time is unmeasured, so its tokens
    /// are left out too.
    fn generation(self, usage: &TokenUsage) -> Option<(u64, Duration)> {
        let elapsed = Duration::from_millis(self.end?.checked_sub(self.start?)?);
        if !generation::qualifies(self.chunks, elapsed) {
            return None;
        }
        let output = usage.output_tokens?;
        let output = if self.reasoning {
            output
        } else {
            output.checked_sub(usage.reasoning_output_tokens.unwrap_or(0))?
        };
        Some((output, elapsed))
    }
}

impl TurnUsageTracker {
    pub fn register(&mut self, message: &str) {
        self.reports.entry(message.to_owned()).or_default();
        self.windows.entry(message.to_owned()).or_default();
    }

    /// Fold a main-session part snapshot's server timestamps into its
    /// request's window. Parts of unregistered messages are ignored.
    pub fn observe_part(&mut self, part: &Value) {
        let Some(window) = part
            .get("messageID")
            .and_then(Value::as_str)
            .and_then(|id| self.windows.get_mut(id))
        else {
            return;
        };
        let at = |pointer| part.pointer(pointer).and_then(Value::as_u64);
        match part.get("type").and_then(Value::as_str) {
            Some(kind @ ("text" | "reasoning")) => {
                window.reasoning |= kind == "reasoning";
                window.include(at("/time/start"));
                window.include(at("/time/end"));
            }
            // A call's start marks the end of its streamed input.
            Some("tool") => window.end_at(at("/state/time/start")),
            _ => {}
        }
    }

    /// Count one streamed text/reasoning delta for a request.
    pub fn chunk(&mut self, message: &str) {
        if let Some(window) = self.windows.get_mut(message) {
            window.chunks = window.chunks.saturating_add(1);
        }
    }

    pub fn invalidate(&mut self) {
        self.incomplete = true;
    }

    pub fn observe(&mut self, info: &Value) {
        let Some(report) = info
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| self.reports.get_mut(id))
        else {
            return;
        };
        // Initial assistant placeholders contain zero counters. A finish
        // reason or completion timestamp makes the request's usage final.
        if info
            .get("finish")
            .and_then(Value::as_str)
            .filter(|finish| !finish.is_empty())
            .is_none()
            && info
                .pointer("/time/completed")
                .and_then(Value::as_u64)
                .is_none()
        {
            return;
        }
        *report = request_usage(info);
    }

    pub fn total(&self) -> Option<TokenUsage> {
        if self.reports.values().any(Option::is_none) {
            return None;
        }
        self.reported()
    }

    pub fn has_reports(&self) -> bool {
        self.reports.values().any(Option::is_some)
    }

    /// Completed requests are available for live TPS even while the next
    /// request is still running. Final TPS requires all requests above.
    pub fn reported(&self) -> Option<TokenUsage> {
        if self.incomplete {
            return None;
        }
        let mut reports = self.reports.values().flatten();
        let mut total = *reports.next()?;
        let add = |a: Option<u64>, b: Option<u64>| a?.checked_add(b?);
        for report in reports {
            let usage = *report;
            total = TokenUsage {
                input_tokens: add(total.input_tokens, usage.input_tokens),
                output_tokens: add(total.output_tokens, usage.output_tokens),
                total_tokens: add(total.total_tokens, usage.total_tokens),
                cached_input_tokens: add(total.cached_input_tokens, usage.cached_input_tokens),
                cache_write_input_tokens: add(
                    total.cache_write_input_tokens,
                    usage.cache_write_input_tokens,
                ),
                reasoning_output_tokens: add(
                    total.reasoning_output_tokens,
                    usage.reasoning_output_tokens,
                ),
                cost_usd: total
                    .cost_usd
                    .zip(usage.cost_usd)
                    .map(|(a, b)| a + b)
                    .filter(|cost| cost.is_finite()),
                generation: None,
            };
        }
        total.generation = self.generation();
        Some(total)
    }

    /// Generation over every completed request whose window qualifies.
    fn generation(&self) -> Option<GenerationUsage> {
        self.reports
            .iter()
            .filter_map(|(id, usage)| self.windows.get(id)?.generation(usage.as_ref()?))
            .fold(None, |total, (output, elapsed)| {
                generation::accumulate(total, output, elapsed)
            })
    }
}

fn request_usage(info: &Value) -> Option<TokenUsage> {
    let tokens = info.get("tokens")?.as_object()?;
    let count = |key: &str| tokens.get(key).and_then(Value::as_u64);
    let cached_input_tokens = tokens
        .get("cache")
        .and_then(|cache| cache.get("read"))
        .and_then(Value::as_u64);
    let cache_write_input_tokens = tokens
        .get("cache")
        .and_then(|cache| cache.get("write"))
        .and_then(Value::as_u64);
    let reasoning_output_tokens = count("reasoning");
    // OpenCode splits cache from input and reasoning from output. Zeron's
    // normalized counts include those subsets, never added again to totals.
    let input_tokens = count("input").and_then(|input| {
        input
            .checked_add(cached_input_tokens?)?
            .checked_add(cache_write_input_tokens?)
    });
    let output_tokens =
        count("output").and_then(|output| output.checked_add(reasoning_output_tokens?));
    Some(TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens: count("total").or_else(|| input_tokens?.checked_add(output_tokens?)),
        cached_input_tokens,
        cache_write_input_tokens,
        reasoning_output_tokens,
        cost_usd: info
            .get("cost")
            .and_then(Value::as_f64)
            .filter(|cost| cost.is_finite() && *cost >= 0.0),
        generation: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn report(id: &str) -> Value {
        json!({
            "id": id,
            "finish": "stop",
            "cost": 0.01,
            "tokens": {
                "input": 10, "output": 20, "reasoning": 5,
                "cache": {"read": 30, "write": 40}
            }
        })
    }

    #[test]
    fn request_counts_include_cache_and_reasoning_once() {
        let mut info = report("first");
        info["tokens"]["total"] = json!(105);
        let usage = request_usage(&info).unwrap();
        assert_eq!(usage.input_tokens, Some(80));
        assert_eq!(usage.output_tokens, Some(25));
        assert_eq!(usage.total(), Some(105));
        assert_eq!(usage.cached_input_tokens, Some(30));
        assert_eq!(usage.cache_write_input_tokens, Some(40));
        assert_eq!(usage.reasoning_output_tokens, Some(5));
        assert_eq!(usage.cost_usd, Some(0.01));
        assert_eq!(usage.average_tps(Some(1000)), Some(25.0));
        assert_eq!(usage.generation, None);
    }

    #[test]
    fn repeated_snapshots_replace_instead_of_accumulating() {
        let mut tracker = TurnUsageTracker::default();
        tracker.register("first");
        tracker.observe(&report("first"));
        tracker.observe(&report("first"));
        assert_eq!(tracker.total().unwrap().output_tokens, Some(25));
        let mut corrected = report("first");
        corrected["tokens"]["output"] = json!(30);
        tracker.observe(&corrected);
        assert_eq!(tracker.total().unwrap().output_tokens, Some(35));
    }

    #[test]
    fn sums_every_request_but_does_not_guess_an_unfinished_one() {
        let mut tracker = TurnUsageTracker::default();
        tracker.register("first");
        tracker.observe(&report("first"));
        tracker.register("second");
        let mut placeholder = report("second");
        placeholder.as_object_mut().unwrap().remove("finish");
        tracker.observe(&placeholder);
        assert_eq!(tracker.total(), None);
        assert_eq!(tracker.reported().unwrap().output_tokens, Some(25));
        tracker.observe(&report("second"));
        let total = tracker.total().unwrap();
        assert_eq!(total.input_tokens, Some(160));
        assert_eq!(total.output_tokens, Some(50));
        assert_eq!(total.total(), Some(210));
        assert_eq!(total.cached_input_tokens, Some(60));
        assert_eq!(total.cache_write_input_tokens, Some(80));
        assert_eq!(total.reasoning_output_tokens, Some(10));
        assert_eq!(total.cost_usd, Some(0.02));
        assert_eq!(total.generation, None);
    }

    #[test]
    fn reports_from_another_turn_are_not_registered_implicitly() {
        let mut tracker = TurnUsageTracker::default();
        tracker.register("current");
        tracker.observe(&report("current"));
        tracker.observe(&report("retired"));
        assert_eq!(tracker.total().unwrap().output_tokens, Some(25));
    }

    #[test]
    fn zero_is_reported_only_after_the_request_finishes() {
        let mut tracker = TurnUsageTracker::default();
        tracker.register("first");
        let mut info = json!({"id": "first", "cost": 0, "tokens": {
            "input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}
        }});
        tracker.observe(&info);
        assert_eq!(tracker.total(), None);
        info["time"] = json!({"completed": 1});
        tracker.observe(&info);
        let total = tracker.total().unwrap();
        assert_eq!(total.output_tokens, Some(0));
        assert_eq!(total.average_tps(Some(1000)), Some(0.0));
    }

    #[test]
    fn malformed_counts_and_cost_are_not_guessed() {
        let mut info = report("first");
        info["tokens"]["output"] = json!("unknown");
        info["cost"] = json!(-1);
        let usage = request_usage(&info).unwrap();
        assert_eq!(usage.output_tokens, None);
        assert_eq!(usage.total_tokens, None);
        assert_eq!(usage.cost_usd, None);
        assert_eq!(usage.average_tps(Some(1000)), None);
        assert_eq!(request_usage(&json!({"tokens": "unknown"})), None);
    }

    #[test]
    fn unavailable_cache_does_not_hide_reported_output() {
        let mut info = report("first");
        info["tokens"].as_object_mut().unwrap().remove("cache");
        let usage = request_usage(&info).unwrap();
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.cached_input_tokens, None);
        assert_eq!(usage.output_tokens, Some(25));
        assert_eq!(usage.reasoning_output_tokens, Some(5));
    }

    fn part(message: &str, kind: &str, time: Value) -> Value {
        json!({"messageID": message, "type": kind, "time": time})
    }

    fn stream(tracker: &mut TurnUsageTracker, message: &str, chunks: u32) {
        for _ in 0..chunks {
            tracker.chunk(message);
        }
    }

    #[test]
    fn generation_spans_server_part_times_and_ends_at_the_tool_call() {
        let mut tracker = TurnUsageTracker::default();
        tracker.register("first");
        tracker.observe_part(&part("first", "reasoning", json!({"start": 1_000, "end": 1_200})));
        tracker.observe_part(&part("first", "text", json!({"start": 1_300, "end": 1_800})));
        tracker.observe_part(&json!({
            "messageID": "first", "type": "tool",
            "state": {"status": "completed", "time": {"start": 2_000, "end": 9_000}}
        }));
        stream(&mut tracker, "first", 5);
        assert_eq!(tracker.reported(), None);
        tracker.observe(&report("first"));
        let generation = tracker.total().unwrap().generation.unwrap();
        // Streamed reasoning counts toward both tokens and time; the tool's
        // own run (2s to 9s) does not.
        assert_eq!(generation.output_tokens, 25);
        assert_eq!(generation.elapsed_ms, 1_000);
        assert_eq!(generation.tps(), Some(25.0));
    }

    #[test]
    fn hidden_reasoning_is_left_out_and_requests_add_up() {
        let mut tracker = TurnUsageTracker::default();
        tracker.register("first");
        tracker.register("second");
        tracker.observe_part(&part("first", "text", json!({"start": 1_000, "end": 2_000})));
        tracker.observe_part(&part("second", "text", json!({"start": 5_000})));
        tracker.observe_part(&part("second", "text", json!({"end": 6_000})));
        stream(&mut tracker, "first", 3);
        stream(&mut tracker, "second", 3);
        tracker.observe(&report("first"));
        tracker.observe(&report("second"));
        let generation = tracker.total().unwrap().generation.unwrap();
        assert_eq!(generation.output_tokens, 40);
        assert_eq!(generation.elapsed_ms, 2_000);
    }

    #[test]
    fn bursts_short_windows_and_unfinished_requests_are_not_measured() {
        let mut tracker = TurnUsageTracker::default();
        for message in ["burst", "short", "running"] {
            tracker.register(message);
        }
        tracker.observe_part(&part("burst", "text", json!({"start": 1_000, "end": 3_000})));
        stream(&mut tracker, "burst", 2);
        tracker.observe_part(&part("short", "text", json!({"start": 1_000, "end": 1_100})));
        stream(&mut tracker, "short", 10);
        tracker.observe_part(&part("running", "text", json!({"start": 1_000, "end": 3_000})));
        stream(&mut tracker, "running", 10);
        tracker.observe(&report("burst"));
        tracker.observe(&report("short"));
        assert_eq!(tracker.reported().unwrap().generation, None);
        // Parts of messages outside the turn are ignored.
        tracker.observe_part(&part("retired", "text", json!({"start": 0, "end": 9_000})));
        tracker.chunk("retired");
        assert_eq!(tracker.reported().unwrap().generation, None);
    }

    #[test]
    fn a_request_without_a_correlatable_id_invalidates_full_turn_counts() {
        let mut tracker = TurnUsageTracker::default();
        tracker.register("first");
        tracker.observe(&report("first"));
        tracker.invalidate();
        assert_eq!(tracker.total(), None);
        assert_eq!(tracker.reported(), None);
    }
}
