use std::collections::HashMap;

use serde_json::Value;
use zeron_proto::TokenUsage;

/// Request snapshots for the active user turn, keyed by assistant message ID.
#[derive(Default)]
pub(super) struct TurnUsageTracker {
    reports: HashMap<String, Option<TokenUsage>>,
    incomplete: bool,
}

impl TurnUsageTracker {
    pub fn register(&mut self, message: &str) {
        self.reports.entry(message.to_owned()).or_default();
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
            };
        }
        Some(total)
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
