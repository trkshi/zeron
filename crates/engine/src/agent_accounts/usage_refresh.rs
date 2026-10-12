//! Host-owned quota refreshes, coalesced across turns and connected clients.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, watch};
use zeron_proto::{AgentAccountsSnapshot, AgentEvent, HarnessId};

use super::{AgentAccounts, FORCED_MIN_INTERVAL, UsageProbeScope, lock};

const FALLBACK_INTERVAL: Duration = Duration::from_secs(5 * 60);
const PROVIDERS: [HarnessId; 8] = [
    HarnessId::ClaudeCode,
    HarnessId::Codex,
    HarnessId::Cursor,
    HarnessId::Grok,
    HarnessId::Devin,
    HarnessId::Opencode,
    HarnessId::Pi,
    HarnessId::Hermes,
];

struct Pending {
    revision: u64,
    dirty: bool,
    login_changed: bool,
    due: Instant,
    fallback: Instant,
}

#[derive(Default)]
struct Schedule {
    pending: HashMap<HarnessId, Pending>,
    runs: HashMap<String, HarnessId>,
}

impl Schedule {
    fn mark(&mut self, harness: HarnessId, now: Instant) {
        let pending = self.pending.entry(harness).or_insert(Pending {
            revision: 0,
            dirty: false,
            login_changed: false,
            due: now,
            fallback: now + FALLBACK_INTERVAL,
        });
        pending.revision = pending.revision.wrapping_add(1);
        // A stream of activity must not postpone an already queued refresh.
        if !pending.dirty {
            pending.dirty = true;
            pending.due = now;
        } else {
            // One keyed provider can have both eligible and backed-off logins.
            // Recheck eligibility after new activity without bypassing either gate.
            pending.due = pending.due.min(now + FORCED_MIN_INTERVAL);
        }
    }

    fn account_changed(&mut self, harness: HarnessId, now: Instant) {
        self.mark(harness, now);
        let pending = self.pending.get_mut(&harness).expect("marked above");
        pending.due = now;
        pending.login_changed = true;
    }

    fn work(&mut self, watched: bool, now: Instant) -> (Vec<(HarnessId, u64)>, Option<Instant>) {
        let demand: HashSet<_> = self
            .runs
            .values()
            .copied()
            .chain(PROVIDERS.into_iter().filter(|_| watched))
            .collect();
        self.pending
            .retain(|harness, pending| pending.dirty || demand.contains(harness));
        for harness in &demand {
            self.pending.entry(*harness).or_insert(Pending {
                revision: 0,
                dirty: false,
                login_changed: false,
                due: now,
                fallback: now + FALLBACK_INTERVAL,
            });
        }
        let mut work = Vec::new();
        let mut next: Option<Instant> = None;
        for (harness, pending) in &mut self.pending {
            if demand.contains(harness) && pending.fallback <= now {
                pending.dirty = true;
                pending.due = now;
                pending.revision = pending.revision.wrapping_add(1);
            }
            let deadline = if pending.dirty {
                if demand.contains(harness) {
                    pending.due.min(pending.fallback)
                } else {
                    pending.due
                }
            } else {
                pending.fallback
            };
            if pending.dirty && deadline <= now {
                pending.login_changed = false;
                work.push((*harness, pending.revision));
            } else {
                next = Some(next.map_or(deadline, |next| next.min(deadline)));
            }
        }
        (work, next)
    }

    fn finish(
        &mut self,
        work: &[(HarnessId, u64)],
        retries: &HashMap<HarnessId, Duration>,
        now: Instant,
    ) {
        for (harness, revision) in work {
            let Some(pending) = self.pending.get_mut(harness) else {
                continue;
            };
            pending.fallback = now + FALLBACK_INTERVAL;
            // A login switched during this probe needs its own eligibility check.
            if pending.login_changed {
                pending.dirty = true;
                pending.due = now;
                continue;
            }
            let more_activity = pending.revision != *revision;
            pending.dirty = more_activity || retries.contains_key(harness);
            if pending.dirty {
                let retry = retries.get(harness).copied().unwrap_or(FORCED_MIN_INTERVAL);
                pending.due = now
                    + if more_activity {
                        retry.min(FORCED_MIN_INTERVAL)
                    } else {
                        retry
                    };
            }
        }
    }
}

