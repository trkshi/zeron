//! Usage history is loaded only while this page is visible. No transcript copies.
//!
//! Layout: a summary column beside a per-provider trend, totals, share bars,
//! and a model/period breakdown; Limits pools each provider's accounts per window.
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use chrono::{DateTime, Local, TimeZone, Utc};
use gpui::{
    AnyElement, Bounds, Context, Entity, FocusHandle, Hsla, IntoElement, PathBuilder, Pixels,
    Render, ScrollHandle, SharedString, Subscription, Task, Window, canvas, div, point, prelude::*,
    px, relative,
};
use zeron_proto::{
    AgentAccount, AgentAccountsSnapshot, AgentAuthKind, AgentResetCreditReply, AgentResetOutcome,
    AgentUsageWindow, HarnessId, UsageHistorySummary, UsageHistoryTotals,
};
use zeron_rpc::methods;

use crate::icons::{self, icon};
use crate::motion::{self, ZERON_PULSE};
use crate::popover::Popup;
use crate::settings::accounts::{AccountsSnapshotCache, format_reset};
use crate::settings::widgets::text_tooltip;
use crate::state::AppState;
use crate::theme::{Theme, mix};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Metric {
    Cost,
    Tokens,
    Limits,
}

#[derive(Clone, PartialEq, Eq)]
struct ModelKey(Option<HarnessId>, Option<String>);

/// Chart hover: the period under the pointer and the pointer inside the plot.
/// The plot width rides along because the tooltip is laid out before the
/// canvas measures itself in the next paint.
#[derive(Clone, Copy, PartialEq)]
struct Hover {
    index: usize,
    x: f32,
    y: f32,
    width: f32,
}

/// A reset-credit redemption on one account, shown in its detail dialog.
#[derive(Clone, PartialEq)]
enum ResetState {
    /// Asking before a credit is spent: redeeming cannot be undone.
    Confirming,
    Busy,
    Done(SharedString),
    Failed(SharedString),
}

/// Providers whose subscription limits the Limits view pools.
const LIMIT_PROVIDERS: [HarnessId; 2] = [HarnessId::Codex, HarnessId::ClaudeCode];

/// Reading order shared by the summary, chart, tooltip, and tables.
const PROVIDER_ORDER: [Option<HarnessId>; 12] = [
    Some(HarnessId::Codex),
    Some(HarnessId::ClaudeCode),
    Some(HarnessId::Grok),
    Some(HarnessId::Cursor),
    Some(HarnessId::Opencode),
    Some(HarnessId::Antigravity),
    Some(HarnessId::Pi),
    Some(HarnessId::Devin),
    Some(HarnessId::Hermes),
    Some(HarnessId::Dsh),
    Some(HarnessId::Mock),
    None,
];

const METRICS: [(Metric, &str); 3] = [
    (Metric::Cost, "Cost"),
    (Metric::Tokens, "Tokens"),
    (Metric::Limits, "Limits"),
];

const PERIODS: [(u32, &str); 4] = [
    (1, "Past 24h"),
    (7, "7 days"),
    (30, "30 days"),
    (90, "90 days"),
];

/// One painted series: colour, points in window pixels, and their tangents.
type Curve = (Hsla, Vec<(f32, f32)>, Vec<f32>);

const CHART_HEIGHT: f32 = 224.0;
/// Room above the top gridline so a stroke at the peak is not clipped.
const PLOT_TOP: f32 = 8.0;
const AXIS_WIDTH: f32 = 56.0;
const TOOLTIP_WIDTH: f32 = 184.0;
/// Width at which the summary sits beside the chart and bars pair up.
const WIDE: f32 = 760.0;

pub struct UsageDashboard {
    state: Entity<AppState>,
    target: Option<String>,
    device_name: String,
    visible: bool,
    active: bool,
    online: bool,
    metric: Metric,
    days: u32,
    by_time: bool,
    device_menu: Popup<()>,
    summary: Option<UsageHistorySummary>,
    loading: bool,
    error: Option<String>,
    account_error: Option<String>,
    selected_model: Option<ModelKey>,
    selected_account: Option<AgentAccount>,
    /// Keyed by account id, so a redemption still running when its dialog
    /// closes reports back if the dialog reopens.
    reset: Option<(String, ResetState)>,
    reset_task: Option<Task<()>>,
    hover_period: Rc<Cell<Option<Hover>>>,
    detail_hover_period: Rc<Cell<Option<Hover>>>,
    scroll: ScrollHandle,
    focus: FocusHandle,
    load_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    _accounts: Subscription,
}

