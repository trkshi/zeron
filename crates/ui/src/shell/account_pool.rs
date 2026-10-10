//! Home's read-only account pool. Snapshots share the Accounts/footer cache;
//! provider probes run only while Home is visible in an active window.

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Context, Entity, Render, SharedString, Subscription, Task, Window, div, prelude::*,
    px,
};
use zeron_engine::registry::AccountAutoSwitchPrefs;
use zeron_proto::{
    AgentAccount, AgentAccountsSnapshot, AgentAuthKind, AgentUsageWindow, HarnessId,
};
use zeron_rpc::methods;

use crate::icons::{self, icon};
use crate::settings::accounts::{AccountsSnapshotCache, format_reset, render_usage_meter};
use crate::settings::widgets::text_tooltip;
use crate::state::AppState;
use crate::theme::Theme;

const PROVIDERS: [(HarnessId, &str); 2] = [
    (HarnessId::Codex, "Codex"),
    (HarnessId::ClaudeCode, "Claude"),
];
const POLL_INTERVAL: Duration = Duration::from_secs(120);
const FORCE_MIN_INTERVAL: Duration = Duration::from_secs(30);
const MAX_USAGE_AGE_MS: i64 = 5 * 60 * 1_000;
const HEADER_HEIGHT: f32 = 30.0;
const PROVIDER_HEIGHT: f32 = 44.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuotaStatus {
    Available,
    Limited,
    Stale,
    Unknown,
}

impl QuotaStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Available => "Available",
            Self::Limited => "Limit reached",
            Self::Stale => "Stale",
            Self::Unknown => "Unknown",
        }
    }
}

fn quota_window(window: &AgentUsageWindow) -> bool {
    matches!(
        window.label.as_str(),
        "5h" | "Session" | "7d" | "Week" | "Weekly"
    )
}

fn quota_status(account: &AgentAccount, now: i64, online: bool) -> QuotaStatus {
    if account.auth_kind != Some(AgentAuthKind::Oauth)
        || (!account.active && !account.switchable)
        || account.usage_error.is_some()
    {
        return QuotaStatus::Unknown;
    }
    let Some(fetched) = account.usage_fetched_at else {
        return QuotaStatus::Unknown;
    };
    if !online || !(0..=MAX_USAGE_AGE_MS).contains(&now.saturating_sub(fetched)) {
        return QuotaStatus::Stale;
    }
    let mut maximum: Option<f32> = None;
    for window in account
        .usage_windows
        .iter()
        .filter(|window| quota_window(window))
    {
        if !window.used_fraction.is_finite() || window.used_fraction < 0.0 {
            return QuotaStatus::Unknown;
        }
        if window
            .resets_at
            .is_some_and(|reset| reset.timestamp_millis() <= now)
        {
            return QuotaStatus::Stale;
        }
        maximum = Some(maximum.unwrap_or(0.0).max(window.used_fraction));
    }
    match maximum {
        Some(fraction) if fraction >= 1.0 => QuotaStatus::Limited,
        Some(_) => QuotaStatus::Available,
        None => QuotaStatus::Unknown,
    }
}

fn account_name(account: &AgentAccount) -> &str {
    account
        .display_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| {
            account
                .email
                .as_deref()
                .filter(|email| !email.trim().is_empty())
        })
        .unwrap_or("Saved account")
}

#[derive(Default, Debug, PartialEq, Eq)]
struct PoolSummary {
    available: usize,
    limited: usize,
    unknown: usize,
    total: usize,
}

fn summarize(accounts: &[&AgentAccount], now: i64, online: bool) -> PoolSummary {
    let mut summary = PoolSummary {
        total: accounts.len(),
        ..Default::default()
    };
    for account in accounts {
        match quota_status(account, now, online) {
            QuotaStatus::Available => summary.available += 1,
            QuotaStatus::Limited => summary.limited += 1,
            QuotaStatus::Stale | QuotaStatus::Unknown => summary.unknown += 1,
        }
    }
    summary
}

pub(super) struct AccountPool {
    state: Entity<AppState>,
    target: Option<String>,
    device_name: String,
    online: bool,
    visible: bool,
    window_active: bool,
    interactive: bool,
    expanded: [bool; 2],
    loaded: bool,
    refreshing: bool,
    last_forced: Option<Instant>,
    auto_switch: Option<AccountAutoSwitchPrefs>,
    error: Option<String>,
    load_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    _cache: Subscription,
    _activation: Option<Subscription>,
}

