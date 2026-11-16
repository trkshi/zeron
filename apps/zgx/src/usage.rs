//! Settings → Usage's figures (ui/account/usage.luau): the engine's
//! `GetTokenUsage` for this device, added up and written out here so the
//! page only lays them out.
//!
//!   usage.read({ days })   1, 7, 30 or 90 → {
//!     hourly, prices_loaded, sessions, unpriced (calls without a price),
//!     cost, tokens, fresh, cache_read, cache_write, output, saved, hit_rate
//!       (words: "$12.34", "1.5M", "62%"), cost_n, tokens_n (numbers),
//!     agents  = { { harness, sessions, cost, tokens, cost_n, tokens_n,
//!                   cost_share, tokens_share } }        the busiest first
//!     models  = { { key, model, harness, cost, tokens, cost_n, tokens_n,
//!                   cost_share, tokens_share, hit_rate, per_million } }
//!                                                        the priciest first
//!     periods = { { start (Unix seconds), cost, tokens, cost_n, tokens_n,
//!                   split = { { harness, cost_n, tokens_n } } } }
//!                                                        oldest first }
//!   `per_million` is what a million of the model's tokens came to, blended
//!   over the kinds it used ("" while nothing of it was priced).
//! It rejects with an outcome word (`offline`, `failed`, `invalid`).

use std::collections::HashMap;

use serde_json::json;
use zeron_proto::HarnessId;
use zeron_proto::token_usage::{TokenCounts, TokenUsage};
use zgx_desktop::WindowContext;
use zgx_script::{ScriptHost, Value};

use crate::engine;

const RANGES: [u32; 4] = [1, 7, 30, 90];

pub fn install(_cx: &mut WindowContext, host: &ScriptHost) {
    host.async_action("usage.read", |args| {
        let days = args.field("days").and_then(Value::as_f64).map(|d| d as u32);
        async move {
            let Some(days) = days.filter(|d| RANGES.contains(d)) else {
                return Err("invalid".to_string());
            };
            let answer = engine::get()
                .call("GetTokenUsage", json!({ "days": days }))
                .await
                .map_err(|error| {
                    eprintln!("zeron-zgx: usage: {error}");
                    engine::outcome(&error).to_string()
                })?;
            let usage: TokenUsage = serde_json::from_value(answer).map_err(|error| {
                eprintln!("zeron-zgx: usage: unreadable answer: {error}");
                "failed".to_string()
            })?;
            Ok(Sums::of(&usage).value(&usage))
        }
    });
}

#[derive(Default)]
struct Sum {
    tokens: TokenCounts,
    cost: f64,
    priced_cost: bool,
}

impl Sum {
    fn add(&mut self, tokens: &TokenCounts, cost: f64, priced: bool) {
        self.tokens.absorb(tokens);
        self.cost += cost;
        self.priced_cost |= priced;
    }
}

/// The report added up every way the page shows it.
struct Sums {
    all: Sum,
    saved: f64,
    unpriced: u64,
    agents: Vec<(HarnessId, Sum)>,
    models: Vec<((HarnessId, String), Sum)>,
    periods: Vec<Sum>,
    /// Each period's figures by agent.
    split: Vec<HashMap<HarnessId, (f64, u64)>>,
}