impl UsageDashboard {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        Self {
            state,
            target: None,
            device_name: String::new(),
            visible: false,
            active: false,
            online: false,
            metric: Metric::Tokens,
            days: 30,
            by_time: false,
            device_menu: Popup::default(),
            summary: None,
            loading: false,
            error: None,
            account_error: None,
            selected_model: None,
            selected_account: None,
            reset: None,
            reset_task: None,
            hover_period: Rc::new(Cell::new(None)),
            detail_hover_period: Rc::new(Cell::new(None)),
            scroll: ScrollHandle::new(),
            focus: cx.focus_handle(),
            load_task: None,
            poll_task: None,
            _accounts: cx.observe_global::<AccountsSnapshotCache>(|_, cx| cx.notify()),
        }
    }

    pub fn focus(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// Closes the open dialog or menu. Returns whether anything was open, so
    /// Escape only navigates back once the page is bare.
    pub fn dismiss_detail(&mut self, cx: &mut Context<Self>) -> bool {
        // Escape backs out of a pending confirmation before closing the dialog.
        if matches!(self.reset, Some((_, ResetState::Confirming))) {
            self.reset = None;
            cx.notify();
            return true;
        }
        if !matches!(self.reset, Some((_, ResetState::Busy))) {
            self.reset = None;
        }
        let menu = self.device_menu.is_open();
        if menu {
            self.close_device_menu(cx);
        }
        let open = self.selected_model.take().is_some() | self.selected_account.take().is_some();
        if open {
            self.detail_hover_period.set(None);
            cx.notify();
        }
        open || menu
    }

    fn close_device_menu(&mut self, cx: &mut Context<Self>) {
        if self.device_menu.begin_close() {
            crate::popover::reap_popup(cx, |page: &mut Self| &mut page.device_menu);
        }
        cx.notify();
    }

    pub fn track(&mut self, visible: bool, active: bool, cx: &mut Context<Self>) -> bool {
        let entering = visible && !self.visible;
        let state = self.state.read(cx);
        let device = state.effective_device_id();
        let target = device
            .clone()
            .filter(|id| Some(id) != state.local_device_id.as_ref());
        let name = device
            .as_ref()
            .and_then(|id| state.devices.iter().find(|d| &d.id == id))
            .map(|device| device.name.clone())
            .unwrap_or_else(|| "This device".into());
        let online = state.engine().is_some()
            && matches!(state.connection, zeron_proto::view::ConnectionStatus::Ready)
            && device
                .as_ref()
                .is_none_or(|id| state.device_online(id, Utc::now()));
        let changed = self.target != target;
        let resumed =
            visible && active && online && (!self.visible || !self.active || !self.online);
        self.visible = visible;
        self.active = active;
        self.online = online;
        self.device_name = name;
        if changed {
            self.target = target;
            self.summary = None;
            self.error = None;
            self.account_error = None;
            self.selected_model = None;
            self.selected_account = None;
            self.load_task = None;
            self.loading = false;
            self.hover_period.set(None);
            self.detail_hover_period.set(None);
        }
        if !visible || !active || !online {
            self.load_task = None;
            self.poll_task = None;
            self.loading = false;
        } else {
            if changed || resumed {
                self.load(false, cx);
            }
            if self.poll_task.is_none() {
                self.poll_task = Some(cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor()
                            .timer(Duration::from_secs(30))
                            .await;
                        if this.update(cx, |page, cx| page.load(false, cx)).is_err() {
                            break;
                        }
                    }
                }));
            }
        }
        entering
    }

    fn load(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.loading || !self.visible || !self.active || !self.online {
            return;
        }
        if self.metric == Metric::Limits
            && !force
            && crate::account_usage_watch::host_refresh_supported(self.state.read(cx), &self.target)
            && cx
                .try_global::<AccountsSnapshotCache>()
                .is_some_and(|cache| cache.0.contains_key(&self.target))
        {
            self.account_error = None;
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.loading = true;
        let target = self.target.clone();
        let days = self.days;
        let limits = self.metric == Metric::Limits;
        let offset = Local::now().offset().local_minus_utc() / 60;
        let history_params = serde_json::json!({"targetDeviceId": target,
            "days": days, "utcOffsetMinutes": offset, "refreshPrices": force});
        let account_params = serde_json::json!({"targetDeviceId": target, "forceUsage": force});
        self.load_task = Some(cx.spawn(async move |this, cx| {
            if !limits {
                let result = crate::attachments::call_with_timeout(
                    &engine,
                    cx.background_executor(),
                    methods::READ_USAGE_HISTORY,
                    history_params,
                    Duration::from_secs(125),
                )
                .await
                .and_then(|value| {
                    serde_json::from_value::<UsageHistorySummary>(value)
                        .map_err(|error| error.to_string())
                });
                if this
                    .update(cx, |page, cx| {
                        if page.target != target || page.days != days {
                            return;
                        }
                        match result {
                            Ok(summary) => {
                                page.summary = Some(summary);
                                page.error = None;
                            }
                            Err(error) => page.error = Some(error),
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            if limits {
                let result = crate::attachments::call_with_timeout(
                    &engine,
                    cx.background_executor(),
                    methods::LIST_AGENT_ACCOUNTS,
                    account_params,
                    Duration::from_secs(35),
                )
                .await
                .and_then(|value| {
                    serde_json::from_value::<AgentAccountsSnapshot>(value)
                        .map_err(|error| error.to_string())
                });
                this.update(cx, |page, cx| {
                    if page.target != target {
                        return;
                    }
                    match result {
                        Ok(snapshot) => {
                            cx.default_global::<AccountsSnapshotCache>()
                                .0
                                .insert(target.clone(), snapshot);
                            page.account_error = None;
                        }
                        Err(error) => page.account_error = Some(error),
                    }
                })
                .ok();
            }
            this.update(cx, |page, cx| {
                if page.target == target && page.days == days {
                    page.loading = false;
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn switch_metric(&mut self, metric: Metric, cx: &mut Context<Self>) {
        if self.metric == metric {
            return;
        }
        self.metric = metric;
        self.selected_model = None;
        self.selected_account = None;
        self.load_task = None;
        self.loading = false;
        self.load(metric == Metric::Limits, cx);
        cx.notify();
    }

    fn select_days(&mut self, days: u32, cx: &mut Context<Self>) {
        if self.days == days {
            return;
        }
        self.days = days;
        self.load_task = None;
        self.loading = false;
        self.summary = None;
        self.selected_model = None;
        self.hover_period.set(None);
        self.load(false, cx);
    }

    /// Redeems one reset credit on the open account. The engine re-probes
    /// the account, so the cached snapshot (and the dialog) show the result.
    fn redeem_reset(&mut self, cx: &mut Context<Self>) {
        let Some(account) = self.selected_account.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.reset = Some((
                account.id,
                ResetState::Failed("Engine not connected. Reconnect and try again.".into()),
            ));
            cx.notify();
            return;
        };
        self.reset = Some((account.id.clone(), ResetState::Busy));
        let target = self.target.clone();
        let params = serde_json::json!({
            "targetDeviceId": target,
            "harness": account.harness,
            "accountId": account.id,
        });
        self.reset_task = Some(cx.spawn(async move |this, cx| {
            let result = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                methods::CONSUME_AGENT_RESET_CREDIT,
                params,
                Duration::from_secs(65),
            )
            .await
            .and_then(|value| {
                serde_json::from_value::<AgentResetCreditReply>(value)
                    .map_err(|error| error.to_string())
            });
            this.update(cx, |page, cx| {
                let state = match result {
                    Ok(reply) => {
                        if page.target == target {
                            cx.default_global::<AccountsSnapshotCache>()
                                .0
                                .insert(target.clone(), reply.snapshot);
                        }
                        ResetState::Done(reset_outcome_text(reply.outcome).into())
                    }
                    Err(error) if error.ends_with("timed out") => ResetState::Failed(
                        "Codex didn't answer in time. Refresh to check whether the reset applied."
                            .into(),
                    ),
                    Err(error) => ResetState::Failed(error.into()),
                };
                page.reset = Some((account.id.clone(), state));
                page.reset_task = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Banked credits with the redeem control, or `None` when the account has
    /// none and nothing to report.
    fn reset_credits(
        &self,
        account: &AgentAccount,
        now: DateTime<Utc>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if account.harness != HarnessId::Codex {
            return None;
        }
        let available = account
            .reset_credits
            .as_ref()
            .map_or(0, |credits| credits.available);
        let state = self
            .reset
            .as_ref()
            .filter(|(id, _)| *id == account.id)
            .map(|(_, state)| state.clone());
        if available == 0 && state.is_none() {
            return None;
        }
        let summary = if available == 0 {
            "No reset credits banked".to_owned()
        } else {
            format!(
                "{available} reset {} banked{}",
                if available == 1 { "credit" } else { "credits" },
                account
                    .reset_credits
                    .as_ref()
                    .and_then(|credits| credits.next_expires_at)
                    .map(|at| format!(" · next expires in {}", resets_in(at, now)))
                    .unwrap_or_default()
            )
        };
        let busy = state == Some(ResetState::Busy);
        let confirming = state == Some(ResetState::Confirming);
        let row = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(icon(icons::TAG).size(px(13.0)).text_color(theme.text_muted))
                    .child(summary),
            )
            .when(available > 0 && !confirming, |row| {
                row.child(
                    button(
                        "usage-reset-use",
                        if busy { "Using…" } else { "Use reset" },
                        ButtonKind::Outline,
                        !busy,
                        theme,
                    )
                    .when(!busy, |button| {
                        button.on_click(cx.listener(|page, _, _, cx| {
                            if let Some(account) = &page.selected_account {
                                page.reset = Some((account.id.clone(), ResetState::Confirming));
                                cx.notify();
                            }
                        }))
                    }),
                )
            });
        let confirm = confirming.then(|| {
            div()
                .p(px(12.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.element_hover)
                .flex()
                .flex_col()
                .gap(px(10.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child("Use a reset credit?"),
                        )
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(theme.text_muted)
                                .child(format!(
                                    "This redeems one credit on {} and clears its current \
                                     rate-limit windows. It cannot be undone.",
                                    account_name(account)
                                )),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            button(
                                "usage-reset-cancel",
                                "Cancel",
                                ButtonKind::Ghost,
                                true,
                                theme,
                            )
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.reset = None;
                                cx.notify();
                            })),
                        )
                        .child(
                            button(
                                "usage-reset-confirm",
                                "Use credit",
                                ButtonKind::Primary,
                                true,
                                theme,
                            )
                            .on_click(cx.listener(|page, _, _, cx| page.redeem_reset(cx))),
                        ),
                )
        });
        let message = match state {
            Some(ResetState::Done(text)) => Some((text, theme.text)),
            Some(ResetState::Failed(text)) => Some((text, theme.warning)),
            _ => None,
        };
        Some(
            div()
                .pt(px(16.0))
                .border_t_1()
                .border_color(theme.border)
                .flex()
                .flex_col()
                .gap(px(10.0))
                .child(row)
                .children(confirm)
                .children(
                    message.map(|(text, color)| {
                        div().text_size(px(12.0)).text_color(color).child(text)
                    }),
                )
                .into_any_element(),
        )
    }

    // ---- header ----

    fn header(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let state = self.state.read(cx);
        let selected_device = state.effective_device_id();
        let devices: Vec<_> = state
            .devices
            .iter()
            .map(|device| (device.id.clone(), device.name.clone()))
            .collect();
        let limits = self.metric == Metric::Limits;
        let window_label = self.summary.as_ref().filter(|_| !limits).map(|summary| {
            format!(
                "{} to {}",
                date_label(summary.since, summary.hourly),
                date_label(summary.until, summary.hourly)
            )
        });

        let mut device = div()
            .id("usage-device")
            .relative()
            .flex()
            .items_center()
            .gap(px(4.0))
            .px(px(6.0))
            .h(px(24.0))
            .rounded(px(6.0))
            .text_size(px(13.0))
            .text_color(theme.text)
            .child(SharedString::from(self.device_name.clone()));
        if devices.len() > 1 {
            device = device
                .role(gpui::Role::Button)
                .tab_index(0)
                .cursor_pointer()
                .hover(|d| d.bg(theme.element_hover))
                .focus_visible(|d| d.border_1().border_color(theme.accent))
                .child(
                    icon(icons::ALT_ARROW_DOWN)
                        .size(px(12.0))
                        .text_color(theme.text_muted),
                )
                // The menu's outside-press closes it first, so a press that
                // found it open must not reopen it on click.
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|page, _, _, _| page.device_menu.note_trigger_press()),
                )
                .on_click(cx.listener(|page, _, _, cx| {
                    if page.device_menu.take_press_was_open() {
                        return;
                    }
                    if page.device_menu.is_open() {
                        page.close_device_menu(cx);
                    } else {
                        page.device_menu.open(());
                        cx.notify();
                    }
                }));
            if self.device_menu.get().is_some() {
                let rows = devices.into_iter().map(|(id, name)| {
                    let selected = selected_device.as_ref() == Some(&id);
                    let key = format!("usage-device-row-{id}");
                    crate::popover::menu_row(theme, selected, key.clone())
                        .id(SharedString::from(key))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(name)),
                        )
                        .when(selected, |row| {
                            row.child(icon(icons::CHECK).size(px(14.0)).text_color(theme.text))
                        })
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.close_device_menu(cx);
                            page.state.update(cx, |state, cx| {
                                state.select_device(id.clone(), cx);
                            });
                            cx.notify();
                        }))
                });
                let menu = crate::popover::popover_card(theme)
                    .min_w(px(200.0))
                    .flex()
                    .flex_col()
                    .children(rows)
                    .on_mouse_down_out(cx.listener(|page, _, _, cx| {
                        if page.device_menu.is_open() {
                            page.close_device_menu(cx);
                        }
                    }))
                    .into_any_element();
                device = device.child(crate::popover::anchored_menu_below(
                    "usage-device-menu",
                    menu,
                    self.device_menu.closing_since(),
                ));
            }
        }

        let metrics = segmented(theme).children(METRICS.into_iter().map(|(metric, label)| {
            self.segment(
                format!("usage-metric-{label}"),
                label,
                self.metric == metric,
                true,
                theme,
            )
            .on_click(cx.listener(move |page, _, _, cx| page.switch_metric(metric, cx)))
        }));
        // The period does not apply to Limits, so it stays in place but
        // disabled rather than shifting the metric toggle.
        let periods = segmented(theme)
            .when(limits, |group| group.opacity(0.5))
            .children(PERIODS.into_iter().map(|(days, label)| {
                let segment = self.segment(
                    format!("usage-days-{days}"),
                    label,
                    self.days == days,
                    !limits,
                    theme,
                );
                if limits {
                    segment
                } else {
                    segment.on_click(cx.listener(move |page, _, _, cx| page.select_days(days, cx)))
                }
            }));
        let refresh = div()
            .id("usage-refresh")
            .role(gpui::Role::Button)
            .aria_label(if limits {
                "Refresh limits"
            } else {
                "Refresh usage"
            })
            .tab_index(0)
            .size(px(28.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .hover(|d| d.bg(theme.element_hover))
            .focus_visible(|d| d.border_1().border_color(theme.accent))
            .tooltip(text_tooltip(if limits {
                "Refresh limits"
            } else {
                "Refresh usage and model pricing"
            }))
            .child(
                icon(icons::REFRESH)
                    .size(px(15.0))
                    .text_color(theme.text_muted),
            )
            .on_click(cx.listener(|page, _, _, cx| page.load(true, cx)));

        div()
            .flex_none()
            .px(px(24.0))
            .py(px(8.0))
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child("Usage"),
                    )
                    .child(
                        div()
                            .px(px(2.0))
                            .text_size(px(13.0))
                            .text_color(theme.text_faint)
                            .child("/"),
                    )
                    .child(device),
            )
            .children(window_label.map(|label| {
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .truncate()
                    .child(label)
            }))
            .child(div().flex_1())
            .child(metrics)
            .child(periods)
            .child(refresh)
            .into_any_element()
    }

    /// One option in a segmented toggle group.
    fn segment(
        &self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        selected: bool,
        enabled: bool,
        theme: &Theme,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id.into())
            .role(gpui::Role::Button)
            .when(enabled, |d| d.tab_index(0).cursor_pointer())
            .px(px(10.0))
            .h(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .text_size(px(12.0))
            .text_color(if selected {
                theme.text
            } else {
                theme.text_muted
            })
            .when(selected, |d| d.bg(theme.surface_raised).shadow_sm())
            .when(enabled && !selected, |d| {
                d.hover(|d| d.text_color(theme.text))
            })
            .focus_visible(|d| d.border_1().border_color(theme.accent))
            .child(label.into())
    }

    // ---- usage ----

    fn history(
        &self,
        summary: &UsageHistorySummary,
        width: f32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let totals = &summary.totals;
        let wide = width >= WIDE;
        let cost = self.metric == Metric::Cost;
        let total_value = value(totals, self.metric);
        let providers: Vec<_> = PROVIDER_ORDER
            .iter()
            .filter_map(|harness| {
                summary.providers.iter().find(|provider| {
                    provider.harness == *harness
                        && (provider.totals.cost_usd() > 0.0
                            || provider.totals.usage.total().unwrap_or(0) > 0)
                })
            })
            .collect();
        let info = if cost {
            format!(
                "API estimate at standard model rates, not your subscription bill. \
                 Reported cost {}, estimated {}. {} of {} turns unpriced. \
                 Cache savings are an API-rate equivalent. Completed Zeron turns only.",
                money(totals.reported_cost_usd),
                money(totals.estimated_cost_usd),
                totals.unpriced_turns,
                totals.turns
            )
        } else {
            "Completed Zeron turns only. Older saved snapshots can have unknown models or \
             incomplete history. Model groups describe the turn model, not individual \
             subagent requests."
                .to_owned()
        };

        let headline = div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .text_size(px(36.0))
                    .line_height(px(40.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(if cost && totals.unpriced_turns == totals.turns {
                        "Unpriced".into()
                    } else {
                        format_value(total_value, self.metric)
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(format!(
                        "{}{}",
                        sessions(totals.sessions),
                        if cost { " · API estimate" } else { "" }
                    ))
                    .child(
                        div()
                            .id("usage-info")
                            .flex()
                            .items_center()
                            .tooltip(text_tooltip(info))
                            .child(
                                icon(icons::INFO_CIRCLE)
                                    .size(px(12.0))
                                    .text_color(theme.text_muted),
                            ),
                    ),
            );
        let summary_column = div()
            .flex_none()
            .w(if wide {
                px(288.0)
            } else {
                px(width.max(180.0))
            })
            .flex()
            .flex_col()
            .gap(px(20.0))
            .child(headline)
            .children(providers.iter().map(|provider| {
                let amount = value(&provider.totals, self.metric);
                let share = if total_value > 0.0 {
                    amount / total_value
                } else {
                    0.0
                };
                let tokens = provider.totals.usage.total().unwrap_or(0) as f64;
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(16.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_1()
                                    .min_w_0()
                                    .items_center()
                                    .gap(px(8.0))
                                    .text_size(px(13.0))
                                    .child(
                                        div()
                                            .flex_none()
                                            .size(px(8.0))
                                            .rounded_full()
                                            .bg(provider_color(provider.harness, theme)),
                                    )
                                    .child(provider_mark(provider.harness, None, 16.0, theme))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .child(provider_label(provider.harness)),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_size(px(11.0))
                                            .text_color(theme.text_muted)
                                            .child(sessions(provider.totals.sessions)),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(13.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(format_value(amount, self.metric)),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(if cost {
                                format!("{} of cost · {} tokens", percent(share), compact(tokens))
                            } else {
                                format!(
                                    "{} of tokens · {}",
                                    percent(share),
                                    money(provider.totals.cost_usd())
                                )
                            }),
                    )
            }));

        let trend = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(heading(
                &format!(
                    "{} {}",
                    if summary.hourly { "Hourly" } else { "Daily" },
                    if cost { "cost" } else { "processed tokens" }
                ),
                theme,
            ))
            .child(self.chart(summary, None, self.metric, theme, cx));

        let stats = [
            ("Processed tokens", count(totals.usage.total())),
            ("Cached input", count(totals.usage.cached_input_tokens)),
            ("Uncached input", count(totals.uncached_input_tokens)),
            ("Output", count(totals.usage.output_tokens)),
            ("Cache savings", money(totals.cache_savings_usd)),
        ];

        let bars: Vec<AnyElement> = if cost {
            share_bar(
                "usage-cost-type",
                "Cost by type",
                cost_segments(totals, theme),
                money,
                theme,
            )
            .into_iter()
            .collect()
        } else {
            share_bar(
                "usage-token-type",
                "Tokens by type",
                token_segments(totals),
                compact,
                theme,
            )
            .into_iter()
            .collect()
        };

        div()
            .flex()
            .flex_col()
            .gap(px(24.0))
            .child(
                div()
                    .flex()
                    .when(!wide, |d| d.flex_col())
                    .gap(px(24.0))
                    .child(summary_column)
                    .child(trend),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(heading("Totals", theme))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_x(px(24.0))
                            .gap_y(px(16.0))
                            .py(px(4.0))
                            .children(
                                stats
                                    .into_iter()
                                    .map(|(label, value)| metric(label, value, 16.0, theme)),
                            ),
                    ),
            )
            .when(!bars.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .when(!wide, |d| d.flex_col())
                        .gap_x(px(48.0))
                        .gap_y(px(32.0))
                        .children(
                            bars.into_iter()
                                .map(|bar| div().flex_1().min_w_0().child(bar)),
                        )
                        .when(wide, |d| d.child(div().flex_1())),
                )
            })
            .child(self.breakdown(summary, &providers, theme, cx))
            .into_any_element()
    }

    fn breakdown(
        &self,
        summary: &UsageHistorySummary,
        providers: &[&zeron_proto::UsageHistoryGroup],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let period = if summary.hourly { "Hour" } else { "Day" };
        let toggle = segmented(theme).children([(false, "Model"), (true, period)].into_iter().map(
            |(by_time, label)| {
                self.segment(
                    format!("usage-breakdown-{label}"),
                    label,
                    self.by_time == by_time,
                    true,
                    theme,
                )
                .on_click(cx.listener(move |page, _, _, cx| {
                    page.by_time = by_time;
                    cx.notify();
                }))
            },
        ));
        let header_row = || {
            div()
                .flex()
                .items_center()
                .gap(px(12.0))
                .py(px(8.0))
                .border_b_1()
                .border_color(theme.border)
                .text_size(px(12.0))
                .text_color(theme.text_muted)
        };
        let body_row = || {
            div()
                .flex()
                .items_center()
                .gap(px(12.0))
                .border_b_1()
                .border_color(theme.border.opacity(0.5))
                .text_size(px(13.0))
                .text_color(theme.text_muted)
        };
        let empty = || {
            div()
                .py(px(24.0))
                .flex()
                .justify_center()
                .text_size(px(13.0))
                .text_color(theme.text_muted)
                .child("No activity in this window.")
        };

        let table = if self.by_time {
            let harnesses: Vec<_> = providers.iter().map(|provider| provider.harness).collect();
            let mut rows = div().flex().flex_col().w_full().child(
                header_row()
                    .child(div().flex_1().min_w_0().child(period))
                    .children(harnesses.iter().map(|harness| {
                        div()
                            .w(px(96.0))
                            .text_right()
                            .truncate()
                            .child(provider_label(*harness))
                    }))
                    .child(div().w(px(88.0)).text_right().child("Total"))
                    .child(div().w(px(80.0)).text_right().child("Tokens")),
            );
            if summary.buckets.is_empty() {
                rows = rows.child(empty());
            }
            for (index, bucket) in summary.buckets.iter().rev().enumerate() {
                let mut by_provider: HashMap<Option<HarnessId>, f64> = HashMap::new();
                for group in &bucket.groups {
                    *by_provider.entry(group.harness).or_default() += group.totals.cost_usd();
                }
                let cost: f64 = by_provider.values().sum();
                let tokens: u64 = bucket
                    .groups
                    .iter()
                    .map(|group| group.totals.usage.total().unwrap_or(0))
                    .sum();
                rows = rows.child(
                    body_row()
                        .id(SharedString::from(format!("usage-period-{index}")))
                        .py(px(8.0))
                        .hover(|d| d.bg(theme.element_hover))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_color(theme.text)
                                .child(date_label(bucket.started_at, summary.hourly)),
                        )
                        .children(harnesses.iter().map(|harness| {
                            div()
                                .w(px(96.0))
                                .text_right()
                                .child(money(by_provider.get(harness).copied().unwrap_or(0.0)))
                        }))
                        .child(
                            div()
                                .w(px(88.0))
                                .text_right()
                                .text_color(theme.text)
                                .child(money(cost)),
                        )
                        .child(div().w(px(80.0)).text_right().child(compact(tokens as f64))),
                );
            }
            rows
        } else {
            let mut models = summary.models.clone();
            models.sort_by(|a, b| {
                value(&b.totals, self.metric).total_cmp(&value(&a.totals, self.metric))
            });
            let total = value(&summary.totals, self.metric);
            let peak = models
                .iter()
                .map(|group| value(&group.totals, self.metric))
                .fold(0.0_f64, f64::max);
            let mut rows = div().flex().flex_col().w_full().child(
                header_row()
                    .child(div().w(px(20.0)).child("#"))
                    .child(div().flex_1().min_w_0().child("Model"))
                    .child(div().w(px(88.0)).text_right().child("Cost"))
                    .child(div().w(px(64.0)).text_right().child("Share"))
                    .child(div().w(px(80.0)).text_right().child("Tokens")),
            );
            if models.is_empty() {
                rows = rows.child(empty());
            }
            for (index, group) in models.into_iter().enumerate() {
                let amount = value(&group.totals, self.metric);
                let unpriced = group.totals.unpriced_turns == group.totals.turns;
                let share = if total > 0.0 && !(self.metric == Metric::Cost && unpriced) {
                    percent(amount / total)
                } else {
                    String::new()
                };
                let fraction = if peak > 0.0 { amount / peak } else { 0.0 };
                let name = group
                    .model
                    .clone()
                    .unwrap_or_else(|| "Unknown model".into());
                let color = provider_color(group.harness, theme);
                let key = ModelKey(group.harness, group.model.clone());
                rows = rows.child(
                    body_row()
                        .id(SharedString::from(format!("usage-model-{index}")))
                        .role(gpui::Role::Button)
                        .aria_label(SharedString::from(format!(
                            "{name} / {}",
                            provider_label(group.harness)
                        )))
                        .tab_index(0)
                        .py(px(10.0))
                        .cursor_pointer()
                        .hover(|d| d.bg(theme.element_hover))
                        .focus_visible(|d| d.bg(theme.element_hover))
                        .child(
                            div()
                                .w(px(20.0))
                                .text_size(px(12.0))
                                .child((index + 1).to_string()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(6.0))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(8.0))
                                        .text_color(theme.text)
                                        .child(provider_mark(
                                            group.harness,
                                            group.model.as_deref(),
                                            14.0,
                                            theme,
                                        ))
                                        .child(div().min_w_0().truncate().child(name)),
                                )
                                .child(
                                    div().h(px(2.0)).w_full().max_w(px(192.0)).child(
                                        div()
                                            .h_full()
                                            .rounded_full()
                                            .when(fraction > 0.0, |bar| {
                                                bar.min_w(px(8.0)).bg(color)
                                            })
                                            .w(relative(fraction as f32)),
                                    ),
                                ),
                        )
                        .child(
                            div()
                                .w(px(88.0))
                                .text_right()
                                .when(!unpriced, |d| d.text_color(theme.text))
                                .child(if unpriced {
                                    "Unpriced".into()
                                } else {
                                    money(group.totals.cost_usd())
                                }),
                        )
                        .child(div().w(px(64.0)).text_right().child(share))
                        .child(
                            div()
                                .w(px(80.0))
                                .text_right()
                                .child(count(group.totals.usage.total())),
                        )
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.selected_model = Some(key.clone());
                            page.detail_hover_period.set(None);
                            cx.notify();
                        })),
                );
            }
            rows
        };

        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.0))
                    .child(heading("Breakdown", theme))
                    .child(toggle),
            )
            .child(table)
            .into_any_element()
    }

    /// Per-provider trend: smoothed lines over light fills, a rounded y-axis,
    /// and a floating per-provider readout at the hovered period.
    fn chart(
        &self,
        summary: &UsageHistorySummary,
        model: Option<&ModelKey>,
        metric: Metric,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let interval = if summary.hourly {
            3_600_000
        } else {
            86_400_000
        };
        let offset = i64::from(Local::now().offset().local_minus_utc()) * 1000;
        let start = (summary.since + offset).div_euclid(interval) * interval - offset;
        let length = ((summary.until - start) / interval + 1).clamp(2, 91) as usize;
        let mut series: HashMap<Option<HarnessId>, Vec<f64>> = HashMap::new();
        for bucket in &summary.buckets {
            let index = ((bucket.started_at - start) / interval).max(0) as usize;
            if index >= length {
                continue;
            }
            for group in &bucket.groups {
                if model.is_some_and(|key| key.0 != group.harness || key.1 != group.model) {
                    continue;
                }
                series
                    .entry(group.harness)
                    .or_insert_with(|| vec![0.0; length])[index] += value(&group.totals, metric);
            }
        }
        // Each series measures from zero, so the scale follows the largest
        // single provider-period rather than the stacked sum.
        let peak = series.values().flatten().copied().fold(0.0_f64, f64::max);
        let (maximum, ticks) = nice_scale(peak, 4);
        let providers: Vec<(Option<HarnessId>, Hsla, Vec<f64>)> = PROVIDER_ORDER
            .iter()
            .filter_map(|harness| {
                let points = series.remove(harness)?;
                Some((*harness, provider_color(*harness, theme), points))
            })
            .collect();
        // Heavier series paint first so a lighter one is not buried.
        let mut painted: Vec<(Hsla, Vec<f64>)> = providers
            .iter()
            .map(|(_, color, points)| (*color, points.clone()))
            .collect();
        painted.sort_by(|a, b| b.1.iter().sum::<f64>().total_cmp(&a.1.iter().sum::<f64>()));

        let detail = model.is_some();
        let hover_state = if detail {
            self.detail_hover_period.clone()
        } else {
            self.hover_period.clone()
        };
        let hover = hover_state.get().filter(|hover| hover.index < length);
        let bounds: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::new(Cell::new(None));
        let measure = bounds.clone();
        let grid = theme.border;
        let cursor = theme.text_muted;
        let paint_ticks = ticks.clone();
        let hover_index = hover.map(|hover| hover.index);
        let plot = canvas(
            move |bounds, _, _| {
                measure.set(Some(bounds));
            },
            move |bounds, _, window, _| {
                let left = f32::from(bounds.origin.x);
                let top = f32::from(bounds.origin.y);
                let w = f32::from(bounds.size.width);
                let h = f32::from(bounds.size.height);
                let y_of = |value: f64| {
                    if maximum > 0.0 {
                        top + h - (value / maximum) as f32 * (h - PLOT_TOP)
                    } else {
                        top + h
                    }
                };
                let x_of = |index: usize| left + w * index as f32 / (length - 1) as f32;
                for tick in &paint_ticks {
                    let y = y_of(*tick);
                    let mut path = PathBuilder::stroke(px(1.0));
                    path.move_to(point(px(left), px(y)));
                    path.line_to(point(px(left + w), px(y)));
                    if let Ok(path) = path.build() {
                        window.paint_path(path, grid);
                    }
                }
                let curves: Vec<Curve> = painted
                    .iter()
                    .map(|(color, values)| {
                        let points: Vec<_> = values
                            .iter()
                            .enumerate()
                            .map(|(index, value)| (x_of(index), y_of(*value)))
                            .collect();
                        let tangents = monotone_tangents(&points);
                        (*color, points, tangents)
                    })
                    .collect();
                // Fills first, then every stroke, so no series covers another's line.
                for (color, points, tangents) in &curves {
                    let mut area = PathBuilder::fill();
                    area.move_to(point(px(points[0].0), px(points[0].1)));
                    trace(&mut area, points, tangents);
                    area.line_to(point(px(left + w), px(top + h)));
                    area.line_to(point(px(left), px(top + h)));
                    area.close();
                    if let Ok(path) = area.build() {
                        window.paint_path(path, color.opacity(0.12));
                    }
                }
                for (color, points, tangents) in &curves {
                    let mut line = PathBuilder::stroke(px(2.0));
                    line.move_to(point(px(points[0].0), px(points[0].1)));
                    trace(&mut line, points, tangents);
                    if let Ok(path) = line.build() {
                        window.paint_path(path, *color);
                    }
                }
                if let Some(index) = hover_index {
                    let x = x_of(index);
                    let mut path = PathBuilder::stroke(px(1.0));
                    path.move_to(point(px(x), px(top + PLOT_TOP)));
                    path.line_to(point(px(x), px(top + h)));
                    if let Ok(path) = path.build() {
                        window.paint_path(path, cursor);
                    }
                }
            },
        )
        .size_full();

        let axis = div()
            .relative()
            .flex_none()
            .w(px(AXIS_WIDTH))
            .h(px(CHART_HEIGHT))
            .when(maximum > 0.0, |axis| {
                axis.children(ticks.iter().map(|tick| {
                    let y = CHART_HEIGHT - (*tick / maximum) as f32 * (CHART_HEIGHT - PLOT_TOP);
                    div()
                        .absolute()
                        .right_0()
                        .top(px(y - 6.0))
                        .h(px(12.0))
                        .text_size(px(10.0))
                        .line_height(px(12.0))
                        .text_color(theme.text_muted)
                        .child(if *tick == 0.0 {
                            "0".to_owned()
                        } else {
                            axis_value(*tick, metric)
                        })
                }))
            });

        let tooltip = hover.map(|hover| {
            let period = start + hover.index as i64 * interval;
            let rows: Vec<_> = providers
                .iter()
                .map(|(harness, _, points)| (*harness, points[hover.index]))
                .collect();
            let total: f64 = rows.iter().map(|(_, value)| value).sum();
            let plot_width = hover.width;
            let height = 46.0 + 18.0 * rows.len() as f32;
            let gap = 12.0;
            let left = if hover.x + gap + TOOLTIP_WIDTH <= plot_width {
                hover.x + gap
            } else {
                hover.x - gap - TOOLTIP_WIDTH
            };
            let top = if hover.y + gap + height <= CHART_HEIGHT {
                hover.y + gap
            } else {
                hover.y - gap - height
            };
            div()
                .absolute()
                .left(px(left.clamp(0.0, (plot_width - TOOLTIP_WIDTH).max(0.0))))
                .top(px(top.clamp(0.0, (CHART_HEIGHT - height).max(0.0))))
                .w(px(TOOLTIP_WIDTH))
                .px(px(10.0))
                .py(px(8.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(theme.border.opacity(0.5))
                .bg(theme.surface_overlay)
                .shadow_lg()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .mb(px(2.0))
                        .text_color(theme.text_muted)
                        .child(date_label(period, summary.hourly)),
                )
                .children(rows.iter().map(|(harness, value)| {
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(12.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .min_w_0()
                                .text_color(theme.text_muted)
                                .child(provider_mark(*harness, None, 12.0, theme))
                                .child(div().truncate().child(provider_label(*harness))),
                        )
                        .child(format_value(*value, metric))
                }))
                .child(
                    div()
                        .mt(px(2.0))
                        .pt(px(4.0))
                        .border_t_1()
                        .border_color(theme.border)
                        .flex()
                        .justify_between()
                        .gap(px(12.0))
                        .child(div().text_color(theme.text_muted).child("Total"))
                        .child(format_value(total, metric)),
                )
        });

        let x_label = |index: usize| {
            date_label(start + index as i64 * interval, summary.hourly).to_uppercase()
        };
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div().flex().gap(px(8.0)).child(axis).child(
                    div()
                        .id(if detail {
                            "usage-detail-trend"
                        } else {
                            "usage-trend"
                        })
                        .relative()
                        .flex_1()
                        .min_w_0()
                        .h(px(CHART_HEIGHT))
                        .child(plot)
                        .children(tooltip)
                        .on_mouse_move(cx.listener(
                            move |page, event: &gpui::MouseMoveEvent, _, cx| {
                                let Some(bounds) = bounds.get() else {
                                    return;
                                };
                                let width = f32::from(bounds.size.width).max(1.0);
                                let x =
                                    f32::from(event.position.x - bounds.origin.x).clamp(0.0, width);
                                let y = f32::from(event.position.y - bounds.origin.y)
                                    .clamp(0.0, CHART_HEIGHT);
                                let index = ((x / width) * (length - 1) as f32).round() as usize;
                                let hover = if detail {
                                    &page.detail_hover_period
                                } else {
                                    &page.hover_period
                                };
                                let next = Some(Hover {
                                    index: index.min(length - 1),
                                    x,
                                    y,
                                    width,
                                });
                                if hover.replace(next) != next {
                                    cx.notify();
                                }
                            },
                        ))
                        .on_hover(cx.listener(move |page, hovered: &bool, _, cx| {
                            if !hovered {
                                let hover = if detail {
                                    &page.detail_hover_period
                                } else {
                                    &page.hover_period
                                };
                                hover.set(None);
                                cx.notify();
                            }
                        })),
                ),
            )
            .child(
                div()
                    .pl(px(AXIS_WIDTH + 8.0))
                    .flex()
                    .justify_between()
                    .text_size(px(10.0))
                    .text_color(theme.text_muted)
                    .child(x_label(0))
                    .child(x_label(length / 2))
                    .child(x_label(length - 1)),
            )
            .into_any_element()
    }

    /// Stand-in with the loaded page's shape while the first answer is out.
    fn skeleton(&self, width: f32, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let delta = motion::pulse_delta(&ZERON_PULSE, cx.entity_id(), cx);
        let pulse = 0.35 + 0.4 * motion::pulse_wave(motion::staggered_phase(delta, 0, 0.0));
        let wash = theme.ink(0.08);
        let bar = |w: Option<f32>, h: f32| {
            let bar = div()
                .flex_none()
                .h(px(h))
                .rounded(px(4.0))
                .bg(wash)
                .opacity(pulse);
            match w {
                Some(w) => bar.w(px(w)),
                None => bar.w_full(),
            }
        };
        let wide = width >= WIDE;
        div()
            .flex()
            .flex_col()
            .gap(px(24.0))
            .child(
                div()
                    .flex()
                    .when(!wide, |d| d.flex_col())
                    .gap(px(24.0))
                    .child(
                        div()
                            .flex_none()
                            .w(if wide {
                                px(288.0)
                            } else {
                                px(width.max(180.0))
                            })
                            .flex()
                            .flex_col()
                            .gap(px(20.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.0))
                                    .child(bar(Some(144.0), 40.0))
                                    .child(bar(Some(128.0), 16.0)),
                            )
                            .children((0..2).map(|_| {
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.0))
                                    .child(
                                        div()
                                            .flex()
                                            .justify_between()
                                            .child(bar(Some(96.0), 14.0))
                                            .child(bar(Some(56.0), 14.0)),
                                    )
                                    .child(bar(Some(144.0), 16.0))
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(bar(Some(96.0), 20.0))
                            .child(
                                div()
                                    .pl(px(AXIS_WIDTH + 8.0))
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.0))
                                    .child(bar(None, CHART_HEIGHT))
                                    .child(bar(None, 16.0)),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(heading("Totals", theme))
                    .child(
                        div().flex().flex_wrap().gap(px(24.0)).children(
                            [
                                "Processed tokens",
                                "Cached input",
                                "Uncached input",
                                "Output",
                                "Cache savings",
                            ]
                            .into_iter()
                            .map(|label| {
                                div()
                                    .min_w(px(120.0))
                                    .flex_1()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.0))
                                    .child(
                                        div()
                                            .text_size(px(12.0))
                                            .text_color(theme.text_muted)
                                            .child(label),
                                    )
                                    .child(bar(Some(64.0), 24.0))
                            }),
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(heading("Breakdown", theme))
                    .child(bar(None, 176.0)),
            )
            .into_any_element()
    }

    // ---- limits ----

    fn limits(&self, width: f32, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let snapshot = cx
            .try_global::<AccountsSnapshotCache>()
            .and_then(|cache| cache.0.get(&self.target));
        let mut content = div().flex().flex_col().gap(px(32.0));
        let Some(snapshot) = snapshot else {
            return content
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(theme.text_muted)
                        .child("No account limits available. Refresh to check this device."),
                )
                .into_any_element();
        };
        let now = Utc::now();
        let wide = width >= WIDE;
        for harness in LIMIT_PROVIDERS {
            let accounts: Vec<_> = snapshot
                .accounts
                .iter()
                .filter(|account| {
                    account.harness == harness && account.auth_kind == Some(AgentAuthKind::Oauth)
                })
                .collect();
            if accounts.is_empty() {
                continue;
            }
            let color = provider_color(Some(harness), theme);
            let mut section = div().flex().flex_col().gap(px(12.0)).child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(provider_mark(Some(harness), None, 16.0, theme))
                    .child(provider_label(Some(harness))),
            );
            for (label, labels) in [
                ("Session", &["5h", "Session"][..]),
                ("Weekly", &["7d", "Week", "Weekly"][..]),
            ] {
                // Snapshot order, so each account keeps its column across windows.
                let members: Vec<_> = accounts
                    .iter()
                    .filter_map(|account| {
                        let window = account
                            .usage_windows
                            .iter()
                            .find(|window| labels.contains(&window.label.as_str()))?;
                        (window.used_fraction.is_finite() && window.used_fraction >= 0.0)
                            .then_some((*account, window))
                    })
                    .collect();
                if members.is_empty() {
                    continue;
                }
                let stale = members.iter().any(|(account, window)| {
                    !self.online
                        || account.usage_error.is_some()
                        || account
                            .usage_fetched_at
                            .is_none_or(|at| now.timestamp_millis().saturating_sub(at) > 300_000)
                        || window.resets_at.is_some_and(|at| at <= now)
                });
                let remaining: f32 = members
                    .iter()
                    .map(|(_, window)| remaining_fraction(window))
                    .sum::<f32>()
                    / members.len() as f32;
                // The soonest reset that hands anything back.
                let refill = members
                    .iter()
                    .filter(|(_, window)| window.used_fraction > 0.0 && window.resets_at.is_some())
                    .min_by_key(|(_, window)| window.resets_at)
                    .map(|(_, window)| window.used_fraction.clamp(0.0, 1.0) / members.len() as f32);
                let many = members.len() > 1;
                let summary = div()
                    .flex_none()
                    .w(px(176.0))
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(label),
                    )
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .text_size(px(30.0))
                                    .line_height(px(36.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(format!("{:.0}%", remaining * 100.0)),
                            )
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .text_color(theme.text_muted)
                                    .child("left"),
                            ),
                    )
                    .children(refill.filter(|_| many).map(|share| {
                        div()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(format!("↻ +{:.0}%", share * 100.0))
                    }))
                    .when(stale, |d| {
                        d.child(
                            div()
                                .text_size(px(12.0))
                                .text_color(theme.text_muted)
                                .child("Stale snapshot"),
                        )
                    });
                let segments =
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .gap(px(4.0))
                        .children(members.into_iter().map(|(account, window)| {
                            let left = remaining_fraction(window);
                            let name = account_name(account);
                            let countdown = window.resets_at.map(|at| resets_in(at, now));
                            let resets = format_reset(window.resets_at, now)
                                .unwrap_or_else(|| "reset unavailable".into());
                            let credits = account
                                .reset_credits
                                .as_ref()
                                .map_or(0, |credits| credits.available);
                            let banked = if credits > 0 {
                                format!(
                                    " · {credits} reset {} banked",
                                    if credits == 1 { "credit" } else { "credits" }
                                )
                            } else {
                                String::new()
                            };
                            let detail = account.clone();
                            div()
                                .id(SharedString::from(format!(
                                    "usage-limit-{}-{label}",
                                    account.id
                                )))
                                .role(gpui::Role::Button)
                                .aria_label(SharedString::from(format!(
                                    "{name}: {:.0}% left, {resets}",
                                    left * 100.0
                                )))
                                .tab_index(0)
                                .flex_1()
                                .min_w_0()
                                .h(px(32.0))
                                .relative()
                                .rounded(px(6.0))
                                .overflow_hidden()
                                .bg(theme.element_hover)
                                .cursor_pointer()
                                .focus_visible(|d| d.border_1().border_color(theme.accent))
                                .tooltip(text_tooltip(format!(
                                    "{name}: {:.0}% left · {resets}{banked}",
                                    left * 100.0
                                )))
                                .child(
                                    div()
                                        .absolute()
                                        .left_0()
                                        .top_0()
                                        .h_full()
                                        .w(relative(left))
                                        .rounded(px(6.0))
                                        .bg(color.opacity(0.35)),
                                )
                                .child(
                                    div()
                                        .relative()
                                        .h_full()
                                        .px(px(8.0))
                                        .flex()
                                        .items_center()
                                        .gap(px(6.0))
                                        .text_size(px(12.0))
                                        .when(many, |d| {
                                            d.child(
                                                div()
                                                    .min_w_0()
                                                    .truncate()
                                                    .font_weight(gpui::FontWeight::MEDIUM)
                                                    .child(SharedString::from(name.clone())),
                                            )
                                        })
                                        .child(
                                            div()
                                                .flex_none()
                                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                                .child(format!("{:.0}%", left * 100.0)),
                                        )
                                        .child(div().flex_1())
                                        // Countdown and credits get their own plate:
                                        // the fill runs under them otherwise.
                                        .when(countdown.is_some() || credits > 0, |d| {
                                            d.child(
                                                div()
                                                    .flex_none()
                                                    .flex()
                                                    .items_center()
                                                    .gap(px(6.0))
                                                    .px(px(6.0))
                                                    .py(px(2.0))
                                                    .rounded(px(4.0))
                                                    .bg(theme.bg.opacity(0.85))
                                                    .text_size(px(11.0))
                                                    .children(
                                                        countdown
                                                            .as_ref()
                                                            .map(|text| format!("↻ {text}")),
                                                    )
                                                    .when(countdown.is_some() && credits > 0, |d| {
                                                        d.child(
                                                            div()
                                                                .text_color(theme.text_muted)
                                                                .child("·"),
                                                        )
                                                    })
                                                    .when(credits > 0, |d| {
                                                        d.child(
                                                            div()
                                                                .flex()
                                                                .items_center()
                                                                .gap(px(2.0))
                                                                .font_weight(
                                                                    gpui::FontWeight::SEMIBOLD,
                                                                )
                                                                .child(
                                                                    icon(icons::TAG)
                                                                        .size(px(11.0))
                                                                        .text_color(theme.text),
                                                                )
                                                                .child(credits.to_string()),
                                                        )
                                                    }),
                                            )
                                        }),
                                )
                                .on_click(cx.listener(move |page, _, _, cx| {
                                    if page.reset.as_ref().is_some_and(|(id, state)| {
                                        *id != detail.id || *state != ResetState::Busy
                                    }) {
                                        page.reset = None;
                                    }
                                    page.selected_account = Some(detail.clone());
                                    cx.notify();
                                }))
                        }));
                section = section.child(
                    div()
                        .p(px(16.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(theme.border.opacity(0.6))
                        .flex()
                        .when(wide, |d| d.items_center())
                        .when(!wide, |d| d.flex_col())
                        .gap_x(px(24.0))
                        .gap_y(px(12.0))
                        .child(summary)
                        .child(segments),
                );
            }
            for account in accounts
                .iter()
                .filter(|account| account.usage_windows.is_empty() || account.usage_error.is_some())
            {
                section = section.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(format!(
                            "{}: {}",
                            account_name(account),
                            account
                                .usage_error
                                .as_deref()
                                .unwrap_or("Limits not reported")
                        )),
                );
            }
            content = content.child(section);
        }
        content
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(
                        "Pool percentages are unweighted account averages, not combined token \
                         capacity. Plans can have different allowances. Limits are separate \
                         from token history.",
                    ),
            )
            .into_any_element()
    }

    // ---- dialogs ----

    fn detail(
        &self,
        summary: Option<&UsageHistorySummary>,
        viewport: gpui::Size<Pixels>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (mark, title, description, body): (_, String, String, gpui::Div) =
            if let Some(selected) = &self.selected_account {
                // The cache holds the latest probe (polls, a redemption); the
                // selection is only the row as it was when clicked.
                let account = cx
                    .try_global::<AccountsSnapshotCache>()
                    .and_then(|cache| cache.0.get(&self.target))
                    .and_then(|snapshot| {
                        snapshot.accounts.iter().find(|account| {
                            account.id == selected.id && account.harness == selected.harness
                        })
                    })
                    .cloned()
                    .unwrap_or_else(|| selected.clone());
                let account = &account;
                let now = Utc::now();
                let mut windows = div().flex().flex_col().gap(px(4.0));
                for window in &account.usage_windows {
                    windows = windows.child(limit_window_row(
                        window,
                        provider_color(Some(account.harness), theme),
                        now,
                        theme,
                    ));
                }
                let body = div()
                    .flex()
                    .flex_col()
                    .gap(px(16.0))
                    .child(windows)
                    .children(account.usage_error.as_ref().map(|error| {
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.warning)
                            .child(SharedString::from(error.clone()))
                    }))
                    .when(account.usage_windows.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(px(13.0))
                                .text_color(theme.text_muted)
                                .child("Limits not reported."),
                        )
                    })
                    .children(self.reset_credits(account, now, theme, cx));
                (
                    (Some(account.harness), None),
                    account_name(account),
                    format!(
                        "{} · {}{}",
                        provider_label(Some(account.harness)),
                        account.plan_label.as_deref().unwrap_or("Unknown plan"),
                        if account.active { " · Active" } else { "" }
                    ),
                    body,
                )
            } else if let (Some(key), Some(summary)) = (&self.selected_model, summary) {
                let group = summary
                    .models
                    .iter()
                    .find(|group| key.0 == group.harness && key.1 == group.model)?;
                let totals = &group.totals;
                let unpriced = totals.unpriced_turns == totals.turns;
                let tokens = totals.usage.total().unwrap_or(0) as f64;
                let mut stats = vec![
                    (
                        "Cost",
                        if unpriced {
                            "Unpriced".into()
                        } else {
                            money(totals.cost_usd())
                        },
                    ),
                    ("Tokens", count(totals.usage.total())),
                ];
                if !unpriced && totals.unpriced_turns == 0 && tokens > 0.0 {
                    stats.push(("Per 1M tokens", money(totals.cost_usd() / tokens * 1e6)));
                }
                if let Some(rate) = totals
                    .usage
                    .cache_hit_rate()
                    .filter(|_| totals.incomplete_cache_turns == 0)
                {
                    stats.push(("Cache hit", percent(rate)));
                }
                let share = summary.totals.cost_usd();
                let mut bars: Vec<AnyElement> = Vec::new();
                if !unpriced {
                    bars.extend(share_bar(
                        "usage-detail-cost-type",
                        "Cost by type",
                        cost_segments(totals, theme),
                        money,
                        theme,
                    ));
                }
                bars.extend(share_bar(
                    "usage-detail-token-type",
                    "Tokens by type",
                    token_segments(totals),
                    compact,
                    theme,
                ));
                let body = div()
                    .flex()
                    .flex_col()
                    .gap(px(32.0))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_x(px(24.0))
                            .gap_y(px(16.0))
                            .children(
                                stats
                                    .into_iter()
                                    .map(|(label, value)| metric(label, value, 24.0, theme)),
                            ),
                    )
                    // Unpriced cost is unknown, not zero, so its trend shows tokens.
                    .child(self.chart(
                        summary,
                        Some(key),
                        if unpriced {
                            Metric::Tokens
                        } else {
                            self.metric
                        },
                        theme,
                        cx,
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_x(px(40.0))
                            .gap_y(px(32.0))
                            .children(
                                bars.into_iter()
                                    .map(|bar| div().flex_1().min_w(px(240.0)).child(bar)),
                            ),
                    )
                    .when(totals.unpriced_turns > 0, |d| {
                        d.child(
                            div()
                                .text_size(px(12.0))
                                .text_color(theme.text_muted)
                                .child(format!(
                                    "{} of {} turns have no known price",
                                    totals.unpriced_turns, totals.turns
                                )),
                        )
                    });
                (
                    (group.harness, group.model.clone()),
                    group
                        .model
                        .clone()
                        .unwrap_or_else(|| "Unknown model".into()),
                    if unpriced || share <= 0.0 {
                        provider_label(group.harness).to_owned()
                    } else {
                        format!(
                            "{} · {} of cost",
                            provider_label(group.harness),
                            percent(totals.cost_usd() / share)
                        )
                    },
                    body,
                )
            } else {
                return None;
            };

        let width = (f32::from(viewport.width) - 40.0).clamp(280.0, 768.0);
        let card = div()
            .id("usage-detail-card")
            .w(px(width))
            .max_h(px(f32::from(viewport.height) * 0.9))
            .overflow_y_scroll()
            .p(px(24.0))
            .rounded(px(16.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_dialog)
            .text_color(theme.text)
            .flex()
            .flex_col()
            .gap(px(24.0))
            .on_mouse_down_out(cx.listener(|page, _, _, cx| {
                page.dismiss_detail(cx);
            }))
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(px(16.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(provider_mark(mark.0, mark.1.as_deref(), 20.0, theme))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(px(16.0))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .child(title),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .text_color(theme.text_muted)
                                    .child(description),
                            ),
                    )
                    .child(
                        div()
                            .id("usage-detail-close")
                            .role(gpui::Role::Button)
                            .aria_label("Close")
                            .tab_index(0)
                            .flex_none()
                            .size(px(28.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .hover(|d| d.bg(theme.element_hover))
                            .focus_visible(|d| d.border_1().border_color(theme.accent))
                            .child(
                                icon(icons::CLOSE)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.dismiss_detail(cx);
                            })),
                    ),
            )
            .child(body)
            .into_any_element();
        Some(crate::popover::modal("usage-detail", viewport, card))
    }
}

