//! Scalar usage index: no transcript subscriptions, prompt text, or credential copies.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use loro::{Container, LoroDoc, LoroValue, ToJson, ValueOrContainer};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use zeron_proto::{
    HarnessId, TokenUsage, UsageHistoryBucket, UsageHistoryGroup, UsageHistoryQuery,
    UsageHistoryRecord, UsageHistorySummary, UsageHistoryTotals,
};
use zeron_sync::DocsStore;

use crate::{EngineError, WorkspaceHost, now_ms};

const PRICE_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const DAY_MS: i64 = 86_400_000;
const MAX_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Rate {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

#[derive(Default, Serialize, Deserialize)]
struct Pricing {
    fetched_at: Option<i64>,
    rates: HashMap<String, Rate>,
}

struct Inner {
    db: Mutex<Connection>,
    store: Arc<DocsStore>,
    workspace: WorkspaceHost,
    device_id: String,
    scan: tokio::sync::Mutex<()>,
    price_attempt: Mutex<Option<i64>>,
    last_scan: Mutex<Option<(i64, u64)>>,
}

#[derive(Clone)]
pub struct UsageHistory(Arc<Inner>);

fn db_error(error: impl std::fmt::Display) -> EngineError {
    EngineError::Other(format!("Usage history: {error}"))
}

impl UsageHistory {
    pub fn open(
        root: &Path,
        store: Arc<DocsStore>,
        workspace: WorkspaceHost,
        device_id: String,
    ) -> Result<Self, EngineError> {
        let db = Connection::open(root.join("usage-history.sqlite3")).map_err(db_error)?;
        db.busy_timeout(std::time::Duration::from_secs(2))
            .map_err(db_error)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA journal_size_limit=4194304;
             CREATE TABLE IF NOT EXISTS usage_turns (
               device_id TEXT NOT NULL, message_id TEXT NOT NULL, started_at INTEGER NOT NULL,
               payload TEXT NOT NULL, PRIMARY KEY(device_id, message_id));
             CREATE INDEX IF NOT EXISTS usage_turns_time ON usage_turns(started_at);
             CREATE TABLE IF NOT EXISTS usage_sources (
               chat_id TEXT PRIMARY KEY, cursor INTEGER NOT NULL, epoch INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS usage_pricing (
               id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL);",
        )
        .map_err(db_error)?;
        Ok(Self(Arc::new(Inner {
            db: Mutex::new(db),
            store,
            workspace,
            device_id,
            scan: tokio::sync::Mutex::new(()),
            price_attempt: Mutex::new(None),
            last_scan: Mutex::new(None),
        })))
    }

    /// Called only at turn boundaries, never on text deltas or render frames.
    pub async fn record(&self, record: UsageHistoryRecord) {
        let this = self.clone();
        let result = tokio::task::spawn_blocking(move || this.write(&record, false)).await;
        if let Err(error) = result.unwrap_or_else(|error| Err(db_error(error))) {
            tracing::warn!(%error, "usage index write failed; transcript remains authoritative");
        }
    }

    fn write(&self, record: &UsageHistoryRecord, imported: bool) -> Result<(), EngineError> {
        let payload = serde_json::to_string(record).map_err(db_error)?;
        let db = self.0.db.lock().unwrap_or_else(PoisonError::into_inner);
        // Forks copy message IDs. One host's original billed turn counts once;
        // restored checkpoints do not erase already-consumed usage.
        let query = if imported {
            "INSERT OR IGNORE INTO usage_turns VALUES (?1, ?2, ?3, ?4)"
        } else {
            "INSERT INTO usage_turns VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(device_id, message_id) DO UPDATE SET
             started_at=excluded.started_at, payload=excluded.payload"
        };
        db.execute(
            query,
            params![
                record.device_id,
                record.message_id,
                record.started_at,
                payload
            ],
        )
        .map_err(db_error)?;
        Ok(())
    }

    fn backfill(&self) -> Result<u64, EngineError> {
        let mut skipped = 0;
        let mut chats = self.0.workspace.read_chats()?;
        chats.sort_by_key(|chat| chat.created_at);
        for chat in chats
            .into_iter()
            .filter(|chat| chat.device_id == self.0.device_id)
        {
            let cursor = self.0.store.snapshot_cursor(&chat.id)?;
            let epoch = self.0.store.snapshot_epoch(&chat.id)?;
            let known: Option<(u64, u32)> = {
                let db = self.0.db.lock().unwrap_or_else(PoisonError::into_inner);
                db.query_row(
                    "SELECT cursor, epoch FROM usage_sources WHERE chat_id=?1",
                    [&chat.id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(db_error)?
            };
            if known == Some((cursor, epoch)) {
                continue;
            }
            let Some((bytes, loaded_cursor, loaded_epoch)) =
                self.0.store.load_snapshot_with_cursor(&chat.id)?
            else {
                skipped += 1;
                continue;
            };
            if bytes.len() > MAX_SNAPSHOT_BYTES {
                skipped += 1;
                continue;
            }
            let raw = loro::LoroDoc::new();
            if raw.import(&bytes).is_err() {
                skipped += 1;
                continue;
            }
            for record in snapshot_records(&raw, &chat.id, &self.0.device_id) {
                self.write(&record, true)?;
            }
            self.0.db.lock().unwrap_or_else(PoisonError::into_inner)
                .execute("INSERT INTO usage_sources VALUES (?1, ?2, ?3)
                    ON CONFLICT(chat_id) DO UPDATE SET cursor=excluded.cursor, epoch=excluded.epoch",
                    params![chat.id, loaded_cursor, loaded_epoch]).map_err(db_error)?;
        }
        Ok(skipped)
    }

    fn pricing(&self) -> Result<Pricing, EngineError> {
        let payload: Option<String> = self
            .0
            .db
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .query_row("SELECT payload FROM usage_pricing WHERE id=1", [], |row| {
                row.get(0)
            })
            .optional()
            .map_err(db_error)?;
        payload
            .map(|payload| serde_json::from_str(&payload).map_err(db_error))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    async fn refresh_pricing(&self, force: bool) -> Result<(), EngineError> {
        let fetched = self.pricing()?.fetched_at;
        let now = now_ms();
        if !force && fetched.is_some_and(|at| now.saturating_sub(at) < DAY_MS) {
            return Ok(());
        }
        {
            let mut attempted = self
                .0
                .price_attempt
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if attempted.is_some_and(|at| now.saturating_sub(at) < 60_000) {
                return Ok(());
            }
            *attempted = Some(now);
        }
        let response = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(12))
            .build()
            .map_err(db_error)?
            .get(PRICE_URL)
            .send()
            .await
            .map_err(db_error)?
            .error_for_status()
            .map_err(db_error)?;
        if response
            .content_length()
            .is_some_and(|size| size > 8 * 1024 * 1024)
        {
            return Err(db_error("pricing response exceeds size limit"));
        }
        let bytes = response.bytes().await.map_err(db_error)?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err(db_error("pricing response exceeds size limit"));
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(db_error)?;
        let rates = parse_rates(&value);
        if rates.is_empty() {
            return Err(db_error("pricing source returned no usable model rates"));
        }
        let pricing = serde_json::to_string(&Pricing {
            fetched_at: Some(now),
            rates,
        })
        .map_err(db_error)?;
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            this.0
                .db
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .execute(
                    "INSERT INTO usage_pricing VALUES (1, ?1)
                    ON CONFLICT(id) DO UPDATE SET payload=excluded.payload",
                    [pricing],
                )
                .map_err(db_error)
        })
        .await
        .map_err(db_error)??;
        Ok(())
    }

    pub async fn read(&self, query: UsageHistoryQuery) -> Result<UsageHistorySummary, EngineError> {
        if !matches!(query.days, 1 | 7 | 30 | 90)
            || !(-840..=840).contains(&query.utc_offset_minutes)
        {
            return Err(db_error(
                "choose 1, 7, 30, or 90 days and a valid UTC offset",
            ));
        }
        let _scan = self.0.scan.lock().await;
        let pricing_error = self
            .refresh_pricing(query.refresh_prices)
            .await
            .err()
            .map(|e| e.to_string());
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let last_scan = *this
                .0
                .last_scan
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let skipped = match last_scan {
                Some((at, skipped))
                    if !query.refresh_prices && now_ms().saturating_sub(at) < 120_000 =>
                {
                    skipped
                }
                _ => {
                    let skipped = this.backfill()?;
                    *this
                        .0
                        .last_scan
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) = Some((now_ms(), skipped));
                    skipped
                }
            };
            let pricing = this.pricing()?;
            let until = now_ms();
            let offset = i64::from(query.utc_offset_minutes) * 60_000;
            let hourly = query.days == 1;
            let since = if hourly {
                until - DAY_MS
            } else {
                (until + offset).div_euclid(DAY_MS) * DAY_MS
                    - offset
                    - (i64::from(query.days) - 1) * DAY_MS
            };
            let records: Vec<UsageHistoryRecord> = {
                let db = this.0.db.lock().unwrap_or_else(PoisonError::into_inner);
                let mut statement = db
                    .prepare(
                        "SELECT payload FROM usage_turns
                    WHERE started_at>=?1 AND started_at<=?2 ORDER BY started_at",
                    )
                    .map_err(db_error)?;
                let rows = statement
                    .query_map(params![since, until], |row| row.get::<_, String>(0))
                    .map_err(db_error)?;
                rows.map(|row| serde_json::from_str(&row.map_err(db_error)?).map_err(db_error))
                    .collect::<Result<_, _>>()?
            };
            Ok(aggregate(
                &this.0.device_id,
                &records,
                &pricing,
                since,
                until,
                hourly,
                offset,
                pricing_error,
                skipped,
            ))
        })
        .await
        .map_err(db_error)?
    }
}

