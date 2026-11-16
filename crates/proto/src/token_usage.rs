//! Token usage on this device, as `GetTokenUsage` reports it: what the
//! coding agents' own session logs say they spent, cut into hours or days
//! and, within each, by agent and model.
//!
//! Costs are estimates at public API rates (or what an agent wrote down
//! itself), not a subscription's bill.

use serde::{Deserialize, Serialize};

use crate::agent::HarnessId;

/// `GetTokenUsage`'s params: how many days back, today included. One day is
/// the past 24 hours, by the hour.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageParams {
    pub days: u32,
}

/// The four kinds of token a model bills differently.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenCounts {
    /// Input read fresh: neither from the prompt cache nor written to it.
    pub fresh_input: u64,
    /// Input served from the prompt cache.
    pub cache_read: u64,
    /// Input written into the prompt cache.
    pub cache_write: u64,
    /// Output, reasoning included.
    pub output: u64,
}

impl TokenCounts {
    /// All input, however it was billed.
    pub fn input(&self) -> u64 {
        self.fresh_input + self.cache_read + self.cache_write
    }

    pub fn total(&self) -> u64 {
        self.input() + self.output
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    pub fn absorb(&mut self, more: &TokenCounts) {
        self.fresh_input += more.fresh_input;
        self.cache_read += more.cache_read;
        self.cache_write += more.cache_write;
        self.output += more.output;
    }

    /// The share of input the cache served, 0 to 1 (0 with no input).
    pub fn cache_hit_rate(&self) -> f64 {
        match self.input() {
            0 => 0.0,
            input => self.cache_read as f64 / input as f64,
        }
    }
}

/// One agent's use of one model in one period.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSlice {
    /// Index into [`TokenUsage::starts_ms`].
    pub period: u32,
    pub harness: HarnessId,
    pub model: String,
    pub tokens: TokenCounts,
    pub cost_usd: f64,
    /// What reading from the cache saved over reading the same input fresh.
    pub saved_usd: f64,
    /// Model calls counted.
    pub calls: u64,
    /// Of those, the ones no price was found for (their cost is missing).
    pub unpriced_calls: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCount {
    pub harness: HarnessId,
    pub sessions: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    /// Periods are hours (the past 24 hours) rather than days.
    pub hourly: bool,
    /// Each period's start, Unix milliseconds, oldest first.
    pub starts_ms: Vec<i64>,
    /// Where the last period ends.
    pub end_ms: i64,
    pub slices: Vec<UsageSlice>,
    /// Distinct sessions per agent over the whole range.
    pub sessions: Vec<SessionCount>,
    /// Whether the public price list had loaded (without it, only what the
    /// agents recorded themselves is priced).
    pub prices_loaded: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hit_rate_is_read_over_all_input() {
        let t = TokenCounts { fresh_input: 10, cache_read: 80, cache_write: 10, output: 500 };
        assert_eq!(t.input(), 100);
        assert!((t.cache_hit_rate() - 0.8).abs() < 1e-9);
        assert_eq!(TokenCounts::default().cache_hit_rate(), 0.0);
    }

    #[test]
    fn wire_names_are_camel_case() {
        let json = serde_json::to_value(TokenCounts { fresh_input: 1, ..Default::default() }).unwrap();
        assert_eq!(json["freshInput"], 1);
    }
}