impl Render for UsageDashboard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let viewport = window.viewport_size();
        let width = (f32::from(viewport.width) - 340.0).clamp(240.0, 1050.0);
        let limits = self.metric == Metric::Limits;
        let mut content = div()
            .w_full()
            .max_w(px(1100.0))
            .mx_auto()
            .px(px(24.0))
            .py(px(24.0))
            .flex()
            .flex_col()
            .gap(px(24.0));

        let mut notes: Vec<(String, Hsla)> = Vec::new();
        if !self.online {
            notes.push((
                format!(
                    "{} is offline. Saved figures may be stale.",
                    self.device_name
                ),
                theme.warning,
            ));
        }
        if let Some(summary) = self.summary.as_ref().filter(|_| !limits) {
            let totals = &summary.totals;
            if totals.incomplete_turns > 0
                || totals.incomplete_cache_turns > 0
                || summary.skipped_sources > 0
            {
                notes.push((
                    format!(
                        "Partial totals: {} turns have incomplete input/output counts; {} have \
                         incomplete cache counts; {} saved sources unavailable.",
                        totals.incomplete_turns,
                        totals.incomplete_cache_turns,
                        summary.skipped_sources
                    ),
                    theme.text_muted,
                ));
            }
            if summary.pricing_error.is_some() {
                notes.push((
                    "Model pricing could not refresh. Previously saved prices are retained.".into(),
                    theme.text_muted,
                ));
            }
        }
        if !notes.is_empty() {
            content =
                content.child(div().flex().flex_col().gap(px(4.0)).children(
                    notes.into_iter().map(|(note, color)| {
                        div().text_size(px(13.0)).text_color(color).child(note)
                    }),
                ));
        }

        let error = if limits {
            self.account_error.as_ref()
        } else {
            self.error.as_ref()
        };
        if let Some(error) = error {
            content = content.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme.warning)
                            .child(SharedString::from(error.clone())),
                    )
                    .child(
                        self.segment("usage-retry", "Retry", false, true, &theme)
                            .border_1()
                            .border_color(theme.border)
                            .on_click(cx.listener(|page, _, _, cx| page.load(true, cx))),
                    ),
            );
        }

        // A refresh keeps the previous figures on screen, muted until it answers.
        let muted = self.loading;
        if limits {
            content = content.child(
                div()
                    .when(muted, |d| d.opacity(0.4))
                    .child(self.limits(width, &theme, cx)),
            );
        } else if let Some(summary) = self.summary.as_ref() {
            if summary.totals.turns == 0 {
                content = content.child(
                    div()
                        .py(px(48.0))
                        .flex()
                        .justify_center()
                        .text_size(px(13.0))
                        .text_color(theme.text_muted)
                        .child(
                            "No activity in this window. Completed turns with provider-reported \
                             token counts appear here.",
                        ),
                );
            } else {
                content = content.child(
                    div()
                        .when(muted, |d| d.opacity(0.4))
                        .child(self.history(summary, width, &theme, cx)),
                );
            }
        } else if self.loading {
            content = content.child(self.skeleton(width, &theme, cx));
        } else if error.is_none() {
            content = content.child(
                div()
                    .text_size(px(13.0))
                    .text_color(theme.text_muted)
                    .child("Usage history needs an updated engine on the selected device."),
            );
        }

        div()
            .id("usage-dashboard")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .pt(px(Theme::TITLEBAR_HEIGHT))
            .text_color(theme.text)
            .bg(theme.surface)
            .on_key_down(cx.listener(|page, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.modifiers.modified() {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "c" => page.switch_metric(Metric::Cost, cx),
                    "t" => page.switch_metric(Metric::Tokens, cx),
                    "l" => page.switch_metric(Metric::Limits, cx),
                    _ => {}
                }
            }))
            .child(self.header(&theme, cx))
            .child(
                div()
                    .id("usage-page-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(content),
            )
            .children(self.detail(self.summary.as_ref(), viewport, &theme, cx))
    }
}