/// Inspect billing scalars only: never decode message text, parts, or tool outputs.
fn snapshot_records(raw: &LoroDoc, chat_id: &str, device_id: &str) -> Vec<UsageHistoryRecord> {
    let messages = raw.get_list("messages");
    let mut entries: HashMap<String, (UsageHistoryRecord, bool)> = HashMap::new();
    for index in 0..messages.len() {
        let Some(ValueOrContainer::Container(Container::Map(map))) = messages.get(index) else {
            continue;
        };
        let string = |key: &str| match map.get(key) {
            Some(ValueOrContainer::Value(LoroValue::String(value))) => Some(value.to_string()),
            _ => None,
        };
        if string("role").as_deref() != Some("assistant")
            || string("deviceId").as_deref() != Some(device_id)
        {
            continue;
        }
        let Some(id) = string("id") else {
            continue;
        };
        let root = string("continuationOf").unwrap_or(id);
        let started = match map.get("createdAt") {
            Some(ValueOrContainer::Value(LoroValue::I64(value))) => value,
            _ => continue,
        };
        let usage = match map.get("tokenUsage") {
            Some(ValueOrContainer::Value(LoroValue::String(json))) => {
                serde_json::from_str(&json).ok()
            }
            Some(value) => serde_json::from_value(value.get_deep_value().to_json_value()).ok(),
            None => None,
        };
        let entry = entries.entry(root.clone()).or_insert_with(|| {
            (
                UsageHistoryRecord {
                    message_id: root,
                    chat_id: chat_id.into(),
                    device_id: device_id.into(),
                    started_at: started,
                    harness: None,
                    model: None,
                    usage: TokenUsage::default(),
                },
                false,
            )
        });
        if usage.is_some() || map.get("durationMs").is_some() {
            entry.0.usage = usage.unwrap_or_default();
        }
        entry.1 = string("status").as_deref() == Some("streaming");
    }
    entries
        .into_values()
        .filter_map(|(record, streaming)| {
            (!streaming && record.usage != TokenUsage::default()).then_some(record)
        })
        .collect()
}