pub(super) struct UsageRefresh {
    schedule: Mutex<Schedule>,
    wake: Arc<Notify>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub(super) updates: watch::Sender<AgentAccountsSnapshot>,
}

impl Default for UsageRefresh {
    fn default() -> Self {
        let (updates, _) = watch::channel(AgentAccountsSnapshot::default());
        Self {
            schedule: Mutex::new(Schedule::default()),
            wake: Arc::new(Notify::new()),
            worker: Mutex::new(None),
            updates,
        }
    }
}

impl UsageRefresh {
    pub(super) fn shutdown(&self) {
        if let Some(worker) = lock(&self.worker).take() {
            worker.abort();
        }
        *lock(&self.schedule) = Schedule::default();
    }
}

impl Drop for UsageRefresh {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl AgentAccounts {
    pub(crate) fn start_usage_refresh(&self) {
        let mut worker = lock(&self.inner.refresh.worker);
        if worker.is_some() {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let wake = self.inner.refresh.wake.clone();
        *worker = Some(tokio::spawn(async move {
            loop {
                let Some(inner) = weak.upgrade() else { return };
                let (work, next) = lock(&inner.refresh.schedule)
                    .work(inner.refresh.updates.receiver_count() > 0, Instant::now());
                if !work.is_empty() {
                    let accounts = AgentAccounts { inner };
                    let providers = work.iter().map(|(harness, _)| *harness).collect();
                    let result = {
                        let _ops = accounts.inner.ops.lock().await;
                        accounts
                            .list_locked_with_usage_schedule(
                                true,
                                UsageProbeScope::ActiveHarnesses(providers),
                            )
                            .await
                    };
                    let retries = match result {
                        Ok((_, retries)) => retries,
                        Err(error) => {
                            tracing::warn!(%error, "scheduled account usage refresh failed");
                            work.iter()
                                .map(|(harness, _)| (*harness, FORCED_MIN_INTERVAL))
                                .collect()
                        }
                    };
                    lock(&accounts.inner.refresh.schedule).finish(&work, &retries, Instant::now());
                    continue;
                }
                // Never retain the engine graph while waiting for new work.
                drop(inner);
                match next {
                    Some(at) => tokio::select! {
                        _ = wake.notified() => {},
                        _ = tokio::time::sleep(at.saturating_duration_since(Instant::now())) => {},
                    },
                    None => wake.notified().await,
                }
            }
        }));
    }

    fn request_usage_refresh(&self, harness: HarnessId) {
        if !PROVIDERS.contains(&harness) {
            return;
        }
        lock(&self.inner.refresh.schedule).mark(harness, Instant::now());
        self.inner.refresh.wake.notify_one();
    }

    pub(super) fn usage_account_changed(&self, harness: HarnessId) {
        if !PROVIDERS.contains(&harness) {
            return;
        }
        // Another account must not inherit the previous login's backoff.
        lock(&self.inner.refresh.schedule).account_changed(harness, Instant::now());
        self.inner.refresh.wake.notify_one();
    }

    pub(crate) async fn watch_usage(
        &self,
    ) -> Result<watch::Receiver<AgentAccountsSnapshot>, crate::EngineError> {
        self.start_usage_refresh();
        let first = self.inner.refresh.updates.receiver_count() == 0;
        let rx = self.inner.refresh.updates.subscribe();
        self.list(false).await?;
        if first {
            for harness in PROVIDERS {
                self.request_usage_refresh(harness);
            }
        }
        self.inner.refresh.wake.notify_one();
        Ok(rx)
    }

    pub(crate) fn usage_run(&self, run_id: String, harness: HarnessId, active: bool) -> UsageRun {
        let mut run = UsageRun {
            accounts: self.clone(),
            run_id,
            harness,
            active: false,
            background: false,
            thinking: false,
            reported: None,
        };
        if active {
            run.resume();
        }
        run
    }
}

/// Retires refresh demand even on startup failure, cancellation, or an early return.
pub(crate) struct UsageRun {
    accounts: AgentAccounts,
    run_id: String,
    harness: HarnessId,
    active: bool,
    background: bool,
    thinking: bool,
    reported: Option<(u64, u64)>,
}

impl UsageRun {
    pub(crate) fn resume(&mut self) {
        if !self.active && PROVIDERS.contains(&self.harness) {
            self.active = true;
            self.thinking = false;
            self.reported = None;
            self.sync_demand();
            self.accounts.request_usage_refresh(self.harness);
        }
    }

