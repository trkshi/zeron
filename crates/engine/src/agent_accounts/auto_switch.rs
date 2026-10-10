//! Quota-driven account rotation. Planning never changes the live login;
//! sessions retire only idle runtimes before applying a revalidated plan.

use super::*;

const MAX_USAGE_AGE_MS: i64 = 60_000;

pub(crate) struct AutoSwitchPlan {
    pub harness: HarnessId,
    from: String,
    to: String,
}

fn quota_fraction(account: &AgentAccount, now: i64) -> Option<f32> {
    let age = now.checked_sub(account.usage_fetched_at?)?;
    if !(0..=MAX_USAGE_AGE_MS).contains(&age)
        || account.usage_error.is_some()
        || account.auth_kind != Some(AgentAuthKind::Oauth)
        || !account.switchable
    {
        return None;
    }
    if account.usage_windows.iter().any(|window| {
        matches!(window.label.as_str(), "Workspace limit" | "Usage limit")
            && window.used_fraction == 1.0
            && window.resets_at.is_none()
    }) {
        return Some(1.0);
    }
    let mut fraction: Option<f32> = None;
    for window in &account.usage_windows {
        if !matches!(window.label.as_str(), "Session" | "Week" | "Weekly") {
            continue;
        }
        if !window.used_fraction.is_finite()
            || window.used_fraction < 0.0
            || window
                .resets_at
                .is_some_and(|reset| reset.timestamp_millis() <= now)
        {
            return None;
        }
        fraction = Some(fraction.unwrap_or(0.0).max(window.used_fraction));
    }
    fraction
}

fn exhausted_account(
    snapshot: &AgentAccountsSnapshot,
    harness: HarnessId,
    now: i64,
) -> Option<&AgentAccount> {
    snapshot.accounts.iter().find(|account| {
        account.harness == harness
            && account.active
            && quota_fraction(account, now).is_some_and(|fraction| fraction >= 1.0)
    })
}

fn switch_plan(
    snapshot: &AgentAccountsSnapshot,
    harness: HarnessId,
    now: i64,
) -> Option<AutoSwitchPlan> {
    let active = exhausted_account(snapshot, harness, now)?;
    let replacement = snapshot
        .accounts
        .iter()
        .filter(|account| account.harness == harness && !account.active && account.id != active.id)
        .filter_map(|account| {
            let fraction = quota_fraction(account, now)?;
            (fraction < 1.0).then_some((account, fraction))
        })
        .min_by(|(_, a), (_, b)| a.total_cmp(b))?
        .0;
    Some(AutoSwitchPlan {
        harness,
        from: active.id.clone(),
        to: replacement.id.clone(),
    })
}

impl AgentAccounts {
    pub(crate) async fn plan_auto_switch(
        &self,
        harness: HarnessId,
        refresh_active: bool,
    ) -> Result<Option<AutoSwitchPlan>, EngineError> {
        if !matches!(harness, HarnessId::Codex | HarnessId::ClaudeCode) {
            return Ok(None);
        }
        let _ops = self.inner.ops.lock().await;
        let mut snapshot = self.list_locked(false).await?;
        let fresh = snapshot.accounts.iter().any(|account| {
            account.harness == harness
                && account.active
                && quota_fraction(account, now_ms()).is_some()
        });
        if refresh_active || !fresh {
            snapshot = self
                .list_locked_with_usage_scope(true, UsageProbeScope::ActiveHarness(harness))
                .await?;
        }
        if exhausted_account(&snapshot, harness, now_ms()).is_none() {
            return Ok(None);
        }
        // Only an exhausted active login warrants probing alternatives.
        // Keep the complete slot list so scoped probes cannot prune other caches.
        snapshot = self
            .list_locked_with_usage_scope(true, UsageProbeScope::Harness(harness))
            .await?;
        Ok(switch_plan(&snapshot, harness, now_ms()))
    }