// ---- pieces ----

/// Container for a segmented toggle group; options come from `segment`.
fn segmented(theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(2.0))
        .p(px(2.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.element_hover)
}

struct Segment {
    label: &'static str,
    value: f64,
    color: Hsla,
}

/// A neutral step between background and foreground.
fn ink(theme: &Theme, share: f32) -> Hsla {
    mix(theme.bg, theme.text, share)
}

/// Token-type hues (blue, emerald, amber, pink), kept clear of the provider
/// colours so a type mix never reads as a provider.
const TYPE_INPUT: u32 = 0x60a5fa;
const TYPE_CACHE_READ: u32 = 0x34d399;
const TYPE_CACHE_WRITE: u32 = 0xfbbf24;
const TYPE_OUTPUT: u32 = 0xf472b6;

fn type_color(hex: u32) -> Hsla {
    Hsla::from(gpui::rgb(hex))
}

fn token_segments(totals: &UsageHistoryTotals) -> Vec<Segment> {
    let usage = totals.usage;
    let tokens = |value: Option<u64>| value.unwrap_or(0) as f64;
    vec![
        Segment {
            label: "Input",
            value: tokens(totals.uncached_input_tokens),
            color: type_color(TYPE_INPUT),
        },
        Segment {
            label: "Cache read",
            value: tokens(usage.cached_input_tokens),
            color: type_color(TYPE_CACHE_READ),
        },
        Segment {
            label: "Cache write",
            value: tokens(usage.cache_write_input_tokens),
            color: type_color(TYPE_CACHE_WRITE),
        },
        Segment {
            label: "Output",
            value: tokens(usage.output_tokens),
            color: type_color(TYPE_OUTPUT),
        },
    ]
}