    pub(crate) fn changed(&self) {
        if self.active || self.background {
            self.accounts.request_usage_refresh(self.harness);
        }
    }

    pub(crate) fn observe(&mut self, event: &AgentEvent) {
        if !self.active && !self.background {
            return;
        }
        let thinking_finished = self.thinking
            && matches!(
                event,
                AgentEvent::TextDelta { .. } | AgentEvent::ToolCall { .. }
            );
        if thinking_finished {
            self.thinking = false;
            self.changed();
        }
        match event {
            AgentEvent::ReasoningDelta { text } if !text.is_empty() => self.thinking = true,
            AgentEvent::ToolResult { id, .. } if id != zeron_proto::LIVE_PLAN_TOOL_ID => {
                self.changed()
            }
            AgentEvent::AssistantMessageCompleted { .. } | AgentEvent::InputRequested { .. } => {
                self.changed()
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                let next = (*input_tokens, *output_tokens);
                if self.reported != Some(next) {
                    self.reported = Some(next);
                    self.changed();
                }
            }
            AgentEvent::Steered { .. } => {
                self.thinking = false;
                self.reported = None;
                self.changed();
            }
            _ => {}
        }
    }

    pub(crate) fn pause(&mut self) {
        if self.active {
            self.changed();
            self.active = false;
            self.sync_demand();
        }
    }

    pub(crate) fn background(&mut self, running: bool) {
        if self.background != running && PROVIDERS.contains(&self.harness) {
            self.changed();
            self.background = running;
            self.sync_demand();
        }
    }

