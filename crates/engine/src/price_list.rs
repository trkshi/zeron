//! Public API prices per model, for putting a dollar figure on tokens an
//! agent didn't price itself (Settings → Usage, `token_usage`).
//!
//! The list is models.dev's catalogue, fetched at most once a day and kept
//! at `{data_dir}/model-prices.json` so a restart prices at once, offline
//! too. Until it has loaded, nothing is priced from it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeron_proto::HarnessId;

const CATALOGUE_URL: &str = "https://models.dev/api.json";
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
const FILE_NAME: &str = "model-prices.json";

/// Dollars per million tokens. A missing cache rate bills as plain input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Rates {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
}

impl Rates {
    /// Dollars for this many tokens of each kind.
    pub(crate) fn dollars(&self, fresh: u64, cache_read: u64, cache_write: u64, output: u64) -> f64 {
        let per_million = fresh as f64 * self.input
            + cache_read as f64 * self.cache_read.unwrap_or(self.input)
            + cache_write as f64 * self.cache_write.unwrap_or(self.input)
            + output as f64 * self.output;
        per_million / 1_000_000.0
    }

    /// What `cache_read` tokens would have cost more, read fresh.
    pub(crate) fn cache_saving(&self, cache_read: u64) -> f64 {
        let discount = self.input - self.cache_read.unwrap_or(self.input);
        cache_read as f64 * discount.max(0.0) / 1_000_000.0
    }
}

/// provider → model id → rates.
type Catalogue = BTreeMap<String, BTreeMap<String, Rates>>;

#[derive(Serialize, Deserialize)]
struct OnDisk {
    saved_at_secs: u64,
    catalogue: Catalogue,
}

static LOADED: RwLock<Option<Arc<Catalogue>>> = RwLock::new(None);

fn current() -> Option<Arc<Catalogue>> {
    LOADED.read().unwrap_or_else(|e| e.into_inner()).clone()
}

fn set_current(catalogue: Catalogue) {
    *LOADED.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(catalogue));
}

fn unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Loads what was saved, and fetches a fresh list in the background when
/// that is missing or a day old. Call once, inside the engine's runtime.
pub(crate) fn start(data_dir: &Path) {
    let file = data_dir.join(FILE_NAME);
    let saved = std::fs::read(&file)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<OnDisk>(&bytes).ok());
    let mut stale = true;
    if let Some(saved) = saved {
        stale = unix_secs().saturating_sub(saved.saved_at_secs) >= STALE_AFTER.as_secs();
        set_current(saved.catalogue);
    }
    if !stale {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    runtime.spawn(async move {
        if let Err(err) = fetch(file).await {
            tracing::warn!(error = %err, "model price list not refreshed");
        }
    });
}

async fn fetch(file: PathBuf) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let body: Value = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?
        .get(CATALOGUE_URL)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let catalogue = read_catalogue(&body);
    if catalogue.is_empty() {
        return Err("the price list had no priced models".into());
    }
    let bytes = serde_json::to_vec(&OnDisk { saved_at_secs: unix_secs(), catalogue: catalogue.clone() })?;
    let partial = file.with_extension("json.partial");
    std::fs::write(&partial, bytes)?;
    std::fs::rename(&partial, &file)?;
    set_current(catalogue);
    Ok(())
}

/// `{ provider: { models: { id: { cost: { input, output, cache_read?,
/// cache_write? } } } } }`, keeping only models that carry a cost.
fn read_catalogue(body: &Value) -> Catalogue {
    let mut catalogue = Catalogue::new();
    let Some(providers) = body.as_object() else {
        return catalogue;
    };
    for (provider, entry) in providers {
        let Some(models) = entry.get("models").and_then(Value::as_object) else {
            continue;
        };
        let priced: BTreeMap<String, Rates> = models
            .iter()
            .filter_map(|(id, model)| {
                let rates = serde_json::from_value(model.get("cost")?.clone()).ok()?;
                Some((id.clone(), rates))
            })
            .collect();
        if !priced.is_empty() {
            catalogue.insert(provider.clone(), priced);
        }
    }
    catalogue
}