fn parse_rates(value: &serde_json::Value) -> HashMap<String, Rate> {
    let mut rates = HashMap::new();
    let mut aliases: HashMap<String, Option<Rate>> = HashMap::new();
    let Some(models) = value.as_object() else {
        return rates;
    };
    for (model, value) in models {
        let read = |key: &str| {
            value
                .get(key)?
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0)
        };
        let (Some(input), Some(output)) =
            (read("input_cost_per_token"), read("output_cost_per_token"))
        else {
            continue;
        };
        let rate = Rate {
            input,
            output,
            cache_read: read("cache_read_input_token_cost").unwrap_or(input),
            cache_write: read("cache_creation_input_token_cost").unwrap_or(input),
        };
        let key = model.to_lowercase();
        let bare = key.rsplit('/').next().unwrap_or(&key).to_owned();
        aliases
            .entry(bare)
            .and_modify(|known| {
                if *known != Some(rate) {
                    *known = None;
                }
            })
            .or_insert(Some(rate));
        rates.insert(key, rate);
    }
    for (key, rate) in aliases {
        if let Some(rate) = rate {
            rates.entry(key).or_insert(rate);
        }
    }
    rates
}

#[derive(Default)]
struct Accumulator {
    totals: UsageHistoryTotals,
    sessions: HashSet<String>,
}

