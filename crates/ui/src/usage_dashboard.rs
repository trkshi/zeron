//! Usage history is loaded only while this page is visible. No transcript copies.
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use chrono::{Local, TimeZone, Utc};
use gpui::{
    AnyElement, Bounds, Context, Entity, FocusHandle, Hsla, IntoElement, PathBuilder, Pixels,
    Render, ScrollHandle, SharedString, Subscription, Task, Window, canvas, div, point, prelude::*,
    px, relative,
};
use zeron_proto::{
    AgentAccount, AgentAccountsSnapshot, AgentAuthKind, HarnessId, UsageHistorySummary,
    UsageHistoryTotals,
};
use zeron_rpc::methods;

use crate::icons::{self, icon};
use crate::settings::accounts::{AccountsSnapshotCache, format_reset};
use crate::settings::widgets::text_tooltip;
use crate::state::AppState;
use crate::theme::Theme;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Metric {
    Cost,
    Tokens,
    Limits,
}

#[derive(Clone, PartialEq, Eq)]
struct ModelKey(Option<HarnessId>, Option<String>);

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
    summary: Option<UsageHistorySummary>,
    loading: bool,
    error: Option<String>,
    account_error: Option<String>,
    selected_model: Option<ModelKey>,
    selected_account: Option<AgentAccount>,
    hover_period: Rc<Cell<Option<usize>>>,
    detail_hover_period: Rc<Cell<Option<usize>>>,
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
            summary: None,
            loading: false,
            error: None,
            account_error: None,
            selected_model: None,
            selected_account: None,
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

    pub fn dismiss_detail(&mut self, cx: &mut Context<Self>) -> bool {
        let open = self.selected_model.take().is_some() | self.selected_account.take().is_some();
        if open {
            self.detail_hover_period.set(None);
            cx.notify();
        }
        open
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

    fn control(
        &self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        selected: bool,
        theme: &Theme,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id.into())
            .role(gpui::Role::Button)
            .tab_index(0)
            .px(px(10.0))
            .h(px(28.0))
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
            .when(selected, |d| d.bg(theme.glass_hover()))
            .hover(|d| d.bg(theme.glass_hover()))
            .focus_visible(|d| d.border_1().border_color(theme.accent))
            .cursor_pointer()
            .child(label.into())
    }

    fn header(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let devices: Vec<_> = self
            .state
            .read(cx)
            .devices
            .iter()
            .map(|device| (device.id.clone(), device.name.clone()))
            .collect();
        div()
            .flex_none()
            .px(px(24.0))
            .py(px(12.0))
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    .text_size(px(16.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Usage"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .children(devices.into_iter().map(|(id, name)| {
                        let selected =
                            self.state.read(cx).effective_device_id().as_ref() == Some(&id);
                        self.control(format!("usage-device-{id}"), name, selected, theme)
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.state.update(cx, |state, cx| {
                                    state.select_device(id.clone(), cx);
                                });
                            }))
                    })),
            )
            .child(div().flex_1())
            .child(
                div().flex().gap(px(2.0)).children(
                    [
                        (Metric::Cost, "Cost"),
                        (Metric::Tokens, "Tokens"),
                        (Metric::Limits, "Limits"),
                    ]
                    .into_iter()
                    .map(|(metric, label)| {
                        self.control(
                            format!("usage-metric-{label}"),
                            label,
                            self.metric == metric,
                            theme,
                        )
                        .on_click(cx.listener(move |page, _, _, cx| page.switch_metric(metric, cx)))
                    }),
                ),
            )
            .when(self.metric != Metric::Limits, |header| {
                header.child(
                    div().flex().gap(px(2.0)).children(
                        [
                            (1, "Past 24h"),
                            (7, "7 days"),
                            (30, "30 days"),
                            (90, "90 days"),
                        ]
                        .into_iter()
                        .map(|(days, label)| {
                            self.control(
                                format!("usage-days-{days}"),
                                label,
                                self.days == days,
                                theme,
                            )
                            .on_click(cx.listener(
                                move |page, _, _, cx| {
                                    if page.days != days {
                                        page.days = days;
                                        page.load_task = None;
                                        page.loading = false;
                                        page.summary = None;
                                        page.selected_model = None;
                                        page.hover_period.set(None);
                                        page.load(false, cx);
                                    }
                                },
                            ))
                        }),
                    ),
                )
            })
            .child(
                self.control("usage-refresh", "", false, theme)
                    .aria_label("Refresh usage")
                    .tooltip(text_tooltip("Refresh usage and model pricing"))
                    .child(icon(icons::REFRESH).size(px(15.0)))
                    .on_click(cx.listener(|page, _, _, cx| page.load(true, cx))),
            )
            .into_any_element()
    }

    fn history(
        &self,
        summary: &UsageHistorySummary,
        width: f32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let totals = &summary.totals;
        let mut providers = summary.providers.clone();
        providers.sort_by(|a, b| {
            value(&b.totals, self.metric).total_cmp(&value(&a.totals, self.metric))
        });
        let total_value = value(totals, self.metric);
        let summary_column = div()
            .flex_none()
            .w(if width >= 760.0 {
                px(256.0)
            } else {
                px(width.max(180.0))
            })
            .flex()
            .flex_col()
            .gap(px(18.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(px(32.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(
                                if self.metric == Metric::Cost
                                    && totals.unpriced_turns == totals.turns
                                {
                                    "Unpriced".into()
                                } else {
                                    format_value(total_value, self.metric)
                                },
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(format!(
                                "{} sessions / {} turns{}",
                                totals.sessions,
                                totals.turns,
                                if self.metric == Metric::Cost {
                                    " / API value"
                                } else {
                                    ""
                                }
                            )),
                    ),
            )
            .children(providers.iter().map(|provider| {
                let amount = value(&provider.totals, self.metric);
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .size(px(7.0))
                                    .rounded_full()
                                    .bg(provider_color(provider.harness, theme)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(13.0))
                                    .child(provider_label(provider.harness)),
                            )
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .child(format_value(amount, self.metric)),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(format!(
                                "{:.1}% / {} sessions",
                                if total_value > 0.0 {
                                    amount / total_value * 100.0
                                } else {
                                    0.0
                                },
                                provider.totals.sessions
                            )),
                    )
            }));
        let stats = if self.metric == Metric::Cost {
            vec![
                metric("Reported cost", money(totals.reported_cost_usd), theme),
                metric(
                    "Estimated API value",
                    money(totals.estimated_cost_usd),
                    theme,
                ),
                metric("Unpriced turns", totals.unpriced_turns.to_string(), theme),
                metric("Processed tokens", count(totals.usage.total()), theme),
                metric("API cache savings", money(totals.cache_savings_usd), theme),
            ]
        } else {
            vec![
                metric(
                    "Input (incl. cache)",
                    count(totals.usage.input_tokens),
                    theme,
                ),
                metric(
                    "Cached input",
                    count(totals.usage.cached_input_tokens),
                    theme,
                ),
                metric(
                    "Cache writes",
                    count(totals.usage.cache_write_input_tokens),
                    theme,
                ),
                metric("Output", count(totals.usage.output_tokens), theme),
                metric(
                    "Reasoning (in output)",
                    count(totals.usage.reasoning_output_tokens),
                    theme,
                ),
            ]
        };
        div().flex().flex_col().gap(px(28.0))
            .child(div().flex().when(width < 760.0, |d| d.flex_col()).gap(px(28.0))
                .child(summary_column).child(div().flex_1().min_w_0().child(
                    self.chart(summary, None, theme, cx))))
            .child(div().flex().flex_col().gap(px(12.0)).child(heading("Totals", theme))
                .child(div().flex().flex_wrap().gap(px(24.0)).children(stats)))
            .child(self.type_bar(totals, theme))
            .child(self.breakdown(summary, theme, cx))
            .child(div().text_size(px(11.0)).text_color(theme.text_muted)
                .child("Completed Zeron turns only. Older saved snapshots can have unknown models or incomplete history. Model groups describe the turn model, not individual subagent requests."))
            .when(self.metric == Metric::Cost, |d| d.child(div().text_size(px(11.0)).text_color(theme.text_muted)
                .child("API estimates use standard model rates, not your subscription bill. Unknown models and incomplete billing counts stay unpriced; cache savings are an API-rate equivalent.")))
            .when(totals.incomplete_turns > 0 || totals.incomplete_cache_turns > 0 || summary.skipped_sources > 0, |d| d.child(
                div().text_size(px(11.0)).text_color(theme.warning).child(format!(
                    "Partial totals: {} turns have incomplete input/output counts; {} have incomplete cache counts; {} saved sources unavailable.",
                    totals.incomplete_turns, totals.incomplete_cache_turns, summary.skipped_sources))))
            .when(summary.pricing_error.is_some(), |d| d.child(div().text_size(px(11.0)).text_color(theme.warning)
                .child("Model pricing could not refresh. Previously saved prices are retained.")))
            .into_any_element()
    }

    fn type_bar(&self, totals: &UsageHistoryTotals, theme: &Theme) -> AnyElement {
        let (label, values): (&str, Vec<(&str, Option<f64>)>) = if self.metric == Metric::Cost {
            (
                "Cost by type (API-rate allocation)",
                ["Input", "Cache read", "Cache write", "Output", "Other"]
                    .into_iter()
                    .zip(totals.category_cost_usd.map(Some))
                    .collect(),
            )
        } else {
            let usage = totals.usage;
            (
                "Tokens by type",
                vec![
                    (
                        "Uncached input",
                        totals.uncached_input_tokens.map(|n| n as f64),
                    ),
                    ("Cache read", usage.cached_input_tokens.map(|n| n as f64)),
                    (
                        "Cache write",
                        usage.cache_write_input_tokens.map(|n| n as f64),
                    ),
                    ("Output", usage.output_tokens.map(|n| n as f64)),
                ],
            )
        };
        let total: f64 = values.iter().filter_map(|(_, v)| *v).sum();
        let bar = div()
            .w_full()
            .h(px(8.0))
            .flex()
            .gap(px(2.0))
            .overflow_hidden()
            .rounded(px(3.0))
            .bg(theme.glass_hover())
            .children(values.iter().enumerate().filter_map(|(index, (_, value))| {
                let fraction = value.unwrap_or(0.0) / total;
                (fraction.is_finite() && fraction > 0.0).then(|| {
                    div()
                        .h_full()
                        .w(relative(fraction as f32))
                        .bg(theme.text.opacity(0.25 + index as f32 * 0.16))
                })
            }));
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(heading(label, theme))
            .child(bar)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(14.0))
                    .children(values.iter().enumerate().map(|(index, (label, value))| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .text_size(px(11.0))
                            .child(
                                div()
                                    .size(px(7.0))
                                    .rounded(px(2.0))
                                    .bg(theme.text.opacity(0.25 + index as f32 * 0.16)),
                            )
                            .child(div().text_color(theme.text_muted).child(*label))
                            .child(
                                value
                                    .map(|value| format_value(value, self.metric))
                                    .unwrap_or_else(|| "Not reported".into()),
                            )
                    })),
            )
            .into_any_element()
    }

    fn breakdown(
        &self,
        summary: &UsageHistorySummary,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut models = summary.models.clone();
        models.sort_by(|a, b| {
            value(&b.totals, self.metric).total_cmp(&value(&a.totals, self.metric))
        });
        let total = value(&summary.totals, self.metric);
        let mut rows = div().flex().flex_col().w_full().child(table_row(
            theme,
            "Model / Provider",
            "Cost",
            "Share",
            "Tokens",
            true,
        ));
        if self.by_time {
            for bucket in summary.buckets.iter().rev() {
                let cost: f64 = bucket.groups.iter().map(|g| g.totals.cost_usd()).sum();
                let tokens: u64 = bucket
                    .groups
                    .iter()
                    .map(|g| g.totals.usage.total().unwrap_or(0))
                    .sum();
                rows = rows.child(table_row(
                    theme,
                    &date_label(bucket.started_at, summary.hourly),
                    &money(cost),
                    "",
                    &compact(tokens as f64),
                    false,
                ));
            }
        } else {
            for (index, group) in models.into_iter().enumerate() {
                let amount = value(&group.totals, self.metric);
                let share = if total > 0.0 { amount / total } else { 0.0 };
                let label = format!(
                    "{} / {}",
                    group.model.as_deref().unwrap_or("Unknown model"),
                    provider_label(group.harness)
                );
                let key = ModelKey(group.harness, group.model.clone());
                rows = rows.child(
                    div()
                        .id(format!("usage-model-{index}"))
                        .role(gpui::Role::Button)
                        .aria_label(SharedString::from(label.clone()))
                        .tab_index(0)
                        .cursor_pointer()
                        .hover(|d| d.bg(theme.glass_hover()))
                        .focus_visible(|d| d.border_1().border_color(theme.accent))
                        .child(table_row(
                            theme,
                            &label,
                            &if group.totals.unpriced_turns == group.totals.turns {
                                "Unpriced".into()
                            } else {
                                money(group.totals.cost_usd())
                            },
                            &format!("{:.1}%", share * 100.0),
                            &count(group.totals.usage.total()),
                            false,
                        ))
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.selected_model = Some(key.clone());
                            page.detail_hover_period.set(None);
                            cx.notify();
                        })),
                );
            }
        }
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(heading("Breakdown", theme))
                    .child(
                        div().flex().children(
                            [
                                (false, "Model"),
                                (true, if summary.hourly { "Hour" } else { "Day" }),
                            ]
                            .into_iter()
                            .map(|(by_time, label)| {
                                self.control(
                                    format!("usage-breakdown-{label}"),
                                    label,
                                    self.by_time == by_time,
                                    theme,
                                )
                                .on_click(cx.listener(
                                    move |page, _, _, cx| {
                                        page.by_time = by_time;
                                        cx.notify();
                                    },
                                ))
                            }),
                        ),
                    ),
            )
            .child(rows)
            .into_any_element()
    }

    fn chart(
        &self,
        summary: &UsageHistorySummary,
        model: Option<&ModelKey>,
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
                    .or_insert_with(|| vec![0.0; length])[index] +=
                    value(&group.totals, self.metric);
            }
        }
        let maximum = series
            .values()
            .flatten()
            .copied()
            .fold(0.0_f64, f64::max)
            .max(1.0)
            * 1.1;
        let painted: Vec<_> = series
            .into_iter()
            .map(|(harness, points)| (provider_color(harness, theme), points))
            .collect();
        let bounds: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::new(Cell::new(None));
        let measure = bounds.clone();
        let border = theme.border;
        let detail = model.is_some();
        let hover_state = if detail {
            self.detail_hover_period.clone()
        } else {
            self.hover_period.clone()
        };
        let hover = hover_state.clone();
        let chart = canvas(
            move |bounds, _, _| {
                measure.set(Some(bounds));
            },
            move |bounds, _, window, _| {
                let w = f32::from(bounds.size.width);
                let h = f32::from(bounds.size.height);
                for tick in 0..=4 {
                    let y = bounds.origin.y + px(h * tick as f32 / 4.0);
                    let mut path = PathBuilder::stroke(px(1.0));
                    path.move_to(point(bounds.origin.x, y));
                    path.line_to(point(bounds.right(), y));
                    if let Ok(path) = path.build() {
                        window.paint_path(path, border);
                    }
                }
                for (color, values) in &painted {
                    let mut line = PathBuilder::stroke(px(2.0));
                    let mut area = PathBuilder::fill();
                    area.move_to(point(bounds.origin.x, bounds.bottom()));
                    for (index, value) in values.iter().enumerate() {
                        let position = point(
                            bounds.origin.x + px(w * index as f32 / (length - 1) as f32),
                            bounds.bottom() - px(h * (*value / maximum) as f32),
                        );
                        if index == 0 {
                            line.move_to(position);
                        } else {
                            line.line_to(position);
                        }
                        area.line_to(position);
                    }
                    area.line_to(point(bounds.right(), bounds.bottom()));
                    area.close();
                    if let Ok(path) = area.build() {
                        window.paint_path(path, color.opacity(0.10));
                    }
                    if let Ok(path) = line.build() {
                        window.paint_path(path, *color);
                    }
                }
                if let Some(index) = hover.get().filter(|index| *index < length) {
                    let x = bounds.origin.x + px(w * index as f32 / (length - 1) as f32);
                    let mut path = PathBuilder::stroke(px(1.0));
                    path.move_to(point(x, bounds.origin.y));
                    path.line_to(point(x, bounds.bottom()));
                    if let Ok(path) = path.build() {
                        window.paint_path(path, border);
                    }
                }
            },
        )
        .w_full()
        .h(px(190.0));
        let period = hover_state
            .get()
            .filter(|index| *index < length)
            .map(|index| start + index as i64 * interval);
        let hovered_value = period.map(|period| {
            summary
                .buckets
                .iter()
                .find(|bucket| bucket.started_at == period)
                .map_or(0.0, |bucket| {
                    bucket
                        .groups
                        .iter()
                        .filter(|group| {
                            model.is_none_or(|key| key.0 == group.harness && key.1 == group.model)
                        })
                        .map(|group| value(&group.totals, self.metric))
                        .sum()
                })
        });
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .child(heading(
                        if summary.hourly {
                            if self.metric == Metric::Cost {
                                "Hourly API value"
                            } else {
                                "Hourly processed tokens"
                            }
                        } else if self.metric == Metric::Cost {
                            "Daily API value"
                        } else {
                            "Daily processed tokens"
                        },
                        theme,
                    ))
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(format_value(maximum, self.metric)),
                    ),
            )
            .child(
                div()
                    .id(if detail {
                        "usage-detail-trend"
                    } else {
                        "usage-trend"
                    })
                    .child(chart)
                    .on_mouse_move(
                        cx.listener(move |page, event: &gpui::MouseMoveEvent, _, cx| {
                            if let Some(bounds) = bounds.get() {
                                let fraction = f32::from(event.position.x - bounds.origin.x)
                                    / f32::from(bounds.size.width).max(1.0);
                                let next =
                                    Some((fraction.clamp(0.0, 1.0) * (length - 1) as f32).round()
                                        as usize);
                                let hover = if detail {
                                    &page.detail_hover_period
                                } else {
                                    &page.hover_period
                                };
                                if hover.replace(next) != next {
                                    cx.notify();
                                }
                            }
                        }),
                    )
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
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .text_size(px(10.0))
                    .text_color(theme.text_muted)
                    .child(date_label(start, summary.hourly))
                    .child(date_label(summary.until, summary.hourly)),
            )
            .child(
                div()
                    .h(px(16.0))
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child(
                        period
                            .zip(hovered_value)
                            .map(|(period, value)| {
                                format!(
                                    "{} / {}",
                                    date_label(period, summary.hourly),
                                    format_value(value, self.metric)
                                )
                            })
                            .unwrap_or_default(),
                    ),
            )
            .into_any_element()
    }

    fn limits(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let snapshot = cx
            .try_global::<AccountsSnapshotCache>()
            .and_then(|cache| cache.0.get(&self.target));
        let mut content = div().flex().flex_col().gap(px(28.0));
        let Some(snapshot) = snapshot else {
            return content
                .child("No account limits available. Refresh to check this device.")
                .into_any_element();
        };
        for harness in [HarnessId::Codex, HarnessId::ClaudeCode] {
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
            let mut section = div()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(heading(provider_label(Some(harness)), theme));
            for (label, labels) in [
                ("Session", &["5h", "Session"][..]),
                ("Weekly", &["7d", "Week", "Weekly"][..]),
            ] {
                let now = Utc::now();
                let mut windows: Vec<_> = accounts
                    .iter()
                    .filter_map(|account| {
                        let window = account
                            .usage_windows
                            .iter()
                            .find(|window| labels.contains(&window.label.as_str()))?;
                        if !window.used_fraction.is_finite() || window.used_fraction < 0.0 {
                            return None;
                        }
                        Some((*account, window))
                    })
                    .collect();
                windows.sort_by_key(|(_, window)| window.resets_at);
                if windows.is_empty() {
                    continue;
                }
                let stale = windows.iter().any(|(account, window)| {
                    !self.online
                        || account.usage_error.is_some()
                        || account
                            .usage_fetched_at
                            .is_none_or(|at| now.timestamp_millis().saturating_sub(at) > 300_000)
                        || window.resets_at.is_some_and(|at| at <= now)
                });
                let remaining: f32 = windows
                    .iter()
                    .map(|(_, window)| 1.0 - window.used_fraction.clamp(0.0, 1.0))
                    .sum::<f32>()
                    / windows.len() as f32;
                section = section.child(
                    div()
                        .p(px(16.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(theme.border)
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(20.0))
                        .child(
                            div()
                                .w(px(140.0))
                                .flex_none()
                                .flex()
                                .flex_col()
                                .gap(px(4.0))
                                .child(label)
                                .child(
                                    div()
                                        .text_size(px(28.0))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .child(format!("{:.0}% left", remaining * 100.0)),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(theme.text_muted)
                                        .child(if stale {
                                            "Stale snapshot"
                                        } else {
                                            "Account average"
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(200.0))
                                .flex()
                                .flex_wrap()
                                .gap(px(6.0))
                                .children(windows.into_iter().map(|(account, window)| {
                                    let left = 1.0 - window.used_fraction.clamp(0.0, 1.0);
                                    let name = account
                                        .display_name
                                        .as_deref()
                                        .or(account.email.as_deref())
                                        .unwrap_or("Saved account");
                                    let resets = format_reset(window.resets_at, now)
                                        .unwrap_or_else(|| "Reset unavailable".into());
                                    let detail = account.clone();
                                    div()
                                        .id(format!("usage-limit-{}-{label}", account.id))
                                        .role(gpui::Role::Button)
                                        .tab_index(0)
                                        .flex_1()
                                        .min_w(px(160.0))
                                        .h(px(52.0))
                                        .relative()
                                        .rounded(px(6.0))
                                        .overflow_hidden()
                                        .bg(theme.glass_hover())
                                        .cursor_pointer()
                                        .tooltip(text_tooltip(format!("{name}: {resets}")))
                                        .child(
                                            div()
                                                .absolute()
                                                .left_0()
                                                .top_0()
                                                .h_full()
                                                .w(relative(left))
                                                .bg(provider_color(Some(harness), theme)
                                                    .opacity(0.25)),
                                        )
                                        .child(
                                            div()
                                                .relative()
                                                .px(px(10.0))
                                                .py(px(7.0))
                                                .flex()
                                                .flex_col()
                                                .gap(px(5.0))
                                                .child(
                                                    div()
                                                        .flex()
                                                        .items_center()
                                                        .justify_between()
                                                        .gap(px(8.0))
                                                        .text_size(px(12.0))
                                                        .child(
                                                            div()
                                                                .flex_1()
                                                                .min_w_0()
                                                                .truncate()
                                                                .child(SharedString::from(
                                                                    name.to_owned(),
                                                                )),
                                                        )
                                                        .child(format!("{:.0}%", left * 100.0)),
                                                )
                                                .child(
                                                    div()
                                                        .text_size(px(10.0))
                                                        .text_color(theme.text_muted)
                                                        .child(resets),
                                                ),
                                        )
                                        .on_click(cx.listener(move |page, _, _, cx| {
                                            page.selected_account = Some(detail.clone());
                                            cx.notify();
                                        }))
                                })),
                        ),
                );
            }
            for account in accounts
                .iter()
                .filter(|account| account.usage_windows.is_empty() || account.usage_error.is_some())
            {
                section = section.child(div().text_size(px(11.0)).text_color(theme.warning).child(
                    format!(
                            "{}: {}",
                            account
                                .display_name
                                .as_deref()
                                .or(account.email.as_deref())
                                .unwrap_or("Saved account"),
                            account
                                .usage_error
                                .as_deref()
                                .unwrap_or("Limits not reported")
                        ),
                ));
            }
            content = content.child(section);
        }
        content.child(div().text_size(px(11.0)).text_color(theme.text_muted)
            .child("Pool percentages are unweighted account averages, not combined token capacity. Plans can have different allowances. Limits are separate from token history."))
            .into_any_element()
    }

    fn detail(
        &self,
        summary: Option<&UsageHistorySummary>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let mut card = div()
            .id("usage-detail-card")
            .w_full()
            .max_w(px(760.0))
            .max_h(relative(0.9))
            .overflow_y_scroll()
            .p(px(24.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .flex()
            .flex_col()
            .gap(px(20.0));
        if let Some(account) = &self.selected_account {
            card = card
                .child(heading(
                    account
                        .display_name
                        .as_deref()
                        .or(account.email.as_deref())
                        .unwrap_or("Saved account"),
                    theme,
                ))
                .child(format!(
                    "{} / {}{}",
                    provider_label(Some(account.harness)),
                    account.plan_label.as_deref().unwrap_or("Unknown plan"),
                    if account.active { " / Active" } else { "" }
                ));
            for window in &account.usage_windows {
                card = card.child(format!(
                    "{}: {:.1}% used / {}",
                    window.label,
                    window.used_fraction * 100.0,
                    format_reset(window.resets_at, Utc::now())
                        .unwrap_or_else(|| "Reset unavailable".into())
                ));
            }
            if let Some(error) = &account.usage_error {
                card = card.child(SharedString::from(error.clone()));
            }
        } else if let (Some(key), Some(summary)) = (&self.selected_model, summary) {
            let group = summary
                .models
                .iter()
                .find(|group| key.0 == group.harness && key.1 == group.model)?;
            let totals = &group.totals;
            card = card
                .child(heading(
                    group.model.as_deref().unwrap_or("Unknown model"),
                    theme,
                ))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(provider_label(group.harness)),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(28.0))
                        .child(metric(
                            "Reported cost",
                            money(totals.reported_cost_usd),
                            theme,
                        ))
                        .child(metric(
                            "Estimated API value",
                            money(totals.estimated_cost_usd),
                            theme,
                        ))
                        .child(metric("Tokens", count(totals.usage.total()), theme))
                        .child(metric(
                            "Cache hit",
                            totals
                                .usage
                                .cache_hit_rate()
                                .filter(|_| totals.incomplete_cache_turns == 0)
                                .map(|rate| format!("{:.1}%", rate * 100.0))
                                .unwrap_or_else(|| "Not reported".into()),
                            theme,
                        )),
                )
                .child(self.chart(summary, Some(key), theme, cx))
                .child(self.type_bar(totals, theme));
        } else {
            return None;
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .p(px(20.0))
                .bg(gpui::black().opacity(0.55))
                .child(
                    card.child(
                        self.control("usage-detail-close", "Close", false, theme)
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.dismiss_detail(cx);
                            })),
                    ),
                )
                .into_any_element(),
        )
    }
}