    pub(crate) async fn apply_auto_switch(
        &self,
        plan: &AutoSwitchPlan,
    ) -> Result<bool, EngineError> {
        let _ops = self.inner.ops.lock().await;
        let snapshot = self.list_locked(false).await?;
        // A manual switch, removal, reset, or failed probe during retirement
        // takes precedence over the earlier plan.
        let still_exhausted = exhausted_account(&snapshot, plan.harness, now_ms())
            .is_some_and(|account| account.id == plan.from);
        let still_available = snapshot.accounts.iter().any(|account| {
            account.harness == plan.harness
                && account.id == plan.to
                && !account.active
                && quota_fraction(account, now_ms()).is_some_and(|fraction| fraction < 1.0)
        });
        if !still_exhausted || !still_available {
            return Ok(false);
        }
        let snapshot = self.activate_locked(plan.harness, &plan.to).await?;
        if !snapshot.accounts.iter().any(|account| {
            account.harness == plan.harness && account.id == plan.to && account.active
        }) {
            return Err(EngineError::Other(
                "Automatic account switch did not select the saved login".into(),
            ));
        }
        tracing::info!(provider = ?plan.harness, "automatically switched exhausted account");
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(
        id: &str,
        harness: HarnessId,
        active: bool,
        session: f32,
        weekly: f32,
    ) -> AgentAccount {
        AgentAccount {
            id: id.into(),
            harness,
            email: None,
            plan_label: None,
            active,
            usage_windows: vec![
                AgentUsageWindow {
                    label: "Session".into(),
                    used_fraction: session,
                    resets_at: None,
                },
                AgentUsageWindow {
                    label: "Week".into(),
                    used_fraction: weekly,
                    resets_at: None,
                },
            ],
            usage_fetched_at: Some(100_000),
            usage_error: None,
            display_name: None,
            organization: None,
            auth_kind: Some(AgentAuthKind::Oauth),
            switchable: true,
            saved_at: None,
            provider: None,
        }
    }

    #[test]
    fn either_quota_limit_selects_the_account_with_the_most_headroom() {
        for harness in [HarnessId::Codex, HarnessId::ClaudeCode] {
            for (session, week) in [(1.0, 0.1), (0.1, 1.0)] {
                let snapshot = AgentAccountsSnapshot {
                    accounts: vec![
                        account("active", harness, true, session, week),
                        account("less-room", harness, false, 0.8, 0.1),
                        account("more-room", harness, false, 0.2, 0.3),
                    ],
                    warnings: vec![],
                };
                let plan = switch_plan(&snapshot, harness, 100_001).unwrap();
                assert_eq!(plan.from, "active");
                assert_eq!(plan.to, "more-room");
                let mut available = snapshot;
                available.accounts[0].usage_windows[0].used_fraction = 0.99;
                available.accounts[0].usage_windows[1].used_fraction = 0.99;
                assert!(switch_plan(&available, harness, 100_001).is_none());
            }
        }
    }

    #[test]
    fn workspace_limits_rotate_below_full_time_windows_and_exclude_blocked_replacements() {
        let harness = HarnessId::Codex;
        for label in ["Workspace limit", "Usage limit"] {
            let mut active = account("active", harness, true, 0.1, 0.2);
            // A stale window reset cannot clear a separately reported limit.
            active.usage_windows[0].resets_at = DateTime::from_timestamp_millis(90_000);
            active.usage_windows.push(AgentUsageWindow {
                label: label.into(),
                used_fraction: 1.0,
                resets_at: None,
            });
            let mut blocked = account("blocked", harness, false, 0.0, 0.0);
            blocked
                .usage_windows
                .push(active.usage_windows.last().unwrap().clone());
            let mut snapshot = AgentAccountsSnapshot {
                accounts: vec![
                    active,
                    blocked,
                    account("available", harness, false, 0.2, 0.3),
                ],
                warnings: vec![],
            };
            let plan = switch_plan(&snapshot, harness, 100_001).unwrap();
            assert_eq!(plan.from, "active");
            assert_eq!(plan.to, "available");
            snapshot.accounts.pop();
            assert!(switch_plan(&snapshot, harness, 100_001).is_none());
            assert!(switch_plan(&snapshot, harness, 160_001).is_none());
        }
    }

    #[test]
    fn exhausted_unknown_expired_and_other_provider_accounts_are_not_candidates() {
        let harness = HarnessId::Codex;
        let mut expired = account("expired", harness, false, 0.1, 0.1);
        expired.usage_error = Some("Sign in again".into());
        let mut stale = account("stale", harness, false, 0.1, 0.1);
        stale.usage_fetched_at = Some(1);
        let mut unknown = account("unknown", harness, false, 0.1, 0.1);
        unknown.usage_windows.clear();
        let mut api_key = account("key", harness, false, 0.1, 0.1);
        api_key.auth_kind = Some(AgentAuthKind::ApiKey);
        let mut reset = account("reset", harness, false, 0.1, 0.1);
        reset.usage_windows[0].resets_at = DateTime::from_timestamp_millis(90_000);
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![
                account("active", harness, true, 1.0, 0.0),
                account("also-full", harness, false, 0.1, 1.0),
                account("claude", HarnessId::ClaudeCode, false, 0.1, 0.1),
                expired,
                stale,
                unknown,
                api_key,
                reset,
            ],
            warnings: vec![],
        };
        assert!(switch_plan(&snapshot, harness, 100_001).is_none());
        assert!(switch_plan(&snapshot, harness, 160_001).is_none());
    }