    fn sync_demand(&self) {
        let mut schedule = lock(&self.accounts.inner.refresh.schedule);
        if self.active || self.background {
            schedule.runs.insert(self.run_id.clone(), self.harness);
        } else {
            schedule.runs.remove(&self.run_id);
        }
        self.accounts.inner.refresh.wake.notify_one();
    }
}

impl Drop for UsageRun {
    fn drop(&mut self) {
        self.pause();
        self.background(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn more_events_join_the_existing_cooldown_without_postponing_it() {
        let mut schedule = Schedule::default();
        let now = Instant::now();
        schedule.mark(HarnessId::Codex, now);
        let work = schedule.work(false, now).0;
        schedule.finish(
            &work,
            &HashMap::from([(HarnessId::Codex, FORCED_MIN_INTERVAL)]),
            now,
        );
        schedule.mark(HarnessId::Codex, now + Duration::from_secs(10));
        schedule.mark(HarnessId::Codex, now + Duration::from_secs(20));
        assert!(
            schedule
                .work(false, now + Duration::from_secs(29))
                .0
                .is_empty()
        );
        assert_eq!(schedule.work(false, now + FORCED_MIN_INTERVAL).0.len(), 1);
    }

    #[test]
    fn events_arriving_during_a_probe_get_one_trailing_refresh() {
        let mut schedule = Schedule::default();
        let now = Instant::now();
        schedule.mark(HarnessId::Codex, now);
        let work = schedule.work(false, now).0;
        schedule.mark(HarnessId::Codex, now);
        schedule.finish(&work, &HashMap::new(), now);
        assert!(schedule.work(false, now).0.is_empty());
        assert_eq!(schedule.work(false, now + FORCED_MIN_INTERVAL).0.len(), 1);
    }

    #[test]
    fn an_account_switch_during_a_probe_does_not_inherit_its_backoff() {
        let mut schedule = Schedule::default();
        let now = Instant::now();
        schedule.mark(HarnessId::Codex, now);
        let work = schedule.work(false, now).0;
        schedule.account_changed(HarnessId::Codex, now);
        schedule.finish(
            &work,
            &HashMap::from([(HarnessId::Codex, Duration::from_secs(600))]),
            now,
        );
        let switched = schedule.work(false, now).0;
        assert_eq!(switched.len(), 1);
        schedule.finish(&switched, &HashMap::new(), now);
        assert!(schedule.work(false, now).0.is_empty());
    }

    #[test]
    fn new_activity_rechecks_eligible_logins_despite_a_siblings_backoff() {
        let mut schedule = Schedule::default();
        let now = Instant::now();
        schedule.mark(HarnessId::Opencode, now);
        let work = schedule.work(false, now).0;
        schedule.finish(
            &work,
            &HashMap::from([(HarnessId::Opencode, Duration::from_secs(600))]),
            now,
        );
        schedule.mark(HarnessId::Opencode, now + Duration::from_secs(10));
        assert!(
            schedule
                .work(false, now + Duration::from_secs(39))
                .0
                .is_empty()
        );
        assert_eq!(
            schedule.work(false, now + Duration::from_secs(40)).0.len(),
            1
        );
    }

    #[test]
    fn silent_logins_keep_the_fallback_when_a_sibling_is_backed_off() {
        let mut schedule = Schedule::default();
        let now = Instant::now();
        schedule.runs.insert("run".into(), HarnessId::Opencode);
        schedule.mark(HarnessId::Opencode, now);
        let work = schedule.work(false, now).0;
        schedule.finish(
            &work,
            &HashMap::from([(HarnessId::Opencode, Duration::from_secs(600))]),
            now,
        );
        assert_eq!(schedule.work(false, now).1, Some(now + FALLBACK_INTERVAL));
        assert_eq!(schedule.work(false, now + FALLBACK_INTERVAL).0.len(), 1);
    }

    #[test]
    fn silent_turns_share_one_fallback_and_idle_hosts_stop() {
        let mut schedule = Schedule::default();
        let now = Instant::now();
        schedule.runs.insert("one".into(), HarnessId::Codex);
        schedule.runs.insert("two".into(), HarnessId::Codex);
        assert!(schedule.work(false, now).0.is_empty());
        let work = schedule.work(false, now + FALLBACK_INTERVAL).0;
        assert_eq!(work.len(), 1);
        schedule.finish(&work, &HashMap::new(), now + FALLBACK_INTERVAL);
        schedule.runs.clear();
        assert!(
            schedule
                .work(false, now + FALLBACK_INTERVAL * 2)
                .1
                .is_none()
        );
        assert!(schedule.pending.is_empty());
    }

    #[test]
    fn reasoning_chunks_and_duplicate_reports_do_not_refresh_usage() {
        let root = tempfile::tempdir().unwrap();
        let accounts = AgentAccounts::new(super::super::AgentAccountsConfig::isolated(root.path()));
        let mut run = accounts.usage_run("run".into(), HarnessId::Codex, true);
        let revision = |accounts: &AgentAccounts| {
            lock(&accounts.inner.refresh.schedule).pending[&HarnessId::Codex].revision
        };
        let initial = revision(&accounts);
        run.observe(&AgentEvent::ReasoningDelta {
            text: "thinking".into(),
        });
        run.observe(&AgentEvent::ReasoningDelta { text: "".into() });
        assert_eq!(revision(&accounts), initial);
        run.observe(&AgentEvent::TextDelta {
            text: "reply".into(),
        });
        assert_eq!(revision(&accounts), initial + 1);
        let report = AgentEvent::Usage {
            input_tokens: 100,
            output_tokens: 20,
        };
        run.observe(&report);
        let reported = revision(&accounts);
        run.observe(&report);
        assert_eq!(revision(&accounts), reported);
        run.pause();
        let paused = revision(&accounts);
        run.observe(&AgentEvent::ToolResult {
            id: "late".into(),
            is_error: false,
            output: None,
            diff: None,
        });
        assert_eq!(revision(&accounts), paused);
        assert!(lock(&accounts.inner.refresh.schedule).runs.is_empty());
    }

    #[test]
    fn background_children_keep_fallback_demand_after_the_parent_finishes() {
        let root = tempfile::tempdir().unwrap();
        let accounts = AgentAccounts::new(super::super::AgentAccountsConfig::isolated(root.path()));
        let mut run = accounts.usage_run("run".into(), HarnessId::Codex, true);
        run.background(true);
        run.pause();
        assert_eq!(lock(&accounts.inner.refresh.schedule).runs.len(), 1);
        drop(run);
        assert!(lock(&accounts.inner.refresh.schedule).runs.is_empty());
    }
}
