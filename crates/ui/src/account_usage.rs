//! The composer footer's ring cluster. The plan-usage ring shows how much of
//! the active account's rate limit the session's harness has used, beside
//! the context ring. Clicking it opens the harness's accounts — each with its
//! usage meters — and clicking one switches to it, the same `ActivateAgentAccount` Settings →
//! Accounts runs. Both views share [`AccountsSnapshotCache`], so a switch in
//! either shows up in the other.
use std::time::{Duration, Instant};

use futures::{FutureExt, StreamExt, channel::mpsc};
use gpui::{
    Context, Entity, IntoElement, Render, SharedString, Subscription, Task, Window, div,
    prelude::*, px,
};
use zeron_proto::{AgentAccount, AgentAccountsSnapshot, HarnessId};
use zeron_rpc::methods;

use crate::popover;
use crate::settings::accounts::{
    self, AccountsSnapshotCache, UsageLevel, render_usage_meter, reports_usage, signs_in,
    usage_color, usage_level,
};
use crate::state::AppState;
use crate::theme::Theme;

/// The engine enforces the same per-account cooldown and provider backoff.
const FORCE_MIN_INTERVAL: Duration = Duration::from_secs(30);
const ACTIVE_POLL_INTERVAL: Duration = Duration::from_secs(60);
const BACKGROUND_POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy, PartialEq, Eq)]
enum UsageRefresh {
    Cached,
    Active,
    All,
}

fn refresh_delay(age: Option<Duration>, foreground: bool, pending: bool) -> Duration {
    let interval = if pending {
        FORCE_MIN_INTERVAL
    } else if foreground {
        ACTIVE_POLL_INTERVAL
    } else {
        BACKGROUND_POLL_INTERVAL
    };
    interval.saturating_sub(age.unwrap_or(interval))
}

fn usage_params(refresh: UsageRefresh, harness: Option<HarnessId>) -> serde_json::Value {
    let mut params = serde_json::json!({ "forceUsage": refresh != UsageRefresh::Cached });
    if refresh == UsageRefresh::Active
        && let Some(harness) = harness
    {
        params["usageHarness"] = serde_json::json!(harness);
    }
    params
}

#[derive(Default)]
struct CompletionTracker {
    last: Option<(String, Option<String>)>,
}

impl CompletionTracker {
    fn observe(&mut self, next: Option<(String, Option<String>)>) -> bool {
        let completed = match (&self.last, &next) {
            (Some(previous), Some(current)) => {
                previous.0 == current.0 && previous.1 != current.1 && current.1.is_some()
            }
            _ => false,
        };
        self.last = next;
        completed
    }
}

/// The binding limit: the most-used window of the account. Pure.
pub fn used_fraction(account: &AgentAccount) -> Option<f32> {
    account
        .usage_windows
        .iter()
        .map(|window| window.used_fraction.clamp(0.0, 1.0))
        .reduce(f32::max)
}

/// The harness's live account, when it reports usage. Pure.
pub fn active_account(
    snapshot: &AgentAccountsSnapshot,
    harness: HarnessId,
) -> Option<&AgentAccount> {
    snapshot
        .accounts
        .iter()
        .find(|account| account.harness == harness && account.active)
}

/// Loads (and switches) the accounts of the composer's target device.
pub struct AccountUsage {
    state: Entity<AppState>,
    /// `None` = this device, as in [`AccountsSnapshotCache`].
    target: Option<String>,
    harness: Option<HarnessId>,
    /// Once any list has been asked for the current target.
    loaded: bool,
    last_forced: Option<Instant>,
    pending_refresh: Option<UsageRefresh>,
    window_active: bool,
    completions: CompletionTracker,
    poll_wake: mpsc::UnboundedSender<()>,
    error: Option<SharedString>,
    load_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    popup: popover::Popup<FooterCard>,
    _poll: Task<()>,
    _cache: Subscription,
    _state: Subscription,
    _activation: Option<Subscription>,
}

