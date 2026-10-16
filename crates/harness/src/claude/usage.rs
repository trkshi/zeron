use std::collections::{HashMap, HashSet};
use std::time::Instant;

use zeron_proto::{GenerationUsage, TokenUsage};

use super::wire::{MessageBody, UsageBody};
use crate::generation::{self, StreamWindow};

pub(super) const MAX_TRACKED_REQUESTS: usize = 4096;
const MAX_REQUEST_ID_BYTES: usize = 256;

/// Claude repeats one API message across content blocks. Keep request IDs
/// separate from Zeron's display IDs so repeated usage never adds twice.
#[derive(Default)]
pub(super) struct TurnUsageTracker {
    requests: HashMap<String, UsageBody>,
    retired: HashSet<String>,
    current: Option<String>,
    incomplete: bool,
    exhausted: bool,
    /// The streaming request's window, from `message_start` (thinking
    /// included) to `message_stop`.
    window: Option<StreamWindow>,
    generation: Option<GenerationUsage>,
}

impl TurnUsageTracker {
    pub fn reset(&mut self) {
        self.retired.extend(self.requests.drain().map(|(id, _)| id));
        self.current = None;
        self.incomplete = self.exhausted;
        self.window = None;
        self.generation = None;
    }

    /// The turn's measured requests, kept with the final result's counts.
    pub fn generation(&self) -> Option<GenerationUsage> {
        self.generation.filter(|_| !self.incomplete)
    }

    pub fn has_reports(&self) -> bool {
        !self.requests.is_empty() || self.exhausted
    }

    pub fn start(&mut self, message: Option<&MessageBody>, now: Instant) -> Option<TokenUsage> {
        let usage = message.and_then(|message| self.message(message));
        self.current = message
            .and_then(|message| message.id.as_ref())
            .filter(|id| self.requests.contains_key(*id))
            .cloned();
        self.window = self.current.as_ref().map(|_| StreamWindow::open(now));
        usage
    }

    /// One streamed content delta of the current request.
    pub fn chunk(&mut self) {
        if let Some(window) = &mut self.window {
            window.chunk();
        }
    }

    /// Close the current request. Its final output count arrived with the
    /// preceding `message_delta`.
    pub fn stop(&mut self, now: Instant) -> Option<TokenUsage> {
        let window = self.window.take();
        let current = self.current.take()?;
        let elapsed = window?.measured(now)?;
        let output = self.requests.get(&current)?.output_tokens?;
        let before = self.generation;
        self.generation = generation::accumulate(before, output, elapsed);
        (self.generation != before).then(|| self.snapshot()).flatten()
    }

    pub fn message(&mut self, message: &MessageBody) -> Option<TokenUsage> {
        if self.exhausted {
            return None;
        }
        let Some(id) = message.id.as_ref().filter(|id| !id.is_empty()) else {
            if message.usage.is_some() && self.has_reports() {
                self.incomplete = true;
                return Some(TokenUsage::default());
            }
            return None;
        };
        if self.retired.contains(id) {
            return None;
        }
        if id.len() > MAX_REQUEST_ID_BYTES
            || (!self.requests.contains_key(id)
                && self.requests.len() + self.retired.len() >= MAX_TRACKED_REQUESTS)
        {
            // Evicting IDs could admit an old echo as new usage. Until this
            // warm child ends, trust only Claude's authoritative final report.
            self.exhausted = true;
            self.incomplete = true;
            self.current = None;
            self.window = None;
            self.requests = HashMap::new();
            self.retired = HashSet::new();
            return Some(TokenUsage::default());
        }
        let report = self.requests.entry(id.clone()).or_default();
        if let Some(usage) = message
            .usage
            .as_ref()
            .and_then(|value| serde_json::from_value::<UsageBody>(value.clone()).ok())
        {
            merge(report, usage);
        }
        self.snapshot()
    }

    pub fn delta(&mut self, usage: &UsageBody) -> Option<TokenUsage> {
        let report = self.requests.get_mut(self.current.as_ref()?)?;
        // message_delta contains cumulative counters for this request, not
        // token increments, and normally omits the initial input/cache data.
        merge(report, usage.clone());
        self.snapshot()
    }

    fn snapshot(&self) -> Option<TokenUsage> {
        if self.incomplete {
            return Some(TokenUsage::default());
        }
        let mut reports = self.requests.values().map(UsageBody::token_usage);
        let mut total = reports.next()?;
        let add = |a: Option<u64>, b: Option<u64>| a?.checked_add(b?);
        for usage in reports {
            total = TokenUsage {
                input_tokens: add(total.input_tokens, usage.input_tokens),
                output_tokens: add(total.output_tokens, usage.output_tokens),
                cached_input_tokens: add(total.cached_input_tokens, usage.cached_input_tokens),
                cache_write_input_tokens: add(
                    total.cache_write_input_tokens,
                    usage.cache_write_input_tokens,
                ),
                reasoning_output_tokens: add(
                    total.reasoning_output_tokens,
                    usage.reasoning_output_tokens,
                ),
                ..Default::default()
            };
        }
        total.generation = self.generation;
        Some(total)
    }
}