impl Accumulator {
    fn add(&mut self, record: &UsageHistoryRecord, pricing: &Pricing) {
        let totals = &mut self.totals;
        let usage = record.usage;
        totals.turns += 1;
        self.sessions.insert(record.chat_id.clone());
        totals.sessions = self.sessions.len() as u64;
        let add = |a: Option<u64>, b: Option<u64>| match (a, b) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (a, b) => a.or(b),
        };
        // Normalize the not-applicable Codex category only in the aggregate;
        // the stored provider report remains unchanged.
        let writes = usage
            .cache_write_input_tokens
            .or_else(|| (record.harness == Some(HarnessId::Codex)).then_some(0));
        let uncached = usage
            .input_tokens
            .zip(usage.cached_input_tokens)
            .zip(writes)
            .and_then(|((input, cached), writes)| input.checked_sub(cached)?.checked_sub(writes));
        totals.uncached_input_tokens = add(totals.uncached_input_tokens, uncached);
        if uncached.is_none() {
            totals.incomplete_cache_turns += 1;
        }
        totals.usage = TokenUsage {
            input_tokens: add(totals.usage.input_tokens, usage.input_tokens),
            output_tokens: add(totals.usage.output_tokens, usage.output_tokens),
            total_tokens: add(totals.usage.total_tokens, usage.total()),
            cached_input_tokens: add(totals.usage.cached_input_tokens, usage.cached_input_tokens),
            cache_write_input_tokens: add(totals.usage.cache_write_input_tokens, writes),
            reasoning_output_tokens: add(
                totals.usage.reasoning_output_tokens,
                usage.reasoning_output_tokens,
            ),
            ..Default::default()
        };
        if usage.input_tokens.is_none() || usage.output_tokens.is_none() {
            totals.incomplete_turns += 1;
        }
        let rate = record
            .model
            .as_ref()
            .and_then(|model| pricing.rates.get(&model.to_lowercase()));
        let categories = rate.and_then(|rate| {
            let cached = usage.cached_input_tokens?;
            let writes = writes?;
            let uncached = uncached?;
            let categories = [
                uncached as f64 * rate.input,
                cached as f64 * rate.cache_read,
                writes as f64 * rate.cache_write,
                usage.output_tokens? as f64 * rate.output,
            ];
            categories
                .iter()
                .all(|cost| cost.is_finite())
                .then_some(categories)
        });
        let reported = usage
            .cost_usd
            .filter(|cost| cost.is_finite() && *cost >= 0.0);
        if let Some(cost) = reported {
            totals.reported_cost_usd += cost;
        } else if let Some(categories) = categories {
            totals.estimated_cost_usd += categories.iter().sum::<f64>();
        } else {
            totals.unpriced_turns += 1;
        }
        if let Some(categories) = categories {
            let list_cost: f64 = categories.iter().sum();
            let scale = reported.map_or(1.0, |cost| {
                if list_cost > 0.0 {
                    cost / list_cost
                } else {
                    0.0
                }
            });
            for (total, cost) in totals.category_cost_usd.iter_mut().zip(categories) {
                *total += cost * scale;
            }
            if list_cost == 0.0 {
                totals.category_cost_usd[4] += reported.unwrap_or(0.0);
            }
            if let Some(rate) = rate {
                totals.cache_savings_usd += usage.cached_input_tokens.unwrap_or(0) as f64
                    * (rate.input - rate.cache_read).max(0.0);
            }
        } else {
            totals.category_cost_usd[4] += reported.unwrap_or(0.0);
        }
    }
}