impl AccountUsage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let (poll_wake, mut wake_rx) = mpsc::unbounded();
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let Ok(delay) = this.update(cx, |usage, cx| {
                    if usage.harness.is_none() {
                        return BACKGROUND_POLL_INTERVAL;
                    }
                    if usage.next_refresh_delay().is_zero() {
                        let refresh = usage.pending_refresh.unwrap_or(UsageRefresh::Active);
                        usage.load(refresh, cx);
                    }
                    usage.next_refresh_delay()
                }) else {
                    break;
                };
                let timer = cx.background_executor().timer(delay).fuse();
                let wake = wake_rx.next().fuse();
                futures::pin_mut!(timer, wake);
                futures::select! {
                    _ = timer => {},
                    next = wake => {
                        if next.is_none() {
                            break;
                        }
                    },
                }
            }
        });
        Self {
            _state: cx.observe(&state, |usage, _, cx| usage.state_changed(cx)),
            state,
            target: None,
            harness: None,
            loaded: false,
            last_forced: None,
            pending_refresh: None,
            window_active: false,
            completions: CompletionTracker::default(),
            poll_wake,
            error: None,
            load_task: None,
            action_task: None,
            popup: popover::Popup::default(),
            _poll: poll,
            // Settings → Accounts writes the same cache.
            _cache: cx.observe_global::<AccountsSnapshotCache>(|_, cx| cx.notify()),
            _activation: None,
        }
    }

    fn next_refresh_delay(&self) -> Duration {
        refresh_delay(
            self.last_forced.map(|at| at.elapsed()),
            self.window_active,
            self.pending_refresh.is_some(),
        )
    }

    fn state_changed(&mut self, cx: &mut Context<Self>) {
        let completion = {
            let state = self.state.read(cx);
            state.selected_chat.as_ref().and_then(|chat_id| {
                state
                    .session_for(chat_id)
                    .map(|session| (chat_id.clone(), session.last_completed_turn.clone()))
            })
        };
        // The completion marker also advances when a queued turn starts
        // immediately, without an observable Working -> Idle transition.
        if self.completions.observe(completion) && self.harness.is_some() {
            self.load(UsageRefresh::Active, cx);
        }
        cx.notify();
    }

    /// Point at the session's harness and device; loads on first sight of a
    /// device. Cheap enough to call every render.
    pub fn track(
        &mut self,
        harness: Option<HarnessId>,
        target: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let harness = harness.filter(|h| signs_in(*h) && reports_usage(*h));
        if self.target != target || self.harness != harness {
            self.target = target;
            self.harness = harness;
            self.loaded = false;
            self.last_forced = None;
            self.pending_refresh = None;
            self.error = None;
            let _ = self.poll_wake.unbounded_send(());
        }
        if harness.is_some() && !self.loaded {
            self.loaded = true;
            self.load(UsageRefresh::Active, cx);
        }
    }

    fn snapshot<'a>(&self, cx: &'a gpui::App) -> Option<&'a AgentAccountsSnapshot> {
        cx.try_global::<AccountsSnapshotCache>()?
            .0
            .get(&self.target)
    }

    fn params(&self, mut value: serde_json::Value) -> serde_json::Value {
        if let (Some(target), Some(object)) = (&self.target, value.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(target));
        }
        value
    }

    /// Plain list first when nothing is cached (the engine's persisted usage
    /// paints at once), then the forced probe replaces it.
    fn load(&mut self, refresh: UsageRefresh, cx: &mut Context<Self>) {
        let refresh = if refresh != UsageRefresh::Cached
            && self.pending_refresh == Some(UsageRefresh::All)
        {
            UsageRefresh::All
        } else {
            refresh
        };
        let force_usage = refresh != UsageRefresh::Cached;
        if force_usage {
            if self
                .last_forced
                .is_some_and(|at| at.elapsed() < FORCE_MIN_INTERVAL)
            {
                // A turn finishing inside the cooldown still gets a refresh
                // at its end, rather than waiting for the next periodic poll.
                self.pending_refresh = Some(refresh);
                let _ = self.poll_wake.unbounded_send(());
                return;
            }
            self.pending_refresh = None;
            self.last_forced = Some(Instant::now());
            let _ = self.poll_wake.unbounded_send(());
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let key = self.target.clone();
        let paint_first = force_usage && self.snapshot(cx).is_none();
        let plain = self.params(usage_params(UsageRefresh::Cached, self.harness));
        let params = self.params(usage_params(refresh, self.harness));
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let fetch = |params: serde_json::Value| {
                let engine = engine.clone();
                async move {
                    engine
                        .client()
                        .call(methods::LIST_AGENT_ACCOUNTS, params)
                        .await
                        .ok()
                        .and_then(|value| {
                            serde_json::from_value::<AgentAccountsSnapshot>(value).ok()
                        })
                }
            };
            let store = |snapshot: AgentAccountsSnapshot, cx: &mut gpui::AsyncApp| {
                let key = key.clone();
                this.update(cx, |_, cx| {
                    cx.default_global::<AccountsSnapshotCache>()
                        .0
                        .insert(key, snapshot);
                })
                .ok();
            };
            if paint_first && let Some(snapshot) = fetch(plain).await {
                store(snapshot, cx);
            }
            if let Some(snapshot) = fetch(params).await {
                store(snapshot, cx);
            }
        }));
    }

    /// Switch optimistically, like Settings → Accounts: the rows flip at once
    /// and the engine's reply replaces them; a refusal restores the list.
    fn switch(&mut self, account: &AgentAccount, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let key = self.target.clone();
        let cache = &mut cx.default_global::<AccountsSnapshotCache>().0;
        let previous = cache.get(&key).cloned();
        if let Some(snapshot) = cache.get_mut(&key) {
            // Per-provider agents keep one live login per provider: only the
            // switched-to row's group changes.
            accounts::mark_switched(snapshot, account);
        }
        self.error = None;
        let params = self.params(serde_json::json!({
            "id": account.id,
            "accountId": account.id,
            "harness": account.harness,
        }));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::ACTIVATE_AGENT_ACCOUNT, params)
                .await;
            this.update(cx, |usage, cx| {
                match result.map(serde_json::from_value::<AgentAccountsSnapshot>) {
                    Ok(Ok(snapshot)) => {
                        cx.default_global::<AccountsSnapshotCache>()
                            .0
                            .insert(key, snapshot);
                    }
                    Ok(Err(_)) => usage.load(UsageRefresh::Cached, cx),
                    Err(err) => {
                        if let Some(previous) = previous {
                            cx.default_global::<AccountsSnapshotCache>()
                                .0
                                .insert(key, previous);
                        }
                        usage.error = Some(err.to_string().into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The ring's reading: the live account's most-used window, when the
    /// harness has a live account with usage to show.
    fn fraction(&self, cx: &gpui::App) -> Option<f32> {
        used_fraction(active_account(self.snapshot(cx)?, self.harness?)?)
    }

    /// A trigger click: open this ring's popover, or close it when the
    /// press found it open (the card's mouse-down-out already began that
    /// close — see `Popup::note_trigger_press`).
    fn toggle(&mut self, card: FooterCard, cx: &mut Context<Self>) {
        if self.popup.take_press_was_open() || self.popup.as_open() == Some(&card) {
            self.dismiss(cx);
            return;
        }
        if card == FooterCard::Accounts {
            // Opening the card is the moment someone cares: re-probe.
            self.load(UsageRefresh::All, cx);
        }
        self.popup.open(card);
        cx.notify();
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self.popup.begin_close() {
            popover::reap_popup(cx, |usage: &mut Self| &mut usage.popup);
        }
        cx.notify();
    }

    /// A footer ring that opens `card` above itself, right-aligned like the
    /// model picker.
    fn trigger(
        &self,
        chip: gpui::Stateful<gpui::Div>,
        card: FooterCard,
        content: impl FnOnce(&Self, &mut Context<Self>) -> gpui::Div,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let chip = chip
            .relative()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |usage, _, _, _| {
                    usage
                        .popup
                        .note_trigger_press_matching(|open| *open == card);
                }),
            )
            .on_click(cx.listener(move |usage, _, _, cx| usage.toggle(card, cx)));
        if self.popup.get() != Some(&card) {
            return chip.into_any_element();
        }
        let content = content(self, cx)
            .on_mouse_down_out(cx.listener(|usage, _, _, cx| usage.dismiss(cx)))
            .into_any_element();
        chip.child(popover::anchored_menu_above_end(
            card.id(),
            content,
            self.popup.closing_since(),
        ))
        .into_any_element()
    }

    fn accounts_card(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = &Theme::of(cx).for_popup();
        let harness = self.harness;
        let rows: Vec<AgentAccount> = match (harness, self.snapshot(cx)) {
            (Some(harness), Some(snapshot)) => accounts::provider_accounts(snapshot, harness)
                .into_iter()
                .cloned()
                .collect(),
            _ => Vec::new(),
        };
        let title = format!(
            "{} accounts",
            harness.map_or("Agent", accounts::provider_name)
        );
        popover::popover_card(theme)
            .w(px(400.0))
            .flex()
            .flex_col()
            .child(popover::menu_heading(theme, &title))
            .children(rows.iter().enumerate().map(|(ix, account)| {
                let email: SharedString = account
                    .email
                    .clone()
                    .or_else(|| account.display_name.clone())
                    .unwrap_or_else(|| "Unknown account".into())
                    .into();
                let can_switch = !account.active && account.switchable;
                let mut meta: Vec<gpui::AnyElement> = account
                    .plan_label
                    .iter()
                    .map(|plan| {
                        div()
                            .child(SharedString::from(plan.clone()))
                            .into_any_element()
                    })
                    .collect();
                if account.active {
                    meta.push(
                        div()
                            .text_color(theme.accent)
                            .child(SharedString::from("In use"))
                            .into_any_element(),
                    );
                } else if let Some(reason) = account
                    .usage_error
                    .clone()
                    .filter(|_| account.usage_windows.is_empty())
                {
                    meta.push(div().child(SharedString::from(reason)).into_any_element());
                }
                let switch_to = account.clone();
                popover::menu_row(theme, account.active, format!("account-usage-row-{ix}"))
                    .id(("account-usage-row", ix))
                    .when(!can_switch, |row| row.cursor_default())
                    .when(can_switch, |row| {
                        row.on_click(cx.listener(move |usage, _, _, cx| {
                            usage.switch(&switch_to, cx);
                        }))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.5))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(email),
                            )
                            .when(!meta.is_empty(), |el| {
                                el.child(crate::settings::widgets::meta_line(theme, meta))
                            }),
                    )
                    .child(
                        div().flex_none().flex().flex_col().gap(px(2.0)).children(
                            account
                                .usage_windows
                                .iter()
                                .take(2)
                                .map(|window| render_usage_meter(window, theme)),
                        ),
                    )
            }))
            .children(self.error.clone().map(|error| {
                div()
                    .px(px(8.0))
                    .py(px(4.0))
                    .text_size(px(12.0))
                    .text_color(theme.danger)
                    .child(error)
            }))
    }
}