fn merge(report: &mut UsageBody, update: UsageBody) {
    report.input_tokens = update.input_tokens.or(report.input_tokens);
    report.cache_read_input_tokens = update
        .cache_read_input_tokens
        .or(report.cache_read_input_tokens);
    report.cache_creation_input_tokens = update
        .cache_creation_input_tokens
        .or(report.cache_creation_input_tokens);
    // An assistant-block echo can repeat the initial output counter after a
    // later streaming report. Only the final result may correct it downward.
    report.output_tokens = match (report.output_tokens, update.output_tokens) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    if let Some(details) = update.output_tokens_details {
        report.output_tokens_details = Some(details);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, output: u64) -> MessageBody {
        MessageBody {
            id: Some(id.into()),
            usage: Some(serde_json::json!({ "output_tokens": output })),
            ..Default::default()
        }
    }

    #[test]
    fn retired_requests_remain_ignored_across_warm_turns_and_steers() {
        let mut tracker = TurnUsageTracker::default();
        for i in 0..128 {
            assert_eq!(
                tracker
                    .start(Some(&message(&format!("request-{i}"), 10)), Instant::now())
                    .unwrap()
                    .output_tokens,
                Some(10)
            );
            tracker.reset();
        }
        assert_eq!(tracker.start(Some(&message("request-0", 999)), Instant::now()), None);
        assert_eq!(tracker.message(&message("request-127", 999)), None);
        assert_eq!(
            tracker.delta(&UsageBody {
                output_tokens: Some(999),
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            tracker
                .start(Some(&message("new", 7)), Instant::now())
                .unwrap()
                .output_tokens,
            Some(7)
        );
    }

    #[test]
    fn exhaustion_bounds_tracking_and_never_readmits_old_requests() {
        let mut tracker = TurnUsageTracker::default();
        for i in 0..MAX_TRACKED_REQUESTS {
            tracker.start(Some(&message(&format!("request-{i}"), 10)), Instant::now());
            tracker.reset();
        }
        assert_eq!(tracker.retired.len(), MAX_TRACKED_REQUESTS);
        assert!(!tracker.exhausted);
        assert_eq!(
            tracker.start(Some(&message("overflow", 10)), Instant::now()),
            Some(TokenUsage::default())
        );
        assert!(tracker.exhausted);
        assert!(tracker.retired.is_empty());
        assert!(tracker.requests.is_empty());
        for i in 0..MAX_TRACKED_REQUESTS {
            assert_eq!(
                tracker.start(Some(&message(&format!("request-{i}"), 999)), Instant::now()),
                None
            );
            tracker.reset();
        }
        assert_eq!(tracker.message(&message("new", 10)), None);
        assert!(tracker.has_reports());
        assert!(tracker.retired.is_empty());
        assert!(tracker.requests.is_empty());
    }

    #[test]
    fn an_active_request_at_capacity_can_still_update_and_deduplicate() {
        let mut tracker = TurnUsageTracker::default();
        for i in 0..MAX_TRACKED_REQUESTS - 1 {
            tracker.message(&message(&format!("old-{i}"), 10));
            tracker.reset();
        }
        tracker.start(Some(&message("active", 10)), Instant::now());
        let report = tracker
            .delta(&UsageBody {
                output_tokens: Some(20),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(report.output_tokens, Some(20));
        assert_eq!(
            tracker
                .message(&message("active", 10))
                .unwrap()
                .output_tokens,
            Some(20)
        );
        assert!(!tracker.exhausted);
        assert_eq!(
            tracker.requests.len() + tracker.retired.len(),
            MAX_TRACKED_REQUESTS
        );
    }

    #[test]
    fn oversized_request_ids_also_fall_back_without_retaining_them() {
        let mut tracker = TurnUsageTracker::default();
        let oversized = message(&"x".repeat(MAX_REQUEST_ID_BYTES + 1), 10);
        assert_eq!(tracker.start(Some(&oversized), Instant::now()), Some(TokenUsage::default()));
        assert!(tracker.exhausted);
        assert!(tracker.current.is_none());
        assert!(tracker.requests.is_empty());
        assert!(tracker.retired.is_empty());
    }

    #[test]
    fn generation_times_each_request_and_skips_bursts() {
        use std::time::Duration;
        let start = Instant::now();
        let mut tracker = TurnUsageTracker::default();
        // Thinking and text stream from message_start to message_stop.
        tracker.start(Some(&message("long", 1)), start);
        for _ in 0..10 {
            tracker.chunk();
        }
        tracker.delta(&UsageBody {
            output_tokens: Some(905),
            ..Default::default()
        });
        let live = tracker.stop(start + Duration::from_millis(9_050)).unwrap();
        assert_eq!(live.generation.unwrap().tps(), Some(100.0));
        // A short reply delivered as one burst is left out entirely.
        let later = start + Duration::from_secs(20);
        tracker.start(Some(&message("burst", 69)), later);
        tracker.chunk();
        assert_eq!(tracker.stop(later + Duration::from_millis(17)), None);
        assert_eq!(tracker.generation().unwrap().output_tokens, 905);
        // A retired request cannot open a window, and a new turn starts empty.
        tracker.reset();
        assert_eq!(tracker.generation(), None);
        tracker.start(Some(&message("long", 1)), later);
        tracker.chunk();
        assert_eq!(tracker.stop(later + Duration::from_secs(5)), None);
    }
}