impl Sums {
    fn of(usage: &TokenUsage) -> Self {
        let mut all = Sum::default();
        let (mut saved, mut unpriced) = (0.0, 0);
        let mut agents: HashMap<HarnessId, Sum> = HashMap::new();
        let mut models: HashMap<(HarnessId, String), Sum> = HashMap::new();
        let mut periods: Vec<Sum> = usage.starts_ms.iter().map(|_| Sum::default()).collect();
        let mut split: Vec<HashMap<HarnessId, (f64, u64)>> = usage.starts_ms.iter().map(|_| HashMap::new()).collect();
        for slice in &usage.slices {
            let priced = slice.calls > slice.unpriced_calls;
            all.add(&slice.tokens, slice.cost_usd, priced);
            saved += slice.saved_usd;
            unpriced += slice.unpriced_calls;
            agents.entry(slice.harness).or_default().add(&slice.tokens, slice.cost_usd, priced);
            models
                .entry((slice.harness, slice.model.clone()))
                .or_default()
                .add(&slice.tokens, slice.cost_usd, priced);
            if let Some(period) = periods.get_mut(slice.period as usize) {
                period.add(&slice.tokens, slice.cost_usd, priced);
            }
            if let Some(by_agent) = split.get_mut(slice.period as usize) {
                let entry = by_agent.entry(slice.harness).or_default();
                entry.0 += slice.cost_usd;
                entry.1 += slice.tokens.total();
            }
        }
        let mut agents: Vec<_> = agents.into_iter().collect();
        agents.sort_by(|a, b| b.1.cost.total_cmp(&a.1.cost).then(b.1.tokens.total().cmp(&a.1.tokens.total())));
        let mut models: Vec<_> = models.into_iter().collect();
        models.sort_by(|a, b| {
            b.1.cost
                .total_cmp(&a.1.cost)
                .then(b.1.tokens.total().cmp(&a.1.tokens.total()))
                .then(a.0.1.cmp(&b.0.1))
        });
        Self { all, saved, unpriced, agents, models, periods, split }
    }

    fn value(&self, usage: &TokenUsage) -> Value {
        let total_cost = self.all.cost;
        let total_tokens = self.all.tokens.total() as f64;
        let sessions_of = |harness: HarnessId| {
            usage.sessions.iter().find(|s| s.harness == harness).map_or(0, |s| s.sessions)
        };
        let agents: Vec<Value> = self
            .agents
            .iter()
            .map(|(harness, sum)| {
                Value::record([
                    ("harness", Value::from(harness.as_str())),
                    ("sessions", Value::from(sessions_of(*harness) as f64)),
                    ("cost", Value::from(dollars(sum.cost).as_str())),
                    ("tokens", Value::from(token_count(sum.tokens.total()).as_str())),
                    ("cost_n", Value::from(sum.cost)),
                    ("tokens_n", Value::from(sum.tokens.total() as f64)),
                    ("cost_share", Value::from(share(sum.cost, total_cost).as_str())),
                    ("tokens_share", Value::from(share(sum.tokens.total() as f64, total_tokens).as_str())),
                ])
            })
            .collect();
        let models: Vec<Value> = self
            .models
            .iter()
            .map(|((harness, model), sum)| {
                let tokens = sum.tokens.total();
                let per_million = if sum.priced_cost && tokens > 0 {
                    dollars(sum.cost / (tokens as f64 / 1_000_000.0))
                } else {
                    String::new()
                };
                Value::record([
                    ("key", Value::from(format!("{}/{model}", harness.as_str()).as_str())),
                    ("model", Value::from(model.as_str())),
                    ("harness", Value::from(harness.as_str())),
                    ("cost", Value::from(dollars(sum.cost).as_str())),
                    ("tokens", Value::from(token_count(tokens).as_str())),
                    ("cost_n", Value::from(sum.cost)),
                    ("tokens_n", Value::from(tokens as f64)),
                    ("cost_share", Value::from(share(sum.cost, total_cost).as_str())),
                    ("tokens_share", Value::from(share(tokens as f64, total_tokens).as_str())),
                    ("hit_rate", Value::from(rate(sum.tokens.cache_hit_rate()).as_str())),
                    ("per_million", Value::from(per_million.as_str())),
                ])
            })
            .collect();
        let periods: Vec<Value> = usage
            .starts_ms
            .iter()
            .zip(self.periods.iter().zip(&self.split))
            .map(|(start, (sum, by_agent))| {
                let split: Vec<Value> = by_agent
                    .iter()
                    .map(|(harness, (cost, tokens))| {
                        Value::record([
                            ("harness", Value::from(harness.as_str())),
                            ("cost_n", Value::from(*cost)),
                            ("tokens_n", Value::from(*tokens as f64)),
                        ])
                    })
                    .collect();
                Value::record([
                    ("start", Value::from(*start as f64 / 1000.0)),
                    ("cost", Value::from(dollars(sum.cost).as_str())),
                    ("tokens", Value::from(token_count(sum.tokens.total()).as_str())),
                    ("cost_n", Value::from(sum.cost)),
                    ("tokens_n", Value::from(sum.tokens.total() as f64)),
                    ("split", Value::List(split.into())),
                ])
            })
            .collect();
        let t = &self.all.tokens;
        let sessions: u64 = usage.sessions.iter().map(|s| s.sessions).sum();
        Value::record([
            ("hourly", Value::Bool(usage.hourly)),
            ("prices_loaded", Value::Bool(usage.prices_loaded)),
            ("sessions", Value::from(sessions as f64)),
            ("unpriced", Value::from(self.unpriced as f64)),
            ("cost", Value::from(dollars(total_cost).as_str())),
            ("tokens", Value::from(token_count(t.total()).as_str())),
            ("cost_n", Value::from(total_cost)),
            ("tokens_n", Value::from(t.total() as f64)),
            ("fresh", Value::from(token_count(t.fresh_input).as_str())),
            ("cache_read", Value::from(token_count(t.cache_read).as_str())),
            ("cache_write", Value::from(token_count(t.cache_write).as_str())),
            ("output", Value::from(token_count(t.output).as_str())),
            ("saved", Value::from(dollars(self.saved).as_str())),
            ("hit_rate", Value::from(rate(t.cache_hit_rate()).as_str())),
            ("agents", Value::List(agents.into())),
            ("models", Value::List(models.into())),
            ("periods", Value::List(periods.into())),
        ])
    }
}