impl Render for UsageDashboard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let width = (f32::from(window.viewport_size().width) - 340.0).clamp(240.0, 1050.0);
        let mut content = div()
            .w_full()
            .max_w(px(1100.0))
            .mx_auto()
            .px(px(24.0))
            .py(px(28.0))
            .flex()
            .flex_col()
            .gap(px(24.0));
        if !self.online {
            content = content.child(div().text_color(theme.warning).child(format!(
                "{} is offline. Saved figures may be stale.",
                self.device_name
            )));
        }
        let error = if self.metric == Metric::Limits {
            self.account_error.as_ref()
        } else {
            self.error.as_ref()
        };
        if let Some(error) = error {
            content = content
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.warning)
                        .child(SharedString::from(error.clone())),
                )
                .child(
                    self.control("usage-retry", "Retry", false, &theme)
                        .on_click(cx.listener(|page, _, _, cx| page.load(true, cx))),
                );
        }
        if self.metric == Metric::Limits {
            content = content.child(self.limits(&theme, cx));
        } else if let Some(summary) = self.summary.as_ref() {
            if summary.totals.turns == 0 {
                content = content.child(div().py(px(48.0)).text_color(theme.text_muted)
                    .child("No recorded usage in this period. Completed turns with provider-reported token counts appear here."));
            } else {
                content = content.child(self.history(summary, width, &theme, cx));
            }
        } else if self.loading {
            content = content.child(
                div()
                    .py(px(48.0))
                    .text_color(theme.text_muted)
                    .child("Loading saved usage..."),
            );
        } else if error.is_none() {
            content =
                content.child("Usage history needs an updated engine on the selected device.");
        }
        div()
            .id("usage-dashboard")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .flex()
            .flex_col()
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
            .when(self.loading && self.summary.is_some(), |d| {
                d.child(
                    div()
                        .px(px(24.0))
                        .pt(px(6.0))
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child("Refreshing..."),
                )
            })
            .child(
                div()
                    .id("usage-page-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(content),
            )
            .children(self.detail(self.summary.as_ref(), &theme, cx))
    }
}