fn cost_segments(totals: &UsageHistoryTotals, theme: &Theme) -> Vec<Segment> {
    let [input, cache_read, cache_write, output, other] = totals.category_cost_usd;
    vec![
        Segment {
            label: "Input",
            value: input,
            color: type_color(TYPE_INPUT),
        },
        Segment {
            label: "Cache read",
            value: cache_read,
            color: type_color(TYPE_CACHE_READ),
        },
        Segment {
            label: "Cache write",
            value: cache_write,
            color: type_color(TYPE_CACHE_WRITE),
        },
        Segment {
            label: "Output",
            value: output,
            color: type_color(TYPE_OUTPUT),
        },
        // Below a cent the unsplit remainder is rounding, not usage.
        Segment {
            label: "Other",
            value: if other >= 0.005 { other } else { 0.0 },
            color: ink(theme, 0.44),
        },
    ]
}

/// One part-to-whole bar with its legend. Empty segments are left out, and
/// nothing renders without a total.
fn share_bar(
    id: &str,
    label: &str,
    segments: Vec<Segment>,
    format: fn(f64) -> String,
    theme: &Theme,
) -> Option<AnyElement> {
    let visible: Vec<_> = segments
        .into_iter()
        .filter(|segment| segment.value > 0.0)
        .collect();
    let total: f64 = visible.iter().map(|segment| segment.value).sum();
    if total.is_nan() || total <= 0.0 {
        return None;
    }
    let last = visible.len() - 1;
    let bar =
        div()
            .w_full()
            .h(px(8.0))
            .flex()
            .gap(px(2.0))
            .children(visible.iter().enumerate().map(|(index, segment)| {
                div()
                    .id(SharedString::from(format!("{id}-{index}")))
                    .h_full()
                    .min_w(px(4.0))
                    .w(relative((segment.value / total) as f32))
                    .rounded(px(2.0))
                    .when(index == 0, |d| d.rounded_l(px(4.0)))
                    .when(index == last, |d| d.rounded_r(px(4.0)))
                    .bg(segment.color)
                    .tooltip(text_tooltip(format!(
                        "{} · {} · {}",
                        segment.label,
                        format(segment.value),
                        percent(segment.value / total)
                    )))
            }));
    let legend = div()
        .flex()
        .flex_wrap()
        .gap_x(px(16.0))
        .gap_y(px(4.0))
        .text_size(px(12.0))
        .children(visible.iter().map(|segment| {
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(div().size(px(8.0)).rounded(px(2.0)).bg(segment.color))
                .child(div().text_color(theme.text_muted).child(segment.label))
                .child(format(segment.value))
        }));
    Some(
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(heading(label, theme))
            .child(bar)
            .child(legend)
            .into_any_element(),
    )
}