/// Which footer ring's popover is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterCard {
    Accounts,
    Context,
}

impl FooterCard {
    fn id(self) -> &'static str {
        match self {
            FooterCard::Accounts => "account-usage-menu",
            FooterCard::Context => "context-usage-menu",
        }
    }
}

/// The footer's ring cluster: account usage (accent arc, so the two read
/// apart), then context occupancy. Each opens its popover on click.
impl Render for AccountUsage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self._activation.is_none() {
            self.window_active = window.is_window_active();
            let _ = self.poll_wake.unbounded_send(());
            self._activation = Some(cx.observe_window_activation(
                window,
                |usage: &mut AccountUsage, window, _| {
                    usage.window_active = window.is_window_active();
                    let _ = usage.poll_wake.unbounded_send(());
                },
            ));
        }
        let theme = Theme::of(cx).clone();
        let context = self.state.read(cx).context_usage;
        let account = self.fraction(cx).map(|fraction| {
            let level = usage_level(fraction);
            let chip = crate::context_usage::ring_chip(
                "account-usage",
                fraction,
                usage_color(level, &theme),
                match level {
                    UsageLevel::Normal => theme.text_muted,
                    _ => usage_color(level, &theme),
                },
                format!("{}%", (fraction * 100.0).round() as u32),
                self.popup.get() == Some(&FooterCard::Accounts),
                &theme,
            );
            self.trigger(
                chip,
                FooterCard::Accounts,
                |usage, cx| usage.accounts_card(cx),
                cx,
            )
        });
        let context = crate::context_usage::has_window(context).then(|| {
            let chip = crate::context_usage::chip(
                context,
                self.popup.get() == Some(&FooterCard::Context),
                &theme,
            );
            self.trigger(
                chip,
                FooterCard::Context,
                move |_, cx| crate::context_usage::card(context, &Theme::of(cx).for_popup()),
                cx,
            )
        });
        div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .children(account)
            .children(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::AgentUsageWindow;

    #[test]
    fn polling_uses_foreground_and_background_intervals() {
        assert_eq!(
            refresh_delay(Some(Duration::ZERO), true, false),
            Duration::from_secs(60)
        );
        assert_eq!(
            refresh_delay(Some(Duration::ZERO), false, false),
            Duration::from_secs(300)
        );
        assert_eq!(
            refresh_delay(Some(Duration::from_secs(45)), true, false),
            Duration::from_secs(15)
        );
        assert_eq!(
            refresh_delay(Some(Duration::from_secs(120)), false, false),
            Duration::from_secs(180)
        );
        assert_eq!(
            refresh_delay(Some(Duration::from_secs(120)), true, false),
            Duration::ZERO
        );
        assert_eq!(refresh_delay(None, true, false), Duration::ZERO);
    }

    #[test]
    fn completion_and_manual_refreshes_wait_only_for_the_cooldown() {
        for foreground in [true, false] {
            assert_eq!(
                refresh_delay(Some(Duration::from_secs(10)), foreground, true),
                Duration::from_secs(20)
            );
            assert_eq!(
                refresh_delay(Some(Duration::from_secs(30)), foreground, true),
                Duration::ZERO
            );
            assert_eq!(
                refresh_delay(Some(Duration::from_secs(60)), foreground, true),
                Duration::ZERO
            );
        }
    }

    #[test]
    fn automatic_refreshes_are_scoped_but_manual_and_cached_lists_are_not() {
        assert_eq!(
            usage_params(UsageRefresh::Active, Some(HarnessId::Codex)),
            serde_json::json!({ "forceUsage": true, "usageHarness": "codex" })
        );
        assert_eq!(
            usage_params(UsageRefresh::All, Some(HarnessId::Codex)),
            serde_json::json!({ "forceUsage": true })
        );
        assert_eq!(
            usage_params(UsageRefresh::Cached, Some(HarnessId::Codex)),
            serde_json::json!({ "forceUsage": false })
        );
    }

    #[test]
    fn completion_markers_refresh_once_without_firing_on_chat_selection() {
        let marker =
            |chat: &str, turn: Option<&str>| Some((chat.to_owned(), turn.map(str::to_owned)));
        let mut tracker = CompletionTracker::default();
        assert!(!tracker.observe(marker("a", Some("old"))));
        assert!(!tracker.observe(marker("a", Some("old"))));
        assert!(tracker.observe(marker("a", Some("new"))));
        assert!(!tracker.observe(marker("a", Some("new"))));
        assert!(!tracker.observe(marker("b", Some("existing"))));
        assert!(tracker.observe(marker("b", Some("completed"))));
        assert!(!tracker.observe(None));
        assert!(!tracker.observe(marker("a", Some("new"))));
        assert!(!tracker.observe(marker("fresh", None)));
        assert!(tracker.observe(marker("fresh", Some("first"))));
    }

    fn account(harness: HarnessId, active: bool, used: &[f32]) -> AgentAccount {
        serde_json::from_value(serde_json::json!({
            "id": format!("{harness:?}-{active}"),
            "harness": harness,
            "email": null,
            "planLabel": null,
            "active": active,
            "switchable": true,
            "usageWindows": used
                .iter()
                .map(|fraction| AgentUsageWindow {
                    label: "5h".into(),
                    used_fraction: *fraction,
                    resets_at: None,
                })
                .collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    #[test]
    fn ring_shows_the_most_used_window() {
        assert_eq!(used_fraction(&account(HarnessId::Codex, true, &[])), None);
        assert_eq!(
            used_fraction(&account(HarnessId::Codex, true, &[0.12, 0.64])),
            Some(0.64)
        );
        assert_eq!(
            used_fraction(&account(HarnessId::Codex, true, &[1.4])),
            Some(1.0)
        );
    }

    #[test]
    fn active_account_is_scoped_to_the_harness() {
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![
                account(HarnessId::ClaudeCode, true, &[0.3]),
                account(HarnessId::Codex, false, &[0.1]),
                account(HarnessId::Codex, true, &[0.2]),
            ],
            warnings: Vec::new(),
        };
        assert_eq!(
            active_account(&snapshot, HarnessId::Codex).map(|a| a.id.as_str()),
            Some("Codex-true")
        );
        assert!(active_account(&snapshot, HarnessId::Cursor).is_none());
    }
}
