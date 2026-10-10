//! Host-local billing history. Quota limits and context occupancy remain separate.
use serde::{Deserialize, Serialize};

use crate::{HarnessId, TokenUsage};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageHistoryRecord {
    pub message_id: String,
    pub chat_id: String,
    pub device_id: String,
    pub started_at: i64,
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub usage: TokenUsage,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UsageHistoryTotals {
    pub turns: u64,
    pub sessions: u64,
    pub usage: TokenUsage,
    pub reported_cost_usd: f64,
    pub estimated_cost_usd: f64,
    pub unpriced_turns: u64,
    pub incomplete_turns: u64,
    pub incomplete_cache_turns: u64,
    pub uncached_input_tokens: Option<u64>,
    /// Standard API-rate equivalent, not subscription savings or a refund.
    pub cache_savings_usd: f64,
    /// Input, cache read, cache write, output, and unsplittable cost.
    pub category_cost_usd: [f64; 5],
}

impl UsageHistoryTotals {
    pub fn cost_usd(&self) -> f64 {
        self.reported_cost_usd + self.estimated_cost_usd
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageHistoryGroup {
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub totals: UsageHistoryTotals,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageHistoryBucket {
    pub started_at: i64,
    pub groups: Vec<UsageHistoryGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageHistorySummary {
    pub device_id: String,
    pub read_at: i64,
    pub since: i64,
    pub until: i64,
    pub hourly: bool,
    pub totals: UsageHistoryTotals,
    pub providers: Vec<UsageHistoryGroup>,
    pub models: Vec<UsageHistoryGroup>,
    pub buckets: Vec<UsageHistoryBucket>,
    pub pricing_updated_at: Option<i64>,
    pub pricing_error: Option<String>,
    pub skipped_sources: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UsageHistoryQuery {
    pub days: u32,
    pub utc_offset_minutes: i32,
    pub refresh_prices: bool,
}