/// The provider whose own list names an agent's models first.
fn home_provider(harness: HarnessId) -> Option<&'static str> {
    match harness {
        HarnessId::ClaudeCode => Some("anthropic"),
        HarnessId::Codex => Some("openai"),
        HarnessId::Grok => Some("xai"),
        HarnessId::Antigravity => Some("google"),
        _ => None,
    }
}

fn find(catalogue: &Catalogue, harness: HarnessId, model: &str) -> Option<Rates> {
    // "claude-opus-5-5[1m]": the context variant bills as the model.
    let model = model.split('[').next().unwrap_or(model).trim();
    if model.is_empty() || model == "default" {
        return None;
    }
    // "openrouter/gpt-x": that provider's rate, else the bare model's.
    if let Some((provider, id)) = model.split_once('/') {
        if let Some(rates) = catalogue.get(provider).and_then(|models| models.get(id)) {
            return Some(*rates);
        }
        return find(catalogue, harness, id);
    }
    let preferred = home_provider(harness)
        .into_iter()
        .chain(["anthropic", "openai", "google", "xai"]);
    for provider in preferred {
        if let Some(rates) = catalogue.get(provider).and_then(|models| models.get(model)) {
            return Some(*rates);
        }
    }
    catalogue.values().find_map(|models| models.get(model).copied())
}

/// The rates for an agent's model, once the list has loaded.
pub(crate) fn rates(harness: HarnessId, model: &str) -> Option<Rates> {
    let catalogue = current()?;
    find(&catalogue, harness, model)
}

pub(crate) fn loaded() -> bool {
    current().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Catalogue {
        read_catalogue(&serde_json::json!({
            "anthropic": { "models": {
                "claude-test": { "cost": { "input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75 } }
            }},
            "openai": { "models": {
                "gpt-test": { "cost": { "input": 2, "output": 8 } },
                "unpriced": { "name": "no cost here" }
            }},
            "reseller": { "models": {
                "gpt-test": { "cost": { "input": 5, "output": 5 } },
                "rare": { "cost": { "input": 1, "output": 1 } }
            }},
            "junk": { "models": [] }
        }))
    }

    #[test]
    fn keeps_only_priced_models() {
        let c = sample();
        assert_eq!(c.keys().map(String::as_str).collect::<Vec<_>>(), ["anthropic", "openai", "reseller"]);
        assert!(!c["openai"].contains_key("unpriced"));
    }

    #[test]
    fn finds_the_agents_own_provider_first() {
        let c = sample();
        let input = |h, m| find(&c, h, m).map(|r| r.input);
        assert_eq!(input(HarnessId::Codex, "gpt-test"), Some(2.0));
        assert_eq!(input(HarnessId::Pi, "reseller/gpt-test"), Some(5.0));
        assert_eq!(input(HarnessId::Pi, "elsewhere/gpt-test"), Some(2.0));
        assert_eq!(input(HarnessId::ClaudeCode, "claude-test[1m]"), Some(3.0));
        assert_eq!(input(HarnessId::Cursor, "rare"), Some(1.0));
        assert_eq!(input(HarnessId::Codex, "missing"), None);
        assert_eq!(input(HarnessId::Pi, "default"), None);
    }

    #[test]
    fn prices_each_kind_of_token() {
        let rates = sample()["anthropic"]["claude-test"];
        let usd = rates.dollars(1_000_000, 1_000_000, 1_000_000, 1_000_000);
        assert!((usd - (3.0 + 0.3 + 3.75 + 15.0)).abs() < 1e-9);
        assert!((rates.cache_saving(1_000_000) - 2.7).abs() < 1e-9);
        let plain = sample()["openai"]["gpt-test"];
        assert_eq!(plain.cache_saving(1_000_000), 0.0);
    }
}
