use std::collections::{HashMap, HashSet};

use zeron_proto::TokenUsage;

use super::wire::{MessageBody, UsageBody};

/// Claude repeats one API message across content blocks. Keep request IDs
/// separate from Zeron's display IDs so repeated usage never adds twice.
#[derive(Default)]
pub(super) struct TurnUsageTracker {
    requests: HashMap<String, UsageBody>,
    retired: HashSet<String>,
    current: Option<String>,
    incomplete: bool,
}

impl TurnUsageTracker {
    pub fn reset(&mut self) {
        self.retired.extend(self.requests.drain().map(|(id, _)| id));
        self.current = None;
        self.incomplete = false;
    }

    pub fn has_reports(&self) -> bool {
        !self.requests.is_empty()
    }

    pub fn start(&mut self, message: &MessageBody) -> Option<TokenUsage> {
        self.current = message
            .id
            .as_ref()
            .filter(|id| !id.is_empty() && !self.retired.contains(*id))
            .cloned();
        self.message(message)
    }

    pub fn stop(&mut self) {
        self.current = None;
    }

    pub fn message(&mut self, message: &MessageBody) -> Option<TokenUsage> {
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
