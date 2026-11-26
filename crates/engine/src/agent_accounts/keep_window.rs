//! Keep window: the engine starts a Claude Code or Codex account's 5-hour
//! window itself, so it is already counting down when the user comes back,
//! instead of starting at their first message.
//!
//! The window starts with the first request after the last one ran out, and
//! a usage probe does not start it. So once the probe shows no window
//! running (no 5-hour window, or one whose reset has passed), the engine
//! runs the agent's own CLI once with a one-line prompt on the cheapest model
//! at low effort, as that account: the live login as it is, a saved Claude
//! login through `CLAUDE_CODE_OAUTH_TOKEN` (as a pinned chat runs, see
//! [`super::pins`]), a saved Codex login from a throwaway `CODEX_HOME`.
//!
//! Off unless turned on, per account (`SetAgentKeepWindow`), kept in
//! `{data_dir}/agent-accounts/keep-window.json`. Guards:
//! - at most one ping per [`PING_GAP`], whatever happened;
//! - only on a fresh, successful probe (never blind on a failing one);
//! - a ping that worked but left no window running (a plan without a 5-hour
//!   window, Codex Pro) marks the account a dud: no more pings until a
//!   running window is seen again.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use zeron_harness::Harness as _;
use zeron_proto::{AgentAccount, AgentAccountsSnapshot, HarnessId};

use super::{AgentAccounts, ShredOnDrop, harness_slug, lock, private_dir, write_file_atomic};
use crate::{EngineError, new_id, now_ms};

/// `{data_dir}/agent-accounts/keep-window.json`.
pub(super) const KEEP_FILE: &str = "keep-window.json";
/// How often the windows are looked at while any account keeps its window.
const CHECK_EVERY: Duration = Duration::from_secs(5 * 60);
/// No second ping for an account within this.
const PING_GAP: Duration = Duration::from_secs(60 * 60);
/// A ping that hasn't answered by now is killed.
const PING_TIMEOUT: Duration = Duration::from_secs(3 * 60);
/// After pinging, the wait before probing again to see the window start
/// (past the probe's own minimum interval).
const SETTLE: Duration = Duration::from_secs(45);
/// A probe older than this is too old to ping on.
const FRESH: Duration = Duration::from_secs(10 * 60);
/// The 5-hour window's label (`zeron_proto::plan_usage`).
const SESSION: &str = "Session";

const PROMPT: &str = "Reply with just: ok";
const CLAUDE_MODEL: &str = "claude-haiku-5-5";
const CODEX_MODEL: &str = "gpt-6-luna";
const EFFORT: &str = "low";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeepEntry {
    /// Epoch millis of the last ping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_ping: Option<i64>,
    /// A ping that worked left no window running: none until one is seen.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    dud: bool,
}

/// The accounts that keep their window (`"{harness}:{account id}"`; listed
/// is on).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct KeepState {
    #[serde(default)]
    accounts: BTreeMap<String, KeepEntry>,
}

impl KeepState {
    pub(super) fn load(file: &Path) -> Self {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }
}

fn keep_key(harness: HarnessId, id: &str) -> String {
    format!("{}:{id}", harness_slug(harness))
}

fn keeps_harness(harness: HarnessId) -> bool {
    matches!(harness, HarnessId::ClaudeCode | HarnessId::Codex)
}

/// Whether `account`'s probe says no 5-hour window is running: its window's
/// reset has passed (or it names none), Codex reports zero session usage,
/// or there's no 5-hour window beside other windows. `None`: the probe
/// can't say (none yet, failing, or old).
fn window_idle(account: &AgentAccount, now: i64) -> Option<bool> {
    let fetched = account.usage_fetched_at?;
    if account.usage_error.is_some()
        || now - fetched > FRESH.as_millis() as i64
        || account.usage_windows.is_empty()
    {
        return None;
    }
    let now = Utc::now();
    Some(match account.usage_windows.iter().find(|w| w.label == SESSION) {
        Some(w) => {
            // Codex can report a future reset before the first request starts the window.
            (account.harness == HarnessId::Codex && w.used_fraction == 0.0)
                || w.resets_at.is_none_or(|at| at <= now)
        }
        None => true,
    })
}

impl AgentAccounts {
    /// Whether `harness`'s account `id` keeps its window.
    pub(super) fn keeps_window(&self, harness: HarnessId, id: &str) -> bool {
        keeps_harness(harness) && lock(&self.inner.keep).accounts.contains_key(&keep_key(harness, id))
    }

    fn persist_keep(&self, state: &KeepState) {
        let Ok(root) = self.inner.config.private_root() else { return };
        let Ok(bytes) = serde_json::to_vec_pretty(state) else { return };
        if let Err(err) = write_file_atomic(&root.join(KEEP_FILE), &bytes, false) {
            tracing::warn!(error = %err, "could not save the keep-window accounts");
        }
    }