/// "$0.042" under a dollar, else "$1,234.56".
fn dollars(usd: f64) -> String {
    let usd = if usd.is_finite() { usd.max(0.0) } else { 0.0 };
    if usd < 1.0 {
        return format!("${usd:.3}");
    }
    let cents = format!("{usd:.2}");
    let (whole, fraction) = cents.split_once('.').unwrap_or((&cents, "00"));
    format!("${}.{fraction}", grouped(whole))
}

fn grouped(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "950", "9.5K", "120K", "1.25M", "38M", "2.1B".
fn token_count(n: u64) -> String {
    fn short(value: f64, places: usize, unit: &str) -> String {
        let text = format!("{value:.places$}");
        let text = if text.contains('.') { text.trim_end_matches('0').trim_end_matches('.') } else { &text };
        format!("{text}{unit}")
    }
    let x = n as f64;
    match x {
        x if x >= 1e10 => short(x / 1e9, 1, "B"),
        x if x >= 999_500_000.0 => short(x / 1e9, 2, "B"),
        x if x >= 1e7 => short(x / 1e6, 1, "M"),
        x if x >= 999_500.0 => short(x / 1e6, 2, "M"),
        x if x >= 1e5 => short(x / 1e3, 0, "K"),
        x if x >= 1e3 => short(x / 1e3, 1, "K"),
        _ => n.to_string(),
    }
}

/// `part` of `whole` as a percentage ("12.5%"; "0%" of nothing).
fn share(part: f64, whole: f64) -> String {
    if whole > 0.0 { rate(part / whole) } else { "0%".to_string() }
}

/// A 0 to 1 fraction as a percentage, a decimal only under ten.
fn rate(f: f64) -> String {
    let pct = (f * 100.0).clamp(0.0, 100.0);
    if pct > 0.0 && pct < 10.0 { format!("{pct:.1}%") } else { format!("{pct:.0}%") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn figures_read_short() {
        assert_eq!(dollars(0.0421), "$0.042");
        assert_eq!(dollars(1234.5), "$1,234.50");
        assert_eq!(token_count(950), "950");
        assert_eq!(token_count(9_500), "9.5K");
        assert_eq!(token_count(120_400), "120K");
        assert_eq!(token_count(1_250_000), "1.25M");
        assert_eq!(token_count(38_000_000), "38M");
        assert_eq!(share(1.0, 4.0), "25%");
        assert_eq!(rate(0.062), "6.2%");
        assert_eq!(share(1.0, 0.0), "0%");
    }
}