impl AccountPool {
    pub(super) fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        Self {
            state,
            target: None,
            device_name: "This device".into(),
            online: false,
            visible: false,
            window_active: false,
            interactive: false,
            expanded: [false; 2],
            loaded: false,
            refreshing: false,
            last_forced: None,
            auto_switch: None,
            error: None,
            load_task: None,
            poll_task: None,
            _cache: cx.observe_global::<AccountsSnapshotCache>(|_, cx| cx.notify()),
            _activation: None,
        }
    }

    pub(super) fn track(&mut self, visible: bool, window_active: bool, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let device = state.effective_device_id();
        let target = device
            .clone()
            .filter(|id| Some(id) != state.local_device_id.as_ref());
        let device_name = device
            .as_ref()
            .and_then(|id| state.devices.iter().find(|device| &device.id == id))
            .map(|device| device.name.clone())
            .unwrap_or_else(|| "This device".into());
        let online = matches!(state.connection, zeron_proto::view::ConnectionStatus::Ready)
            && state.engine().is_some()
            && device
                .as_ref()
                .is_none_or(|id| state.device_online(id, chrono::Utc::now()));
        let changed = self.target != target;
        let resumed = visible
            && window_active
            && online
            && (!self.visible || !self.window_active || !self.online);
        if changed {
            self.target = target;
            self.expanded = [false; 2];
            self.loaded = false;
            self.last_forced = None;
            self.auto_switch = None;
            self.error = None;
            self.load_task = None;
            self.refreshing = false;
        }
        let presentation_changed = changed
            || self.visible != visible
            || self.window_active != window_active
            || self.online != online
            || self.device_name != device_name;
        self.device_name = device_name;
        self.visible = visible;
        self.window_active = window_active;
        self.online = online;
        if !visible || !window_active || !online {
            self.poll_task = None;
            self.load_task = None;
            self.refreshing = false;
        } else {
            if changed || resumed || !self.loaded {
                self.load(true, cx);
            }
            if self.poll_task.is_none() {
                self.poll_task = Some(cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor().timer(POLL_INTERVAL).await;
                        if this
                            .update(cx, |pool, cx| {
                                pool.load(true, cx);
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }));
            }
        }
        if presentation_changed {
            cx.notify();
        }
    }

    pub(super) fn set_interactive(&mut self, interactive: bool, cx: &mut Context<Self>) {
        if self.interactive != interactive {
            self.interactive = interactive;
            cx.notify();
        }
    }

    fn snapshot<'a>(&self, cx: &'a gpui::App) -> Option<&'a AgentAccountsSnapshot> {
        cx.try_global::<AccountsSnapshotCache>()?
            .0
            .get(&self.target)
    }

    pub(super) fn desired_height(&self, cx: &gpui::App) -> f32 {
        let detail_height: f32 = PROVIDERS
            .iter()
            .enumerate()
            .filter(|(index, _)| self.expanded[*index])
            .map(|(_, (harness, _))| {
                self.snapshot(cx).map_or(0.0, |snapshot| {
                    snapshot
                        .accounts
                        .iter()
                        .filter(|account| account.harness == *harness)
                        .map(|account| {
                            let meters = account
                                .usage_windows
                                .iter()
                                .filter(|window| quota_window(window))
                                .take(2)
                                .count();
                            30.0 + 16.0 * meters.max(1) as f32
                        })
                        .sum::<f32>()
                })
            })
            .sum();
        let note = if self.error.is_some() || !self.online {
            28.0
        } else {
            0.0
        };
        HEADER_HEIGHT + 8.0 + PROVIDER_HEIGHT * 2.0 + detail_height + note
    }

    fn load(&mut self, force: bool, cx: &mut Context<Self>) {
        if !self.visible || !self.window_active || !self.online || self.refreshing {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let force = force
            && self
                .last_forced
                .is_none_or(|at| at.elapsed() >= FORCE_MIN_INTERVAL);
        if force {
            self.last_forced = Some(Instant::now());
        }
        self.loaded = true;
        self.refreshing = true;
        let key = self.target.clone();
        let plain = serde_json::json!({ "targetDeviceId": key, "forceUsage": false });
        let params = serde_json::json!({ "targetDeviceId": key, "forceUsage": force });
        let prefs_params = serde_json::json!({ "targetDeviceId": key });
        let paint_first = force && self.snapshot(cx).is_none();
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let fetch = |params| {
                let engine = engine.clone();
                let executor = executor.clone();
                async move {
                    crate::attachments::call_with_timeout(
                        &engine,
                        &executor,
                        methods::LIST_AGENT_ACCOUNTS,
                        params,
                        Duration::from_secs(20),
                    )
                    .await
                    .and_then(|value| {
                        serde_json::from_value::<AgentAccountsSnapshot>(value)
                            .map_err(|error| error.to_string())
                    })
                }
            };
            if paint_first && let Ok(snapshot) = fetch(plain).await {
                this.update(cx, |pool, cx| {
                    if pool.target == key {
                        cx.default_global::<AccountsSnapshotCache>()
                            .0
                            .insert(key.clone(), snapshot);
                        cx.notify();
                    }
                })
                .ok();
            }
            let prefs = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                methods::GET_ACCOUNT_AUTO_SWITCH,
                prefs_params,
                Duration::from_secs(5),
            )
            .await
            .ok()
            .and_then(|value| serde_json::from_value::<AccountAutoSwitchPrefs>(value).ok());
            let result = fetch(params).await;
            this.update(cx, |pool, cx| {
                if pool.target != key {
                    return;
                }
                pool.refreshing = false;
                pool.auto_switch = prefs;
                match result {
                    Ok(snapshot) => {
                        cx.default_global::<AccountsSnapshotCache>()
                            .0
                            .insert(key, snapshot);
                        pool.error = None;
                    }
                    Err(error) => pool.error = Some(error),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn toggle(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.interactive {
            return;
        }
        self.expanded[index] = !self.expanded[index];
        if self.expanded[index] {
            self.load(true, cx);
        }
        cx.notify();
    }

    fn provider_row(
        &self,
        index: usize,
        snapshot: Option<&AgentAccountsSnapshot>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (harness, name) = PROVIDERS[index];
        let now = chrono::Utc::now();
        let mut accounts: Vec<_> = snapshot
            .into_iter()
            .flat_map(|snapshot| &snapshot.accounts)
            .filter(|account| account.harness == harness)
            .collect();
        accounts.sort_by(|a, b| {
            b.active
                .cmp(&a.active)
                .then_with(|| account_name(a).cmp(account_name(b)))
                .then_with(|| a.id.cmp(&b.id))
        });
        let summary = summarize(
            &accounts,
            now.timestamp_millis(),
            self.online && self.error.is_none(),
        );
        let count = if snapshot.is_none() {
            if self.refreshing {
                "Loading...".to_owned()
            } else {
                "Unavailable".to_owned()
            }
        } else if summary.total == 0 {
            "No accounts".to_owned()
        } else if summary.unknown == summary.total {
            format!("{} unknown", summary.total)
        } else {
            format!("{}/{} available", summary.available, summary.total)
        };
        let active = accounts
            .iter()
            .find(|account| account.active)
            .map(|account| format!("Active: {}", account_name(account)))
            .unwrap_or_else(|| "No active account".into());
        let auto_switch = match self.auto_switch {
            Some(prefs) if prefs.enabled(harness) => "Auto-switch on",
            Some(_) => "Auto-switch off",
            None => "Auto-switch unknown",
        };
        let mut tooltip = format!(
            "{name} on {}\n{active}\n{} available, {} at a limit, {} unknown or stale\n{auto_switch}\nAvailability is the last reported session/weekly quota, not a guarantee of sign-in health.",
            self.device_name, summary.available, summary.limited, summary.unknown
        );
        if let Some(snapshot) = snapshot {
            for warning in snapshot
                .warnings
                .iter()
                .filter(|warning| warning.harness == harness)
            {
                tooltip.push_str(&format!("\n{}", warning.message));
            }
        }
        let (brand_icon, tint) = crate::pickers::harness_brand_icon(harness);
        let button = div()
            .id(("account-pool-provider", index))
            .h(px(PROVIDER_HEIGHT))
            .flex_none()
            .px(px(8.0))
            .flex()
            .flex_col()
            .justify_center()
            .gap(px(2.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .hover(|row| row.bg(theme.element_hover))
            .border_1()
            .border_color(gpui::transparent_black())
            .focus_visible(|row| row.border_color(theme.accent))
            .role(gpui::Role::Button)
            .aria_expanded(self.expanded[index])
            .aria_label(format!(
                "{name}: {count}, {active}, {auto_switch}. {} accounts",
                if self.expanded[index] {
                    "Collapse"
                } else {
                    "Expand"
                }
            ))
            .tab_index(if self.interactive { 0 } else { -1 })
            .tooltip(text_tooltip(tooltip))
            .on_click(cx.listener(move |pool, _, _, cx| {
                cx.stop_propagation();
                pool.toggle(index, cx);
            }))
            .on_key_down(cx.listener(move |pool, event: &gpui::KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    cx.stop_propagation();
                    pool.toggle(index, cx);
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .min_w_0()
                    .child(
                        icon(brand_icon)
                            .size(px(12.0))
                            .text_color(tint.unwrap_or(theme.text_muted)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.text)
                            .child(name),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(if summary.limited > 0 {
                                theme.warning
                            } else {
                                theme.text_muted
                            })
                            .child(count),
                    )
                    .child(
                        icon(if self.expanded[index] {
                            icons::ALT_ARROW_DOWN
                        } else {
                            icons::ALT_ARROW_RIGHT
                        })
                        .size(px(10.0))
                        .text_color(theme.text_muted),
                    ),
            )
            .child(
                div()
                    .pl(px(18.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .min_w_0()
                    .text_size(crate::typography::ui_rems(10.5))
                    .text_color(theme.text_muted)
                    .child(div().flex_1().min_w_0().truncate().child(active))
                    .child(div().flex_none().child(auto_switch)),
            );
        div()
            .flex_none()
            .child(button)
            .when(self.expanded[index], |group| {
                group.children(
                    accounts
                        .iter()
                        .map(|account| self.account_row(account, theme)),
                )
            })
            .into_any_element()
    }

    fn account_row(&self, account: &AgentAccount, theme: &Theme) -> AnyElement {
        let now = chrono::Utc::now();
        let status = quota_status(
            account,
            now.timestamp_millis(),
            self.online && self.error.is_none(),
        );
        let mut tooltip = vec![
            account
                .email
                .as_deref()
                .unwrap_or(account_name(account))
                .to_owned(),
        ];
        if let Some(plan) = &account.plan_label {
            tooltip.push(plan.clone());
        }
        if account.active {
            tooltip.push("Active account".into());
        }
        if let Some(error) = &account.usage_error {
            tooltip.push(error.clone());
        }
        if let Some(fetched) = account
            .usage_fetched_at
            .and_then(chrono::DateTime::from_timestamp_millis)
        {
            tooltip.push(format!(
                "Last updated {}",
                fetched
                    .with_timezone(&chrono::Local)
                    .format("%b %-d, %-I:%M:%S %p")
            ));
        }
        for window in &account.usage_windows {
            let percent = if window.used_fraction.is_finite() && window.used_fraction >= 0.0 {
                format!("{:.1}%", window.used_fraction * 100.0)
            } else {
                "unavailable".into()
            };
            tooltip.push(format!(
                "{}: {percent}; {}",
                window.label,
                format_reset(window.resets_at, now).unwrap_or_else(|| "reset unavailable".into())
            ));
        }
        let color = match status {
            QuotaStatus::Available => theme.success,
            QuotaStatus::Limited => theme.danger,
            QuotaStatus::Stale | QuotaStatus::Unknown => theme.text_muted,
        };
        let meters: Vec<_> = account
            .usage_windows
            .iter()
            .filter(|window| quota_window(window))
            .take(2)
            .filter(|window| window.used_fraction.is_finite() && window.used_fraction >= 0.0)
            .map(|window| render_usage_meter(window, theme))
            .collect();
        let has_meters = !meters.is_empty();
        div()
            .id(SharedString::from(format!(
                "account-pool-account-{}",
                account.id
            )))
            .flex_none()
            .px(px(8.0))
            .py(px(6.0))
            .ml(px(18.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .min_w_0()
            .tooltip(text_tooltip(tooltip.join("\n")))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .min_w_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.text)
                            .child(account_name(account).to_owned()),
                    )
                    .when(account.active, |row| {
                        row.child(icon(icons::CHECK).size(px(10.0)).text_color(theme.accent))
                    })
                    .child(
                        div()
                            .flex_none()
                            .text_size(crate::typography::ui_rems(10.5))
                            .text_color(color)
                            .child(status.label()),
                    ),
            )
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .children(meters)
                    .when(!has_meters, |row| {
                        row.child(
                            div()
                                .h(px(16.0))
                                .text_color(theme.text_muted)
                                .child("Usage unavailable"),
                        )
                    }),
            )
            .into_any_element()
    }
}

impl Render for AccountPool {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self._activation.is_none() {
            self._activation = Some(cx.observe_window_activation(window, |pool, window, cx| {
                pool.track(pool.visible, window.is_window_active(), cx);
            }));
        }
        let theme = Theme::of(cx).clone();
        let snapshot = self.snapshot(cx).cloned();
        let can_refresh = self.interactive && self.online && !self.refreshing;
        let note = if !self.online {
            Some("Device offline")
        } else if self.error.is_some() {
            Some("Refresh failed")
        } else {
            None
        };
        div()
            .id("home-account-pool")
            .debug_selector(|| "home-account-pool".into())
            .size_full()
            .flex()
            .flex_col()
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .bg(super::working_now::panel_background(&theme))
            .overflow_hidden()
            .text_size(crate::typography::ui_rems(11.0))
            .text_color(theme.text_muted)
            .child(
                div()
                    .h(px(HEADER_HEIGHT))
                    .flex_none()
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        icon(icons::KEY_MINIMALISTIC)
                            .size(px(12.0))
                            .text_color(theme.text_muted),
                    )
                    .child("Account pool")
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_right()
                            .child(self.device_name.clone()),
                    )
                    .child(
                        div()
                            .id("account-pool-refresh")
                            .size(px(22.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(gpui::transparent_black())
                            .focus_visible(|button| button.border_color(theme.accent))
                            .role(gpui::Role::Button)
                            .aria_label("Refresh account usage")
                            .tab_index(if can_refresh { 0 } else { -1 })
                            .when(can_refresh, |button| {
                                button
                                    .cursor_pointer()
                                    .hover(|button| button.bg(theme.element_hover))
                            })
                            .tooltip(text_tooltip(if self.refreshing {
                                "Refreshing usage..."
                            } else {
                                "Refresh account usage"
                            }))
                            .on_click(cx.listener(move |pool, _, _, cx| {
                                cx.stop_propagation();
                                if can_refresh {
                                    pool.load(true, cx);
                                }
                            }))
                            .on_key_down(cx.listener(
                                move |pool, event: &gpui::KeyDownEvent, _, cx| {
                                    if can_refresh
                                        && matches!(event.keystroke.key.as_str(), "enter" | "space")
                                    {
                                        cx.stop_propagation();
                                        pool.load(true, cx);
                                    }
                                },
                            ))
                            .child(
                                icon(icons::REFRESH)
                                    .size(px(12.0))
                                    .text_color(theme.text_muted)
                                    .opacity(if can_refresh { 1.0 } else { 0.5 }),
                            ),
                    ),
            )
            .child(
                div()
                    .id("home-account-pool-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(4.0))
                    .pb(px(4.0))
                    .children(
                        (0..PROVIDERS.len())
                            .map(|index| self.provider_row(index, snapshot.as_ref(), &theme, cx)),
                    )
                    .children(note.map(|note| {
                        div()
                            .h(px(28.0))
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .text_color(theme.warning)
                            .child(note)
                            .tooltip(text_tooltip(self.error.clone().unwrap_or_else(|| {
                                "Reconnect this device to refresh account usage.".into()
                            })))
                    })),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> AgentAccount {
        AgentAccount {
            id: "saved-account".into(),
            harness: HarnessId::Codex,
            email: Some("user@example.com".into()),
            plan_label: None,
            active: false,
            usage_windows: vec![
                AgentUsageWindow {
                    label: "Session".into(),
                    used_fraction: 0.04,
                    resets_at: None,
                },
                AgentUsageWindow {
                    label: "Weekly".into(),
                    used_fraction: 0.62,
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
    fn either_quota_can_exhaust_an_account() {
        let mut account = account();
        assert_eq!(
            quota_status(&account, 100_001, true),
            QuotaStatus::Available
        );
        for index in 0..2 {
            account.usage_windows[index].used_fraction = 1.0;
            assert_eq!(quota_status(&account, 100_001, true), QuotaStatus::Limited);
            account.usage_windows[index].used_fraction = 0.1;
        }
    }

    #[test]
    fn offline_old_and_reset_readings_are_never_available() {
        let mut account = account();
        assert_eq!(quota_status(&account, 100_001, false), QuotaStatus::Stale);
        assert_eq!(
            quota_status(&account, 100_000 + MAX_USAGE_AGE_MS + 1, true),
            QuotaStatus::Stale
        );
        assert_eq!(quota_status(&account, 99_999, true), QuotaStatus::Stale);
        account.usage_windows[0].resets_at = chrono::DateTime::from_timestamp_millis(100_000);
        assert_eq!(quota_status(&account, 100_001, true), QuotaStatus::Stale);
    }

    #[test]
    fn errors_missing_invalid_and_unusable_accounts_are_unknown() {
        let base = account();
        let mut cases = vec![];
        let mut error = base.clone();
        error.usage_error = Some("Sign in again".into());
        cases.push(error);
        let mut missing = base.clone();
        missing.usage_fetched_at = None;
        cases.push(missing);
        let mut empty = base.clone();
        empty.usage_windows.clear();
        cases.push(empty);
        let mut invalid = base.clone();
        invalid.usage_windows[0].used_fraction = f32::NAN;
        cases.push(invalid);
        let mut key = base.clone();
        key.auth_kind = Some(AgentAuthKind::ApiKey);
        cases.push(key);
        let mut unswitchable = base;
        unswitchable.switchable = false;
        cases.push(unswitchable);
        for account in cases {
            assert_eq!(quota_status(&account, 100_001, true), QuotaStatus::Unknown);
        }
    }

    #[test]
    fn summary_counts_each_account_once_without_combining_percentages() {
        let available = account();
        let mut limited = account();
        limited.usage_windows[1].used_fraction = 1.0;
        let mut unknown = account();
        unknown.usage_error = Some("Retry later".into());
        assert_eq!(
            summarize(&[&available, &limited, &unknown], 100_001, true),
            PoolSummary {
                available: 1,
                limited: 1,
                unknown: 1,
                total: 3,
            }
        );
    }

    #[gpui::test]
    fn reads_the_selected_devices_shared_cache_without_changing_accounts(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let state = cx.new(|_| {
                let mut state = AppState::new();
                state.local_device_id = Some("windows".into());
                state.selected_device = Some("windows".into());
                state
            });
            let local = AgentAccountsSnapshot {
                accounts: vec![account()],
                warnings: vec![],
            };
            let mut remote_account = account();
            remote_account.id = "ubuntu-account".into();
            remote_account.email = Some("ubuntu@example.com".into());
            let remote = AgentAccountsSnapshot {
                accounts: vec![remote_account],
                warnings: vec![],
            };
            let cache = cx.default_global::<AccountsSnapshotCache>();
            cache.0.insert(None, local.clone());
            cache.0.insert(Some("ubuntu".into()), remote.clone());
            let pool = cx.new(|cx| AccountPool::new(state.clone(), cx));
            pool.update(cx, |pool, cx| {
                pool.track(true, true, cx);
                assert_eq!(pool.target, None);
                assert_eq!(pool.snapshot(cx), Some(&local));
                pool.expanded[0] = true;
            });
            state.update(cx, |state, _| state.selected_device = Some("ubuntu".into()));
            pool.update(cx, |pool, cx| {
                pool.track(true, true, cx);
                assert_eq!(pool.target.as_deref(), Some("ubuntu"));
                assert_eq!(pool.snapshot(cx), Some(&remote));
                assert_eq!(pool.expanded, [false; 2]);
                // An offline engine must not create polling or request tasks.
                assert!(pool.load_task.is_none());
                assert!(pool.poll_task.is_none());
                pool.track(false, false, cx);
            });
            let cache = cx.global::<AccountsSnapshotCache>();
            assert_eq!(cache.0.get(&None), Some(&local));
            assert_eq!(cache.0.get(&Some("ubuntu".into())), Some(&remote));
        });
    }
}