/// One account window: label and percent, a bar of quota left with a hairline
/// where even spending would be, and the countdown.
fn limit_window_row(
    window: &AgentUsageWindow,
    color: Hsla,
    now: DateTime<Utc>,
    theme: &Theme,
) -> gpui::Div {
    let left = remaining_fraction(window);
    // The fill is quota left, so the even-spending mark is the time left.
    let time_left = window_span(&window.label)
        .zip(window.resets_at)
        .map(|(span, at)| {
            ((at - now).num_seconds() as f32 / span.num_seconds() as f32).clamp(0.0, 1.0)
        });
    div()
        .flex()
        .items_center()
        .gap(px(16.0))
        .child(
            div()
                .flex_none()
                .w(px(176.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(theme.text_muted)
                        .child(SharedString::from(window.label.clone())),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child(format!("{:.0}% left", left * 100.0)),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .h(px(24.0))
                .relative()
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .top(px(6.0))
                        .bottom(px(6.0))
                        .rounded_full()
                        .bg(theme.element_hover),
                )
                .when(left > 0.0, |d| {
                    d.child(
                        div()
                            .absolute()
                            .left_0()
                            .top(px(6.0))
                            .bottom(px(6.0))
                            .w(relative(left))
                            .rounded_full()
                            .bg(color),
                    )
                })
                .children(time_left.map(|share| {
                    div()
                        .absolute()
                        .top(px(2.0))
                        .bottom(px(2.0))
                        .left(relative(share))
                        .w(px(1.0))
                        .bg(theme.text.opacity(0.6))
                })),
        )
        .child(
            div()
                .flex_none()
                .w(px(112.0))
                .text_right()
                .text_size(px(12.0))
                .text_color(theme.text_muted)
                .child(
                    window
                        .resets_at
                        .map(|at| format!("resets in {}", resets_in(at, now)))
                        .unwrap_or_default(),
                ),
        )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonKind {
    Primary,
    Outline,
    Ghost,
}

/// A text button for dialog actions. Disabled buttons take no clicks.
fn button(
    id: &'static str,
    label: &'static str,
    kind: ButtonKind,
    enabled: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .role(gpui::Role::Button)
        .flex_none()
        .h(px(28.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .text_size(px(12.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .map(|button| match kind {
            ButtonKind::Primary => button.bg(theme.solid).text_color(theme.on_solid),
            ButtonKind::Outline => button
                .border_1()
                .border_color(theme.border_strong)
                .text_color(theme.text),
            ButtonKind::Ghost => button.text_color(theme.text_muted),
        })
        .when(enabled, |button| {
            button
                .tab_index(0)
                .cursor_pointer()
                .focus_visible(|d| d.border_1().border_color(theme.accent))
                .map(|button| match kind {
                    ButtonKind::Primary => button.hover(|d| d.opacity(0.9)),
                    _ => button.hover(|d| d.bg(theme.element_hover).text_color(theme.text)),
                })
        })
        .when(!enabled, |button| button.opacity(0.5))
        .child(label)
}

fn reset_outcome_text(outcome: AgentResetOutcome) -> &'static str {
    match outcome {
        AgentResetOutcome::Reset => "Reset applied. The windows have cleared.",
        AgentResetOutcome::NothingToReset => "Nothing to reset right now, so no credit was used.",
        AgentResetOutcome::NoCredit => "No reset credit left on this account.",
        AgentResetOutcome::AlreadyRedeemed => "That credit was already redeemed.",
    }
}

/// Brand mark for the harness (and, on model rows, the model) a row belongs
/// to, centred in a square box.
fn provider_mark(
    harness: Option<HarnessId>,
    model: Option<&str>,
    size: f32,
    theme: &Theme,
) -> gpui::Div {
    let mark = div()
        .flex_none()
        .size(px(size))
        .flex()
        .items_center()
        .justify_center();
    match harness {
        Some(harness) => {
            let (path, tint) = crate::pickers::model_brand_icon(harness, model);
            mark.child(
                icon(path)
                    .size(px(size))
                    .text_color(tint.unwrap_or(theme.text)),
            )
        }
        None => mark,
    }
}

pub(crate) fn provider_label(harness: Option<HarnessId>) -> &'static str {
    match harness {
        Some(HarnessId::Codex) => "Codex",
        Some(HarnessId::ClaudeCode) => "Claude Code",
        Some(HarnessId::Opencode) => "OpenCode",
        Some(HarnessId::Cursor) => "Cursor",
        Some(HarnessId::Grok) => "Grok",
        Some(HarnessId::Antigravity) => "Antigravity",
        Some(HarnessId::Devin) => "Devin",
        Some(HarnessId::Hermes) => "Hermes",
        Some(HarnessId::Pi) => "Pi",
        Some(HarnessId::Dsh) => "DeepSeek Harness",
        Some(HarnessId::Mock) => "Mock",
        None => "Unknown provider",
    }
}

/// Series colours shared by the chart, summary, breakdown bars, and limits.
fn provider_color(harness: Option<HarnessId>, theme: &Theme) -> Hsla {
    match harness {
        Some(HarnessId::Codex) => theme.text,
        Some(HarnessId::ClaudeCode) => icons::claude_brand(),
        Some(HarnessId::Grok) => mix(theme.bg, theme.text, 0.72),
        Some(HarnessId::Cursor) => Hsla::from(gpui::rgb(0x8b8b8b)),
        Some(HarnessId::Opencode) => Hsla::from(gpui::rgb(0x5b9bbd)),
        Some(HarnessId::Antigravity) => Hsla::from(gpui::rgb(0x8c7bd1)),
        Some(HarnessId::Pi) => Hsla::from(gpui::rgb(0x4d9f8a)),
        Some(HarnessId::Dsh) => icons::deepseek_brand(),
        _ => theme.text_muted,
    }
}

fn account_name(account: &AgentAccount) -> String {
    account
        .display_name
        .as_deref()
        .or(account.email.as_deref())
        .unwrap_or("Saved account")
        .to_owned()
}

fn remaining_fraction(window: &AgentUsageWindow) -> f32 {
    1.0 - window.used_fraction.clamp(0.0, 1.0)
}

/// Window length implied by its label; providers report only the reset time.
fn window_span(label: &str) -> Option<chrono::Duration> {
    match label {
        "5h" | "Session" => Some(chrono::Duration::hours(5)),
        "7d" | "Week" | "Weekly" => Some(chrono::Duration::days(7)),
        _ => None,
    }
}

/// `3d 4h`, `2h 15m`, or `12m` until `at`.
pub(crate) fn resets_in(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let minutes = (at - now).num_minutes().max(0);
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

/// Rounds the axis maximum up to a readable 1/2/5 x 10^n step at or above the
/// peak, so the tallest period is never drawn past the top of the plot.
fn nice_scale(peak: f64, count: u32) -> (f64, Vec<f64>) {
    if peak.is_nan() || peak <= 0.0 {
        return (0.0, vec![0.0]);
    }
    let raw = peak / f64::from(count);
    let magnitude = 10f64.powf(raw.log10().floor());
    let normalized = raw / magnitude;
    let step = magnitude
        * if normalized > 5.0 {
            10.0
        } else if normalized > 2.0 {
            5.0
        } else if normalized > 1.0 {
            2.0
        } else {
            1.0
        };
    let max = (peak / step).ceil() * step;
    let ticks = (0..)
        .map(|index| f64::from(index) * step)
        .take_while(|tick| *tick <= max + step * 1e-6)
        .collect();
    (max, ticks)
}

/// Shape-preserving cubic tangents that cannot overshoot spiky usage data.
fn monotone_tangents(points: &[(f32, f32)]) -> Vec<f32> {
    let count = points.len();
    if count < 2 {
        return vec![0.0; count];
    }
    let slopes: Vec<f32> = points
        .windows(2)
        .map(|pair| {
            let dx = pair[1].0 - pair[0].0;
            if dx == 0.0 {
                0.0
            } else {
                (pair[1].1 - pair[0].1) / dx
            }
        })
        .collect();
    let mut tangents = vec![0.0; count];
    tangents[0] = slopes[0];
    tangents[count - 1] = slopes[count - 2];
    for index in 1..count - 1 {
        let (previous, next) = (slopes[index - 1], slopes[index]);
        tangents[index] = if previous * next <= 0.0 {
            0.0
        } else {
            (previous + next) / 2.0
        };
    }
    for index in 0..count - 1 {
        let slope = slopes[index];
        if slope == 0.0 {
            tangents[index] = 0.0;
            tangents[index + 1] = 0.0;
            continue;
        }
        let a = tangents[index] / slope;
        let b = tangents[index + 1] / slope;
        let magnitude = a * a + b * b;
        if magnitude > 9.0 {
            let scale = 3.0 / magnitude.sqrt();
            tangents[index] = scale * a * slope;
            tangents[index + 1] = scale * b * slope;
        }
    }
    tangents
}

/// Appends the smoothed curve through `points`; the path must already sit at
/// the first point.
fn trace(path: &mut PathBuilder, points: &[(f32, f32)], tangents: &[f32]) {
    for index in 0..points.len().saturating_sub(1) {
        let (x0, y0) = points[index];
        let (x1, y1) = points[index + 1];
        let dx = x1 - x0;
        path.cubic_bezier_to(
            point(px(x1), px(y1)),
            point(px(x0 + dx / 3.0), px(y0 + tangents[index] * dx / 3.0)),
            point(px(x1 - dx / 3.0), px(y1 - tangents[index + 1] * dx / 3.0)),
        );
    }
}

fn value(totals: &UsageHistoryTotals, metric: Metric) -> f64 {
    if metric == Metric::Cost {
        totals.cost_usd()
    } else {
        totals.usage.total().unwrap_or(0) as f64
    }
}

fn format_value(value: f64, metric: Metric) -> String {
    if metric == Metric::Cost {
        money(value)
    } else {
        compact(value)
    }
}

/// Axis ticks drop the cents when the step is whole.
fn axis_value(value: f64, metric: Metric) -> String {
    if metric == Metric::Cost && value.fract() == 0.0 {
        format!("${value:.0}")
    } else {
        format_value(value, metric)
    }
}

pub(crate) fn money(value: f64) -> String {
    let cents = (value * 100.0).round() as i64;
    let sign = if cents < 0 { "-" } else { "" };
    let cents = cents.abs();
    let whole = (cents / 100).to_string();
    let mut grouped = String::with_capacity(whole.len() + whole.len() / 3);
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("{sign}${grouped}.{:02}", cents % 100)
}

pub(crate) fn compact(value: f64) -> String {
    if value >= 1e9 {
        format!("{:.2}B", value / 1e9)
    } else if value >= 1e6 {
        format!("{:.2}M", value / 1e6)
    } else if value >= 1e3 {
        format!("{:.1}K", value / 1e3)
    } else {
        format!("{value:.0}")
    }
}

fn count(value: Option<u64>) -> String {
    value
        .map(|value| compact(value as f64))
        .unwrap_or_else(|| "Not reported".into())
}

fn percent(share: f64) -> String {
    let value = share * 100.0;
    if value > 0.0 && value < 0.1 {
        "<0.1%".into()
    } else if (value * 10.0).round() % 10.0 == 0.0 {
        format!("{value:.0}%")
    } else {
        format!("{value:.1}%")
    }
}

fn sessions(count: u64) -> String {
    format!(
        "{count} {}",
        if count == 1 { "session" } else { "sessions" }
    )
}

fn date_label(millis: i64, hourly: bool) -> String {
    Local
        .timestamp_millis_opt(millis)
        .single()
        .map(|time| {
            time.format(if hourly { "%b %-d %-I%p" } else { "%b %-d" })
                .to_string()
        })
        .unwrap_or_default()
}

fn heading(label: &str, theme: &Theme) -> gpui::Div {
    div()
        .text_size(px(13.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text)
        .child(SharedString::from(label.to_owned()))
}

fn metric(label: &str, value: String, size: f32, theme: &Theme) -> gpui::Div {
    div()
        .min_w(px(120.0))
        .flex_1()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .text_size(px(12.0))
                .text_color(theme.text_muted)
                .child(SharedString::from(label.to_owned())),
        )
        .child(
            div()
                .text_size(px(size))
                .font_weight(if size > 20.0 {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::MEDIUM
                })
                .child(value),
        )
}