fn provider_label(harness: Option<HarnessId>) -> &'static str {
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
        Some(HarnessId::Mock) => "Mock",
        None => "Unknown provider",
    }
}

fn provider_color(harness: Option<HarnessId>, theme: &Theme) -> Hsla {
    match harness {
        Some(HarnessId::ClaudeCode) => icons::claude_brand(),
        Some(HarnessId::Codex) => theme.text,
        Some(HarnessId::Opencode) => theme.success,
        Some(HarnessId::Cursor) => theme.accent,
        _ => theme.text_muted,
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

fn money(value: f64) -> String {
    format!("${value:.2}")
}

fn compact(value: f64) -> String {
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

fn metric(label: &str, value: String, theme: &Theme) -> gpui::Div {
    div()
        .min_w(px(130.0))
        .flex_1()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(theme.text_muted)
                .child(SharedString::from(label.to_owned())),
        )
        .child(
            div()
                .text_size(px(16.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(value),
        )
}

fn table_row(
    theme: &Theme,
    label: &str,
    cost: &str,
    share: &str,
    tokens: &str,
    header: bool,
) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(12.0))
        .py(px(if header { 8.0 } else { 12.0 }))
        .border_b_1()
        .border_color(theme.border)
        .text_size(px(if header { 11.0 } else { 12.0 }))
        .text_color(if header { theme.text_muted } else { theme.text })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(SharedString::from(label.to_owned())),
        )
        .child(
            div()
                .w(px(84.0))
                .text_right()
                .child(SharedString::from(cost.to_owned())),
        )
        .child(
            div()
                .w(px(60.0))
                .text_right()
                .text_color(theme.text_muted)
                .child(SharedString::from(share.to_owned())),
        )
        .child(
            div()
                .w(px(76.0))
                .text_right()
                .child(SharedString::from(tokens.to_owned())),
        )
}