type GroupKey = (Option<HarnessId>, Option<String>);

#[allow(clippy::too_many_arguments)]
fn aggregate(
    device: &str,
    records: &[UsageHistoryRecord],
    pricing: &Pricing,
    since: i64,
    until: i64,
    hourly: bool,
    offset: i64,
    pricing_error: Option<String>,
    skipped_sources: u64,
) -> UsageHistorySummary {
    let mut total = Accumulator::default();
    let mut models: HashMap<GroupKey, Accumulator> = HashMap::new();
    let mut providers: HashMap<GroupKey, Accumulator> = HashMap::new();
    let mut buckets: BTreeMap<i64, HashMap<GroupKey, Accumulator>> = BTreeMap::new();
    let interval = if hourly { 3_600_000 } else { DAY_MS };
    for record in records {
        total.add(record, pricing);
        providers
            .entry((record.harness, None))
            .or_default()
            .add(record, pricing);
        let key = (record.harness, record.model.clone());
        models.entry(key.clone()).or_default().add(record, pricing);
        let bucket = (record.started_at + offset).div_euclid(interval) * interval - offset;
        buckets
            .entry(bucket)
            .or_default()
            .entry(key)
            .or_default()
            .add(record, pricing);
    }
    let groups = |map: HashMap<GroupKey, Accumulator>| {
        let mut groups: Vec<_> = map
            .into_iter()
            .map(|((harness, model), value)| UsageHistoryGroup {
                harness,
                model,
                totals: value.totals,
            })
            .collect();
        groups.sort_by(|a, b| {
            b.totals
                .usage
                .total()
                .unwrap_or(0)
                .cmp(&a.totals.usage.total().unwrap_or(0))
                .then_with(|| a.model.cmp(&b.model))
        });
        groups
    };
    UsageHistorySummary {
        device_id: device.into(),
        read_at: until,
        since,
        until,
        hourly,
        totals: total.totals,
        providers: groups(providers),
        models: groups(models),
        buckets: buckets
            .into_iter()
            .map(|(started_at, entries)| UsageHistoryBucket {
                started_at,
                groups: groups(entries),
            })
            .collect(),
        pricing_updated_at: pricing.fetched_at,
        pricing_error,
        skipped_sources,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, usage: TokenUsage) -> UsageHistoryRecord {
        UsageHistoryRecord {
            message_id: id.into(),
            chat_id: "chat".into(),
            device_id: "device".into(),
            started_at: DAY_MS,
            harness: Some(HarnessId::Codex),
            model: Some("test".into()),
            usage,
        }
    }

    #[test]
    fn reasoning_is_not_charged_twice_and_reported_cost_takes_precedence() {
        let pricing = Pricing {
            fetched_at: None,
            rates: HashMap::from([(
                "test".into(),
                Rate {
                    input: 1.0,
                    output: 2.0,
                    cache_read: 0.1,
                    cache_write: 1.25,
                },
            )]),
        };
        let usage = TokenUsage {
            input_tokens: Some(100),
            cached_input_tokens: Some(50),
            cache_write_input_tokens: Some(10),
            output_tokens: Some(20),
            reasoning_output_tokens: Some(5),
            cost_usd: Some(7.0),
            ..Default::default()
        };
        let summary = aggregate(
            "device",
            &[record("one", usage)],
            &pricing,
            0,
            DAY_MS * 2,
            false,
            0,
            None,
            0,
        );
        assert_eq!(summary.totals.cost_usd(), 7.0);
        assert_eq!(summary.totals.estimated_cost_usd, 0.0);
        assert!((summary.totals.category_cost_usd.iter().sum::<f64>() - 7.0).abs() < 1e-9);
    }

    #[test]
    fn missing_cache_counts_do_not_produce_a_fake_estimate() {
        let pricing = Pricing {
            fetched_at: None,
            rates: HashMap::from([(
                "test".into(),
                Rate {
                    input: 1.0,
                    output: 2.0,
                    cache_read: 0.1,
                    cache_write: 1.25,
                },
            )]),
        };
        let mut acc = Accumulator::default();
        acc.add(
            &record(
                "one",
                TokenUsage {
                    input_tokens: Some(100),
                    output_tokens: Some(20),
                    ..Default::default()
                },
            ),
            &pricing,
        );
        assert_eq!(acc.totals.unpriced_turns, 1);
        assert_eq!(acc.totals.cost_usd(), 0.0);
    }

    #[test]
    fn ambiguous_qualified_models_are_not_aliased() {
        let rates = parse_rates(&serde_json::json!({
            "a/test": {"input_cost_per_token": 1, "output_cost_per_token": 2},
            "b/test": {"input_cost_per_token": 3, "output_cost_per_token": 4}
        }));
        assert!(!rates.contains_key("test"));
    }

    #[test]
    fn provider_sessions_are_not_counted_once_per_model() {
        let one = record(
            "one",
            TokenUsage {
                input_tokens: Some(100),
                output_tokens: Some(20),
                ..Default::default()
            },
        );
        let mut two = one.clone();
        two.message_id = "two".into();
        two.model = Some("other".into());
        let summary = aggregate(
            "device",
            &[one, two],
            &Pricing::default(),
            0,
            DAY_MS * 2,
            false,
            0,
            None,
            0,
        );
        assert_eq!(summary.models.len(), 2);
        assert_eq!(summary.providers[0].totals.sessions, 1);
        assert_eq!(summary.totals.turns, 2);
        assert_eq!(summary.totals.usage.total(), Some(240));
        assert_eq!(summary.totals.incomplete_cache_turns, 2);
        assert_eq!(summary.totals.uncached_input_tokens, None);
    }

    #[test]
    fn snapshot_continuations_replace_usage_and_do_not_double_count() {
        let raw = LoroDoc::new();
        let messages = raw.get_list("messages");
        for (index, id, output) in [(0, "root", 10), (1, "tail", 20)] {
            let map = messages
                .insert_container(index, loro::LoroMap::new())
                .unwrap();
            map.insert("id", id).unwrap();
            map.insert("role", "assistant").unwrap();
            map.insert("deviceId", "device").unwrap();
            map.insert("createdAt", DAY_MS + index as i64).unwrap();
            map.insert("status", "complete").unwrap();
            if index == 1 {
                map.insert("continuationOf", "root").unwrap();
            }
            map.insert(
                "tokenUsage",
                serde_json::to_string(&TokenUsage {
                    output_tokens: Some(output),
                    ..Default::default()
                })
                .unwrap(),
            )
            .unwrap();
        }
        let records = snapshot_records(&raw, "chat", "device");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].message_id, "root");
        assert_eq!(records[0].started_at, DAY_MS);
        assert_eq!(records[0].usage.output_tokens, Some(20));
        let Some(ValueOrContainer::Container(Container::Map(tail))) = messages.get(1) else {
            panic!("missing tail");
        };
        tail.insert("status", "streaming").unwrap();
        assert!(snapshot_records(&raw, "chat", "device").is_empty());
    }
}