    /// Turn keeping `harness`'s account `id`'s window on or off; answers the
    /// snapshot. Turned on, the windows are looked at right away.
    pub async fn set_keep_window(
        &self,
        harness: HarnessId,
        id: &str,
        enabled: bool,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        if !keeps_harness(harness) {
            return Err(EngineError::Other(
                "Only Claude Code and Codex accounts can keep their 5-hour window running.".into(),
            ));
        }
        if enabled && self.read_slot(harness, id).is_none() {
            return Err(EngineError::Other("That account isn't saved on this device.".into()));
        }
        let state = {
            let mut state = lock(&self.inner.keep);
            if enabled {
                // (Turned on again: a fresh start, no old dud or gap.)
                state.accounts.insert(keep_key(harness, id), KeepEntry::default());
            } else {
                state.accounts.remove(&keep_key(harness, id));
            }
            state.clone()
        };
        self.persist_keep(&state);
        if enabled {
            self.inner.keep_wake.notify_one();
        }
        self.list(false).await
    }

    /// Start looking after the accounts that keep their window: every
    /// [`CHECK_EVERY`], and as one is turned on. Call once.
    pub fn start_keep_window(&self) {
        let accounts = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(CHECK_EVERY) => {}
                    _ = accounts.inner.keep_wake.notified() => {}
                }
                accounts.keep_window_tick().await;
            }
        });
    }

    async fn keep_window_tick(&self) {
        if lock(&self.inner.keep).accounts.is_empty() {
            return;
        }
        let Ok(snapshot) = self.list(true).await else { return };
        let now = now_ms();
        let mut pinged: Vec<String> = Vec::new();
        for account in &snapshot.accounts {
            if !self.keeps_window(account.harness, &account.id) {
                continue;
            }
            let key = keep_key(account.harness, &account.id);
            let Some(idle) = window_idle(account, now) else { continue };
            let state = {
                let mut state = lock(&self.inner.keep);
                let Some(entry) = state.accounts.get_mut(&key) else { continue };
                if !idle {
                    // A window running: whatever a dud was, it has one now.
                    if !entry.dud {
                        continue;
                    }
                    entry.dud = false;
                } else {
                    if entry.dud
                        || entry.last_ping.is_some_and(|at| now - at < PING_GAP.as_millis() as i64)
                    {
                        continue;
                    }
                    entry.last_ping = Some(now);
                }
                state.clone()
            };
            self.persist_keep(&state);
            if !idle {
                continue;
            }
            match self.ping(account).await {
                Ok(()) => {
                    tracing::info!(harness = harness_slug(account.harness), "keep window: pinged");
                    pinged.push(key);
                }
                Err(err) => {
                    tracing::warn!(harness = harness_slug(account.harness), error = %err, "keep window: ping failed");
                }
            }
        }
        if pinged.is_empty() {
            return;
        }
        // See whether the pings started a window; one that didn't won't.
        tokio::time::sleep(SETTLE).await;
        let Ok(snapshot) = self.list(true).await else { return };
        let now = now_ms();
        let mut state = lock(&self.inner.keep);
        let mut changed = false;
        for account in &snapshot.accounts {
            let key = keep_key(account.harness, &account.id);
            if pinged.contains(&key)
                && window_idle(account, now) == Some(true)
                && let Some(entry) = state.accounts.get_mut(&key)
            {
                tracing::info!(harness = harness_slug(account.harness), "keep window: no window started; holding off");
                entry.dud = true;
                changed = true;
            }
        }
        let state = state.clone();
        if changed {
            self.persist_keep(&state);
        }
    }

    /// One minimal turn of `account`'s agent, as that account.
    async fn ping(&self, account: &AgentAccount) -> Result<(), String> {
        let root = self.inner.config.private_root().map_err(|e| e.to_string())?;
        let dir = root.join(".keep-window");
        private_dir(&dir).map_err(|e| e.to_string())?;
        match account.harness {
            HarnessId::ClaudeCode => self.ping_claude(account, &dir).await,
            HarnessId::Codex => self.ping_codex(account, &dir, &root).await,
            _ => Err("not an agent that keeps a window".into()),
        }
    }

    async fn ping_claude(&self, account: &AgentAccount, dir: &Path) -> Result<(), String> {
        let exe = zeron_harness::ClaudeHarness::new()
            .executable_path()
            .ok_or("the claude CLI was not found")?;
        let mut cmd = zeron_harness::process::Command::new(&exe);
        zeron_harness::compose_child_path(&mut cmd, &exe);
        cmd.args([
            "-p",
            PROMPT,
            "--model",
            CLAUDE_MODEL,
            "--effort",
            EFFORT,
            "--tools",
            "",
            "--no-session-persistence",
        ]);
        // A saved login other than the live one runs on its own token (the
        // live login runs as it is). Never the live login in its place.
        match self.pin_run(HarnessId::ClaudeCode, &account.id).await? {
            Some(run) => {
                for (key, value) in run.env {
                    cmd.env(key, value);
                }
            }
            None if !account.active => return Err("the saved login can't be used".into()),
            None => {}
        }
        run(&mut cmd, dir).await
    }

    async fn ping_codex(&self, account: &AgentAccount, dir: &Path, root: &Path) -> Result<(), String> {
        let exe = zeron_harness::CodexHarness::new()
            .executable_path()
            .ok_or("the codex CLI was not found")?;
        let mut cmd = zeron_harness::process::Command::new(&exe);
        zeron_harness::compose_child_path(&mut cmd, &exe);
        let effort = format!("model_reasoning_effort=\"{EFFORT}\"");
        cmd.args(["exec", "-m", CODEX_MODEL, "-c", &effort, "--skip-git-repo-check", "--ephemeral", "-s", "read-only", "-C"])
            .arg(dir)
            .arg(PROMPT);
        if account.active {
            cmd.env("CODEX_HOME", &self.inner.config.codex_home);
            return run(&mut cmd, dir).await;
        }
        // A saved login: its `auth.json` in a throwaway home (named so the
        // startup sweep reclaims it after a crash), and whatever Codex
        // refreshed there saved back, so the slot's refresh token stays the
        // one that works.
        let slot = self
            .read_slot(HarnessId::Codex, &account.id)
            .ok_or("the saved login is gone")?;
        let home = root.join(format!(".login-keep-{}", new_id()));
        private_dir(&home).map_err(|e| e.to_string())?;
        let _shred = ShredOnDrop(home.clone());
        let auth = serde_json::to_vec_pretty(&slot.credentials).map_err(|e| e.to_string())?;
        write_file_atomic(&home.join("auth.json"), &auth, true).map_err(|e| e.to_string())?;
        cmd.env("CODEX_HOME", &home);
        let ran = run(&mut cmd, dir).await;
        let after = std::fs::read_to_string(home.join("auth.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
        if let Some(after) = after
            && after != slot.credentials
        {
            let _ops = self.inner.ops.lock().await;
            if let Some(mut now) = self.read_slot(HarnessId::Codex, &account.id) {
                now.credentials = after;
                if let Err(err) = self.write_slot(&now) {
                    tracing::warn!(error = %err, "keep window: could not save Codex's refreshed login");
                }
            }
        }
        ran
    }
}

/// Run a ping to its end (killed past [`PING_TIMEOUT`]); its output is
/// never kept, only whether it worked.
async fn run(cmd: &mut zeron_harness::process::Command, dir: &Path) -> Result<(), String> {
    cmd.current_dir(dir)
        .stdin(zeron_harness::process::Stdio::null())
        .stdout(zeron_harness::process::Stdio::null())
        .stderr(zeron_harness::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(PING_TIMEOUT, cmd.output())
        .await
        .map_err(|_| "timed out".to_string())?
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("exited with {}", output.status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::AgentUsageWindow;

    fn account(windows: Vec<AgentUsageWindow>) -> AgentAccount {
        let mut a: AgentAccount = serde_json::from_value(serde_json::json!({
            "id": "a", "harness": "claude-code", "email": null, "planLabel": null,
            "active": true, "switchable": true,
        }))
        .unwrap();
        a.usage_windows = windows;
        a.usage_fetched_at = Some(now_ms());
        a
    }

    fn window(label: &str, resets_in_secs: Option<i64>) -> AgentUsageWindow {
        AgentUsageWindow {
            label: label.into(),
            used_fraction: 0.1,
            resets_at: resets_in_secs.map(|s| Utc::now() + chrono::Duration::seconds(s)),
        }
    }

    #[test]
    fn a_running_window_is_not_idle() {
        let a = account(vec![window(SESSION, Some(3600)), window("Week", Some(86_400))]);
        assert_eq!(window_idle(&a, now_ms()), Some(false));
    }

    #[test]
    fn only_codex_treats_zero_usage_with_a_future_reset_as_idle() {
        let mut a = account(vec![window(SESSION, Some(18_000))]);
        a.usage_windows[0].used_fraction = 0.0;
        assert_eq!(window_idle(&a, now_ms()), Some(false));

        a.harness = HarnessId::Codex;
        assert_eq!(window_idle(&a, now_ms()), Some(true));

        a.usage_windows[0].used_fraction = 0.01;
        assert_eq!(window_idle(&a, now_ms()), Some(false));
    }

    #[test]
    fn a_window_past_its_reset_or_without_one_is_idle() {
        assert_eq!(window_idle(&account(vec![window(SESSION, Some(-60))]), now_ms()), Some(true));
        assert_eq!(window_idle(&account(vec![window(SESSION, None)]), now_ms()), Some(true));
        assert_eq!(window_idle(&account(vec![window("Week", Some(86_400))]), now_ms()), Some(true));
    }

    #[test]
    fn no_fresh_probe_says_nothing() {
        assert_eq!(window_idle(&account(vec![]), now_ms()), None);
        let mut a = account(vec![window(SESSION, Some(-60))]);
        a.usage_error = Some("Rate limited".into());
        assert_eq!(window_idle(&a, now_ms()), None);
        let mut a = account(vec![window(SESSION, Some(-60))]);
        a.usage_fetched_at = Some(now_ms() - FRESH.as_millis() as i64 - 1);
        assert_eq!(window_idle(&a, now_ms()), None);
    }
}