    fn install_codex_login(
        accounts: &AgentAccounts,
        config: &AgentAccountsConfig,
        key: &str,
        session: f32,
        week: f32,
    ) -> String {
        let claims = serde_json::json!({
            "email": format!("{key}@example.com"),
            "https://api.openai.com/auth": { "chatgpt_account_id": key },
        });
        let id_token = format!(
            "e30.{}.sig",
            BASE64_URL.encode(serde_json::to_vec(&claims).unwrap())
        );
        let credentials = serde_json::json!({"tokens": {
            "id_token": id_token,
            "access_token": "test-access",
            "refresh_token": "test-refresh",
            "account_id": key,
        }});
        std::fs::create_dir_all(&config.codex_home).unwrap();
        write_file_atomic(
            &config.codex_auth_file(),
            &serde_json::to_vec(&credentials).unwrap(),
            true,
        )
        .unwrap();
        let detected = accounts.detect_codex().unwrap();
        accounts
            .snapshot_detected(HarnessId::Codex, &detected)
            .unwrap();
        let id = slot_id_for(HarnessId::Codex, &detected.account_key);
        let mut usage = UsageEntry::default();
        usage.record(
            Ok(UsageSnapshot {
                windows: account(&id, HarnessId::Codex, true, session, week).usage_windows,
                plan_label: None,
            }),
            credentials_fingerprint(&credentials),
            now_ms(),
        );
        lock(&accounts.inner.usage)
            .insert(usage_key(HarnessId::Codex, &detected.account_key), usage);
        id
    }

    #[tokio::test]
    async fn planning_keeps_the_login_and_applying_preserves_both_saved_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let config = AgentAccountsConfig::isolated(dir.path());
        let accounts = AgentAccounts::new(config.clone());
        let alternate = install_codex_login(&accounts, &config, "available", 0.2, 0.3);
        let active = install_codex_login(&accounts, &config, "full", 0.1, 1.0);
        let before = std::fs::read(config.codex_auth_file()).unwrap();
        let plan = accounts
            .plan_auto_switch(HarnessId::Codex, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(plan.from, active);
        assert_eq!(plan.to, alternate);
        assert_eq!(std::fs::read(config.codex_auth_file()).unwrap(), before);
        assert!(accounts.apply_auto_switch(&plan).await.unwrap());
        assert_eq!(accounts.detect_codex().unwrap().account_key, "available");
        assert_eq!(accounts.read_slots(HarnessId::Codex).len(), 2);
    }

    #[tokio::test]
    async fn a_manual_account_change_invalidates_an_automatic_switch_plan() {
        let dir = tempfile::tempdir().unwrap();
        let config = AgentAccountsConfig::isolated(dir.path());
        let accounts = AgentAccounts::new(config.clone());
        install_codex_login(&accounts, &config, "available", 0.2, 0.3);
        install_codex_login(&accounts, &config, "full", 1.0, 0.1);
        let plan = accounts
            .plan_auto_switch(HarnessId::Codex, false)
            .await
            .unwrap()
            .unwrap();
        install_codex_login(&accounts, &config, "manual", 0.2, 0.3);
        assert!(!accounts.apply_auto_switch(&plan).await.unwrap());
        assert_eq!(accounts.detect_codex().unwrap().account_key, "manual");
    }
}
