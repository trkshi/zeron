//! Home's stats panels: device health, git activity, CI builds, and spend.
//! One entity polls every online device while Home is visible in an active
//! window; Shell places the panels beside Working now and the account pool.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, Local, TimeZone, Utc};
use gpui::{AnyElement, Context, Entity, EventEmitter, SharedString, Task, div, prelude::*, px};
use zeron_proto::{
    CiRepo, CiRun, DeviceHealth, GitActivityRepo, HarnessId, ListeningService, UsageHistorySummary,
};
use zeron_rpc::methods;

use crate::icons::{self, icon};
use crate::popover::Popup;
use crate::settings::accounts::{usage_color, usage_level};
use crate::settings::widgets::text_tooltip;
use crate::state::AppState;
use crate::theme::Theme;

const TICK: Duration = Duration::from_secs(5);
const HEALTH_EVERY: Duration = Duration::from_secs(10);
const CI_EVERY: Duration = Duration::from_secs(60);
const GIT_EVERY: Duration = Duration::from_secs(120);
const SPEND_EVERY: Duration = Duration::from_secs(120);
const SERVICES_EVERY: Duration = Duration::from_secs(15);
/// How often to re-ask a device whose engine predates a panel's RPC, in case
/// it was updated meanwhile.
const UNSUPPORTED_EVERY: Duration = Duration::from_secs(600);
const SERVICE_ROW_HEIGHT: f32 = 32.0;
const MAX_SERVICE_ROWS: usize = 6;
const GIT_DAYS: u32 = 14;
const SPEND_DAYS: u32 = 7;
const HEADER_HEIGHT: f32 = 40.0;
const DEVICE_ROW_HEIGHT: f32 = 58.0;
const RUN_ROW_HEIGHT: f32 = 40.0;
const MAX_RUN_ROWS: usize = 4;
const SPEND_ROW_HEIGHT: f32 = 26.0;
const MAX_SPEND_ROWS: usize = 4;
const GIT_PANEL_HEIGHT: f32 = 176.0;
const CHART_HEIGHT: f32 = 64.0;

/// The panels Customize can hide. Working now always shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum HomePanel {
    Accounts,
    Devices,
    Git,
    Builds,
    Services,
    Spend,
}

impl HomePanel {
    /// Customize order, and the order masonry places panels after Working now.
    pub(crate) const ALL: [Self; 6] = [
        Self::Accounts,
        Self::Git,
        Self::Devices,
        Self::Builds,
        Self::Services,
        Self::Spend,
    ];

    /// Persisted in `UiSettings::home_hidden_panels`.
    fn key(self) -> &'static str {
        match self {
            Self::Accounts => "accounts",
            Self::Devices => "devices",
            Self::Git => "git",
            Self::Builds => "builds",
            Self::Services => "services",
            Self::Spend => "spend",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Accounts => "Account pool",
            Self::Devices => "Devices",
            Self::Git => "Git activity",
            Self::Builds => "Builds",
            Self::Services => "Running services",
            Self::Spend => "Spend",
        }
    }

    /// Settings → Home's row description.
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Accounts => "Codex and Claude quota across your signed-in accounts.",
            Self::Devices => "CPU, memory, and disk of each online device, and engine uptime.",
            Self::Git => "Your commits over the last 14 days, across projects or one at a time.",
            Self::Builds => "Latest GitHub Actions runs of your projects' repositories.",
            Self::Services => "Ports your processes listen on, by project.",
            Self::Spend => "Today's and this week's cost by provider.",
        }
    }
}

/// Show or hide a panel, persisted in `UiSettings::home_hidden_panels`.
pub(crate) fn set_panel_shown(panel: HomePanel, shown: bool, cx: &mut gpui::App) {
    crate::settings::update(crate::settings::SavePolicy::Immediate, cx, |settings| {
        settings.home_hidden_panels.retain(|key| key != panel.key());
        if !shown {
            settings.home_hidden_panels.push(panel.key().to_owned());
        }
    });
}

pub(crate) fn panel_shown(panel: HomePanel, cx: &gpui::App) -> bool {
    !crate::settings::home_hidden_panels(cx)
        .iter()
        .any(|key| key == panel.key())
}

pub(super) enum HomeStatsEvent {
    OpenUsage,
    OpenSettings,
}

/// One RPC reading and its freshness.
struct Reading<T> {
    value: Option<T>,
    error: Option<String>,
    at: Option<Instant>,
    loading: bool,
    /// The device's engine answered "unknown method": it predates this panel,
    /// so the panel leaves the device out rather than showing an error.
    unsupported: bool,
}

impl<T> Default for Reading<T> {
    fn default() -> Self {
        Self {
            value: None,
            error: None,
            at: None,
            loading: false,
            unsupported: false,
        }
    }
}

impl<T> Reading<T> {
    fn due(&self, every: Duration) -> bool {
        let every = if self.unsupported {
            UNSUPPORTED_EVERY
        } else {
            every
        };
        !self.loading && self.at.is_none_or(|at| at.elapsed() >= every)
    }
}

fn unknown_method(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("unknown method") || message.contains("unknownmethod")
}

/// A device the panels read, with the RPC target that addresses it (`None`
/// for the local engine).
#[derive(Clone)]
struct Host {
    id: String,
    name: String,
    platform: String,
    target: Option<String>,
    local: bool,
}

pub(super) struct HomeStats {
    state: Entity<AppState>,
    visible: bool,
    window_active: bool,
    health: HashMap<String, Reading<DeviceHealth>>,
    git: HashMap<String, Reading<Vec<GitActivityRepo>>>,
    ci: HashMap<String, Reading<Vec<CiRepo>>>,
    services: HashMap<String, Reading<Vec<ListeningService>>>,
    /// The all-ports choice the services readings were taken with.
    services_all_ports: bool,
    spend: Reading<UsageHistorySummary>,
    spend_target: Option<Option<String>>,
    /// Representative space id; `None` is every project.
    project: Option<String>,
    project_menu: Popup<()>,
    customize_menu: Popup<()>,
    tasks: Vec<Task<()>>,
    poll_task: Option<Task<()>>,
}

impl EventEmitter<HomeStatsEvent> for HomeStats {}

impl HomeStats {
    pub(super) fn new(state: Entity<AppState>) -> Self {
        Self {
            state,
            visible: false,
            window_active: false,
            health: HashMap::new(),
            git: HashMap::new(),
            ci: HashMap::new(),
            services: HashMap::new(),
            services_all_ports: false,
            spend: Reading::default(),
            spend_target: None,
            project: None,
            project_menu: Popup::default(),
            customize_menu: Popup::default(),
            tasks: Vec::new(),
            poll_task: None,
        }
    }

    pub(super) fn track(&mut self, visible: bool, window_active: bool, cx: &mut Context<Self>) {
        let resumed = visible && window_active && (!self.visible || !self.window_active);
        self.visible = visible;
        self.window_active = window_active;
        if !visible || !window_active {
            self.poll_task = None;
            return;
        }
        if resumed {
            self.poll(cx);
        }
        if self.poll_task.is_none() {
            self.poll_task = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(TICK).await;
                    if this.update(cx, |stats, cx| stats.poll(cx)).is_err() {
                        return;
                    }
                }
            }));
        }
    }

    fn hosts(&self, cx: &gpui::App) -> Vec<Host> {
        let state = self.state.read(cx);
        let now = Utc::now();
        let ready = matches!(state.connection, zeron_proto::view::ConnectionStatus::Ready)
            && state.engine().is_some();
        if !ready {
            return Vec::new();
        }
        let local = state.local_device_id.clone();
        let mut hosts: Vec<Host> = state
            .devices
            .iter()
            .filter(|device| {
                local.as_ref() == Some(&device.id) || state.device_online(&device.id, now)
            })
            .map(|device| {
                let is_local = local.as_ref() == Some(&device.id);
                Host {
                    id: device.id.clone(),
                    name: device.name.clone(),
                    platform: device.platform.clone(),
                    target: (!is_local).then(|| device.id.clone()),
                    local: is_local,
                }
            })
            .collect();
        // This device first, then by name: rows must not jump on refresh.
        hosts.sort_by(|a, b| b.local.cmp(&a.local).then_with(|| a.name.cmp(&b.name)));
        hosts
    }

    /// Git project folders hosted on `device`.
    fn git_paths(&self, device: &str, cx: &gpui::App) -> Vec<String> {
        self.state
            .read(cx)
            .spaces
            .iter()
            .filter(|space| space.device_id == device && space.git_detected)
            .map(|space| space.path.clone())
            .collect()
    }

    /// Every project folder hosted on `device`, git or not.
    fn project_paths(&self, device: &str, cx: &gpui::App) -> Vec<String> {
        self.state
            .read(cx)
            .spaces
            .iter()
            .filter(|space| space.device_id == device)
            .map(|space| space.path.clone())
            .collect()
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        if !self.visible || !self.window_active {
            return;
        }
        self.tasks.retain(|task| !task.is_ready());
        let hosts = self.hosts(cx);
        for host in &hosts {
            if panel_shown(HomePanel::Devices, cx)
                && self
                    .health
                    .entry(host.id.clone())
                    .or_default()
                    .due(HEALTH_EVERY)
            {
                let params = serde_json::json!({ "targetDeviceId": host.target });
                self.fetch(
                    host.id.clone(),
                    methods::GET_DEVICE_HEALTH,
                    params,
                    |stats| &mut stats.health,
                    cx,
                );
            }
            let paths = self.git_paths(&host.id, cx);
            let all_ports = crate::settings::home_services_all_ports(cx);
            if all_ports != self.services_all_ports {
                self.services_all_ports = all_ports;
                self.services.clear();
            }
            if panel_shown(HomePanel::Services, cx)
                && self
                    .services
                    .entry(host.id.clone())
                    .or_default()
                    .due(SERVICES_EVERY)
            {
                let params = serde_json::json!({
                    "targetDeviceId": host.target,
                    "paths": self.project_paths(&host.id, cx),
                    "allPorts": all_ports,
                });
                self.fetch(
                    host.id.clone(),
                    methods::LIST_LISTENING_SERVICES,
                    params,
                    |stats| &mut stats.services,
                    cx,
                );
            }
            if paths.is_empty() {
                continue;
            }
            if panel_shown(HomePanel::Git, cx)
                && self.git.entry(host.id.clone()).or_default().due(GIT_EVERY)
            {
                let params = serde_json::json!({
                    "targetDeviceId": host.target, "paths": paths, "days": GIT_DAYS,
                });
                self.fetch(
                    host.id.clone(),
                    methods::GET_GIT_ACTIVITY,
                    params,
                    |stats| &mut stats.git,
                    cx,
                );
            }
            if panel_shown(HomePanel::Builds, cx)
                && self.ci.entry(host.id.clone()).or_default().due(CI_EVERY)
            {
                let params = serde_json::json!({ "targetDeviceId": host.target, "paths": paths });
                self.fetch(
                    host.id.clone(),
                    methods::LIST_CI_RUNS,
                    params,
                    |stats| &mut stats.ci,
                    cx,
                );
            }
        }
        // Drop devices that went offline so stale rows don't linger.
        let live: HashSet<&str> = hosts.iter().map(|host| host.id.as_str()).collect();
        self.health.retain(|id, _| live.contains(id.as_str()));
        self.git.retain(|id, _| live.contains(id.as_str()));
        self.ci.retain(|id, _| live.contains(id.as_str()));
        self.services.retain(|id, _| live.contains(id.as_str()));
        if panel_shown(HomePanel::Spend, cx) {
            self.poll_spend(cx);
        }
    }

    fn fetch<T: serde::de::DeserializeOwned + 'static>(
        &mut self,
        device: String,
        method: &'static str,
        params: serde_json::Value,
        slot: fn(&mut Self) -> &mut HashMap<String, Reading<T>>,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        slot(self).entry(device.clone()).or_default().loading = true;
        self.tasks.push(cx.spawn(async move |this, cx| {
            let result = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                method,
                params,
                Duration::from_secs(30),
            )
            .await
            .and_then(|value| serde_json::from_value::<T>(value).map_err(|e| e.to_string()));
            this.update(cx, |stats, cx| {
                let reading = slot(stats).entry(device).or_default();
                reading.loading = false;
                reading.at = Some(Instant::now());
                match result {
                    Ok(value) => {
                        reading.value = Some(value);
                        reading.error = None;
                        reading.unsupported = false;
                    }
                    Err(error) if unknown_method(&error) => {
                        reading.value = None;
                        reading.error = None;
                        reading.unsupported = true;
                    }
                    Err(error) => reading.error = Some(error),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn poll_spend(&mut self, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let target = state
            .effective_device_id()
            .filter(|id| Some(id) != state.local_device_id.as_ref());
        if self.spend_target.as_ref() != Some(&target) {
            self.spend = Reading::default();
            self.spend_target = Some(target.clone());
        }
        if !self.spend.due(SPEND_EVERY) {
            return;
        }
        let Some(engine) = state.engine().cloned() else {
            return;
        };
        let offset = Local::now().offset().local_minus_utc() / 60;
        let params = serde_json::json!({ "targetDeviceId": target, "days": SPEND_DAYS,
            "utcOffsetMinutes": offset, "refreshPrices": false });
        self.spend.loading = true;
        self.tasks.push(cx.spawn(async move |this, cx| {
            let result = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                methods::READ_USAGE_HISTORY,
                params,
                Duration::from_secs(125),
            )
            .await
            .and_then(|value| {
                serde_json::from_value::<UsageHistorySummary>(value).map_err(|e| e.to_string())
            });
            this.update(cx, |stats, cx| {
                if stats.spend_target.as_ref() != Some(&target) {
                    return;
                }
                stats.spend.loading = false;
                stats.spend.at = Some(Instant::now());
                match result {
                    Ok(summary) => {
                        stats.spend.value = Some(summary);
                        stats.spend.error = None;
                    }
                    Err(error) => stats.spend.error = Some(error),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    // ---- git ----

    /// Representative space id for each git folder, keyed by device and path.
    fn project_index(&self, cx: &gpui::App) -> HashMap<(String, String), (String, String)> {
        let state = self.state.read(cx);
        state
            .spaces
            .iter()
            .filter(|space| space.git_detected)
            .map(|space| {
                let project = state.representative_space(space);
                (
                    (space.device_id.clone(), space.path.clone()),
                    (project.id.clone(), project.display_name().to_owned()),
                )
            })
            .collect()
    }

    /// Distinct commits in the selected project(s), newest first not needed.
    fn commits(&self, cx: &gpui::App) -> Vec<(i64, u64, u64)> {
        let index = self.project_index(cx);
        let mut seen = HashSet::new();
        let mut commits = Vec::new();
        for (device, reading) in &self.git {
            for repo in reading.value.iter().flatten() {
                let project = index.get(&(device.clone(), repo.path.clone()));
                if let Some(selected) = &self.project
                    && project.is_none_or(|(id, _)| id != selected)
                {
                    continue;
                }
                for commit in &repo.commits {
                    if seen.insert(commit.hash.clone()) {
                        commits.push((commit.at, commit.additions, commit.deletions));
                    }
                }
            }
        }
        commits
    }

    /// Commits in the last fourteen days, for Home's summary pill.
    pub(super) fn commit_count(&self, cx: &gpui::App) -> Option<usize> {
        (panel_shown(HomePanel::Git, cx) && self.git.values().any(|r| r.value.is_some()))
            .then(|| self.commits(cx).len())
    }

    pub(super) fn git_height(&self) -> f32 {
        GIT_PANEL_HEIGHT
    }

    pub(super) fn render_git(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let commits = self.commits(cx);
        let today = Local::now().date_naive();
        let days: Vec<_> = (0..GIT_DAYS as i64)
            .rev()
            .map(|back| today - chrono::Days::new(back as u64))
            .collect();
        let mut per_day = vec![0usize; days.len()];
        let (mut added, mut deleted) = (0u64, 0u64);
        for (at, additions, deletions) in &commits {
            let Some(date) = Local.timestamp_opt(*at, 0).single().map(|t| t.date_naive()) else {
                continue;
            };
            if let Some(slot) = days.iter().position(|day| *day == date) {
                per_day[slot] += 1;
                added += additions;
                deleted += deletions;
            }
        }
        let total: usize = per_day.iter().sum();
        let max = per_day.iter().copied().max().unwrap_or(0).max(1);
        let loading = self.git.is_empty() || self.git.values().all(|r| r.value.is_none());
        let error = self
            .git
            .values()
            .find_map(|reading| reading.error.clone())
            .filter(|_| loading);
        let bars = days.iter().zip(&per_day).map(|(day, count)| {
            let fraction = *count as f32 / max as f32;
            div()
                .id(SharedString::from(format!("home-git-day-{day}")))
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .justify_end()
                .tooltip(text_tooltip(format!(
                    "{} · {count} commit{}",
                    day.format("%a %b %-d"),
                    if *count == 1 { "" } else { "s" }
                )))
                .child(
                    div()
                        .w_full()
                        .rounded(px(2.0))
                        .when(*count == 0, |bar| bar.h(px(2.0)).bg(theme.wash(0.08)))
                        .when(*count > 0, |bar| {
                            bar.h(gpui::relative(fraction.max(0.06)))
                                .bg(theme.accent.opacity(0.75))
                        }),
                )
        });
        let labels = days.iter().map(|day| {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .justify_center()
                .child(weekday_letter(day.weekday()))
        });
        let summary = div()
            .flex()
            .items_end()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(26.0))
                    .text_color(theme.text)
                    .child(if loading && error.is_none() {
                        "–".to_owned()
                    } else {
                        total.to_string()
                    }),
            )
            .child(
                div()
                    .pb(px(5.0))
                    .text_size(crate::typography::ui_rems(12.5))
                    .text_color(theme.text_muted)
                    .child(format!("commits · {GIT_DAYS} days")),
            )
            .child(div().flex_1())
            .child(
                div()
                    .pb(px(5.0))
                    .flex()
                    .gap(px(6.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .child(
                        div()
                            .text_color(theme.success)
                            .child(format!("+{}", compact(added))),
                    )
                    .child(
                        div()
                            .text_color(theme.danger)
                            .child(format!("-{}", compact(deleted))),
                    ),
            );
        panel(&theme, "home-git-activity")
            .child(header(
                &theme,
                icons::GIT_BRANCH,
                "Git activity",
                Some(self.project_picker(&theme, cx)),
            ))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px(px(14.0))
                    .pb(px(10.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(summary)
                    .child(match error {
                        Some(error) => note(&theme, &error).into_any_element(),
                        None => div()
                            .h(px(CHART_HEIGHT))
                            .flex()
                            .gap(px(4.0))
                            .children(bars)
                            .into_any_element(),
                    })
                    .child(
                        div()
                            .flex()
                            .gap(px(4.0))
                            .text_size(crate::typography::ui_rems(10.5))
                            .text_color(theme.text_muted)
                            .children(labels),
                    ),
            )
            .into_any_element()
    }

    fn project_picker(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut projects: Vec<(String, String)> = self
            .project_index(cx)
            .into_values()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        projects.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        if self
            .project
            .as_ref()
            .is_some_and(|id| !projects.iter().any(|(project, _)| project == id))
        {
            self.project = None;
        }
        let label = self
            .project
            .as_ref()
            .and_then(|id| projects.iter().find(|(project, _)| project == id))
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| "All projects".into());
        let mut trigger = chip(theme, "home-git-project")
            .child(div().max_w(px(140.0)).truncate().child(label))
            .child(
                icon(icons::ALT_ARROW_DOWN)
                    .size(px(11.0))
                    .text_color(theme.text_muted),
            )
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|stats, _, _, _| stats.project_menu.note_trigger_press()),
            )
            .on_click(cx.listener(|stats, _, _, cx| {
                if stats.project_menu.take_press_was_open() {
                    return;
                }
                if stats.project_menu.is_open() {
                    stats.close_project_menu(cx);
                } else {
                    stats.project_menu.open(());
                    cx.notify();
                }
            }));
        if self.project_menu.get().is_some() {
            let options = std::iter::once((None, "All projects".to_owned()))
                .chain(projects.into_iter().map(|(id, name)| (Some(id), name)));
            let rows = options.map(|(id, name)| {
                let selected = self.project == id;
                let key = format!("home-git-project-{}", id.as_deref().unwrap_or("all"));
                crate::popover::menu_row(theme, selected, key.clone())
                    .id(SharedString::from(key))
                    .child(div().flex_1().min_w_0().truncate().child(name))
                    .when(selected, |row| {
                        row.child(icon(icons::CHECK).size(px(14.0)).text_color(theme.text))
                    })
                    .on_click(cx.listener(move |stats, _, _, cx| {
                        stats.project = id.clone();
                        stats.close_project_menu(cx);
                    }))
            });
            let menu = crate::popover::popover_card(theme)
                .min_w(px(200.0))
                .flex()
                .flex_col()
                .children(rows)
                .on_mouse_down_out(cx.listener(|stats, _, _, cx| {
                    if stats.project_menu.is_open() {
                        stats.close_project_menu(cx);
                    }
                }))
                .into_any_element();
            trigger = trigger.child(crate::popover::anchored_menu_below_end(
                "home-git-project-menu",
                menu,
                self.project_menu.closing_since(),
            ));
        }
        trigger.into_any_element()
    }

    fn close_project_menu(&mut self, cx: &mut Context<Self>) {
        if self.project_menu.begin_close() {
            crate::popover::reap_popup(cx, |stats: &mut Self| &mut stats.project_menu);
        }
        cx.notify();
    }

    // ---- devices ----

    /// Online devices whose engine can report its health.
    fn health_hosts(&self, cx: &gpui::App) -> Vec<Host> {
        let mut hosts = self.hosts(cx);
        hosts.retain(|host| !self.health.get(&host.id).is_some_and(|r| r.unsupported));
        hosts
    }

    pub(super) fn devices_height(&self, cx: &gpui::App) -> f32 {
        HEADER_HEIGHT + 8.0 + DEVICE_ROW_HEIGHT * self.health_hosts(cx).len().max(1) as f32
    }

    pub(super) fn render_devices(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let hosts = self.health_hosts(cx);
        let rows: Vec<AnyElement> = hosts
            .iter()
            .map(|host| {
                let reading = self.health.get(&host.id);
                device_row(&theme, host, reading)
            })
            .collect();
        panel(&theme, "home-devices")
            .child(header(&theme, icons::CPU, "Devices", None))
            .child(
                div()
                    .id("home-devices-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(14.0))
                    .pb(px(8.0))
                    .children(rows)
                    .when(hosts.is_empty(), |list| {
                        list.child(note(&theme, "No devices online"))
                    }),
            )
            .into_any_element()
    }

    // ---- builds ----

    /// Latest runs across every device's repos, one entry per run.
    fn runs(&self) -> (Vec<(String, CiRun)>, Option<String>) {
        let mut seen = HashSet::new();
        let mut runs = Vec::new();
        let mut error = None;
        for reading in self.ci.values() {
            for repo in reading.value.iter().flatten() {
                if let Some(message) = &repo.error {
                    error.get_or_insert_with(|| format!("{}: {message}", repo.repo));
                }
                for run in &repo.runs {
                    if seen.insert(run.id) {
                        runs.push((repo.repo.clone(), run.clone()));
                    }
                }
            }
            if let Some(message) = &reading.error {
                error.get_or_insert_with(|| message.clone());
            }
        }
        runs.sort_by(|a, b| b.1.created_at.cmp(&a.1.created_at));
        runs.truncate(MAX_RUN_ROWS);
        (runs, error)
    }

    pub(super) fn builds_height(&self) -> f32 {
        let rows = self.runs().0.len().max(1);
        HEADER_HEIGHT + 8.0 + RUN_ROW_HEIGHT * rows as f32
    }

    pub(super) fn render_builds(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let (runs, error) = self.runs();
        let loaded = self.ci.values().any(|reading| reading.at.is_some());
        let now = Utc::now();
        let rows: Vec<AnyElement> = runs
            .into_iter()
            .map(|(repo, run)| {
                let (color, state) = run_state(&run, &theme);
                let started = parse_time(&run.created_at);
                let finished = parse_time(&run.updated_at);
                let timing = match (started, finished) {
                    (Some(start), Some(end)) if run.status == "completed" => {
                        format!("{} · {}", ago(end, now), duration(end - start))
                    }
                    (Some(start), _) => format!("{} · {}", state, duration(now - start)),
                    _ => state.to_owned(),
                };
                let url = run.url.clone();
                div()
                    .id(SharedString::from(format!("home-build-{}", run.id)))
                    .h(px(RUN_ROW_HEIGHT))
                    .px(px(10.0))
                    .rounded(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .cursor_pointer()
                    .hover(|row| row.bg(theme.element_hover))
                    .tooltip(text_tooltip(format!(
                        "{repo}\n{} · {}\n{}\n{state}",
                        run.workflow, run.branch, run.title
                    )))
                    .on_click(move |_, _, cx| cx.open_url(&url))
                    .child(div().size(px(8.0)).flex_none().rounded_full().bg(color))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(crate::typography::ui_rems(13.0))
                                    .text_color(theme.text)
                                    .child(run.title.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(crate::typography::ui_rems(11.5))
                                    .text_color(theme.text_muted)
                                    .child(format!("{} · {}", run.workflow, run.branch)),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(crate::typography::ui_rems(11.5))
                            .text_color(theme.text_muted)
                            .child(timing),
                    )
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        let message = match (empty, loaded, error) {
            (true, _, Some(error)) => Some(error),
            (true, true, None) => Some("No GitHub Actions runs".to_owned()),
            (true, false, None) => Some("Loading runs…".to_owned()),
            _ => None,
        };
        panel(&theme, "home-builds")
            .child(header(&theme, icons::ACTION_BUILD, "Builds", None))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px(px(4.0))
                    .pb(px(8.0))
                    .children(rows)
                    .children(
                        message.map(|message| div().px(px(10.0)).child(note(&theme, &message))),
                    ),
            )
            .into_any_element()
    }

    // ---- services ----

    /// Every device's listeners with the device name, local first.
    fn service_rows(&self, cx: &gpui::App) -> Vec<(Host, ListeningService)> {
        let mut rows = Vec::new();
        for host in self.hosts(cx) {
            if let Some(services) = self
                .services
                .get(&host.id)
                .and_then(|reading| reading.value.as_ref())
            {
                rows.extend(
                    services
                        .iter()
                        .map(|service| (host.clone(), service.clone())),
                );
            }
        }
        rows
    }

    pub(super) fn services_height(&self, cx: &gpui::App) -> f32 {
        let rows = self.service_rows(cx).len().clamp(1, MAX_SERVICE_ROWS);
        HEADER_HEIGHT + 8.0 + SERVICE_ROW_HEIGHT * rows as f32
    }

    pub(super) fn render_services(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let rows = self.service_rows(cx);
        let many_hosts = rows
            .iter()
            .map(|(host, _)| host.id.as_str())
            .collect::<HashSet<_>>()
            .len()
            > 1;
        let names: HashMap<(String, String), String> = {
            let state = self.state.read(cx);
            state
                .spaces
                .iter()
                .map(|space| {
                    (
                        (space.device_id.clone(), space.path.clone()),
                        state.representative_space(space).display_name().to_owned(),
                    )
                })
                .collect()
        };
        let all_ports = crate::settings::home_services_all_ports(cx);
        let loaded = self.services.values().any(|reading| reading.at.is_some());
        let error = self
            .services
            .values()
            .find_map(|reading| reading.error.clone())
            .filter(|_| rows.is_empty());
        let list: Vec<AnyElement> = rows
            .into_iter()
            .map(|(host, service)| {
                let project = service
                    .path
                    .as_ref()
                    .and_then(|path| names.get(&(host.id.clone(), path.clone())))
                    .cloned();
                let name = service
                    .framework
                    .clone()
                    .unwrap_or_else(|| service.command.clone());
                let place = match (&project, many_hosts) {
                    (Some(project), true) => format!("{project} · {}", host.name),
                    (Some(project), false) => project.clone(),
                    (None, true) => host.name.clone(),
                    (None, false) => String::new(),
                };
                let url = format!("http://localhost:{}", service.port);
                // Only this device's ports are reachable at localhost.
                let local = host.local;
                div()
                    .id(SharedString::from(format!(
                        "home-service-{}-{}-{}",
                        host.id, service.pid, service.port
                    )))
                    .h(px(SERVICE_ROW_HEIGHT))
                    .px(px(10.0))
                    .rounded(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .tooltip(text_tooltip(format!(
                        "{} (pid {}) on {}{}",
                        service.command,
                        service.pid,
                        host.name,
                        service
                            .path
                            .as_ref()
                            .map(|path| format!("\n{path}"))
                            .unwrap_or_default()
                    )))
                    .when(local, |row| {
                        row.cursor_pointer()
                            .hover(|row| row.bg(theme.element_hover))
                            .on_click(move |_, _, cx| cx.open_url(&url))
                    })
                    .child(
                        div()
                            .w(px(52.0))
                            .flex_none()
                            .text_size(crate::typography::ui_rems(12.5))
                            .text_color(theme.text)
                            .child(format!(":{}", service.port)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(12.5))
                            .text_color(theme.text)
                            .child(name),
                    )
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(180.0))
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.5))
                            .child(place),
                    )
                    .into_any_element()
            })
            .collect();
        let message = if list.is_empty() {
            Some(match (error, loaded) {
                (Some(error), _) => error,
                (None, false) => "Reading ports…".to_owned(),
                (None, true) if all_ports => "Nothing listening".to_owned(),
                (None, true) => "Nothing listening in your projects".to_owned(),
            })
        } else {
            None
        };
        let scope = chip(&theme, "home-services-scope")
            .child(if all_ports { "All ports" } else { "Projects" })
            .tooltip(text_tooltip(if all_ports {
                "Showing every port; click for project ports only"
            } else {
                "Showing ports started in your projects; click for all ports"
            }))
            .on_click(cx.listener(move |stats, _, _, cx| {
                crate::settings::set_home_services_all_ports(!all_ports, cx);
                stats.poll(cx);
                cx.notify();
            }));
        panel(&theme, "home-services")
            .child(header(
                &theme,
                icons::GLOBE,
                "Running services",
                Some(scope.into_any_element()),
            ))
            .child(
                div()
                    .id("home-services-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(4.0))
                    .pb(px(8.0))
                    .children(list)
                    .children(
                        message.map(|message| div().px(px(10.0)).child(note(&theme, &message))),
                    ),
            )
            .into_any_element()
    }

    // ---- spend ----

    fn spend_rows(&self) -> Vec<(Option<HarnessId>, u64, f64)> {
        let Some(summary) = &self.spend.value else {
            return Vec::new();
        };
        let mut rows: Vec<_> = summary
            .providers
            .iter()
            .map(|group| {
                (
                    group.harness,
                    group.totals.usage.total().unwrap_or(0),
                    group.totals.cost_usd(),
                )
            })
            .filter(|(_, tokens, cost)| *tokens > 0 || *cost > 0.0)
            .collect();
        rows.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| b.1.cmp(&a.1)));
        rows.truncate(MAX_SPEND_ROWS);
        rows
    }

    pub(super) fn spend_height(&self) -> f32 {
        HEADER_HEIGHT + 44.0 + SPEND_ROW_HEIGHT * self.spend_rows().len().max(1) as f32 + 8.0
    }

    pub(super) fn render_spend(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let rows = self.spend_rows();
        let (today, week) = match &self.spend.value {
            Some(summary) => {
                let midnight = Local::now()
                    .date_naive()
                    .and_hms_opt(0, 0, 0)
                    .and_then(|start| Local.from_local_datetime(&start).single())
                    .map(|start| start.timestamp())
                    .unwrap_or(i64::MAX);
                let today: f64 = summary
                    .buckets
                    .iter()
                    .filter(|bucket| bucket.started_at >= midnight)
                    .flat_map(|bucket| &bucket.groups)
                    .map(|group| group.totals.cost_usd())
                    .sum();
                (Some(today), Some(summary.totals.cost_usd()))
            }
            None => (None, None),
        };
        let figure = |label: &str, value: Option<f64>| {
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(18.0))
                        .text_color(theme.text)
                        .child(value.map(money).unwrap_or_else(|| "–".into())),
                )
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(11.5))
                        .text_color(theme.text_muted)
                        .child(label.to_owned()),
                )
        };
        let list: Vec<AnyElement> = rows
            .iter()
            .map(|(harness, tokens, cost)| {
                let (path, tint) = harness
                    .map(crate::pickers::harness_brand_icon)
                    .unwrap_or((icons::BOT, None));
                div()
                    .h(px(SPEND_ROW_HEIGHT))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(crate::typography::ui_rems(12.5))
                    .child(
                        icon(path)
                            .size(px(13.0))
                            .text_color(tint.unwrap_or(theme.text_muted)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.text)
                            .child(crate::usage_dashboard::provider_label(*harness)),
                    )
                    .child(
                        div()
                            .text_color(theme.text_muted)
                            .child(format!("{} tokens", compact(*tokens))),
                    )
                    .child(
                        div()
                            .w(px(72.0))
                            .flex()
                            .justify_end()
                            .text_color(theme.text)
                            .child(money(*cost)),
                    )
                    .into_any_element()
            })
            .collect();
        let message = if list.is_empty() {
            Some(match (&self.spend.value, &self.spend.error) {
                (None, Some(error)) => error.clone(),
                (None, None) => "Loading usage…".to_owned(),
                (Some(_), _) => "No usage in the last 7 days".to_owned(),
            })
        } else {
            None
        };
        let open = chip(&theme, "home-spend-open")
            .child("Usage")
            .child(
                icon(icons::ARROW_UP_RIGHT)
                    .size(px(11.0))
                    .text_color(theme.text_muted),
            )
            .on_click(cx.listener(|_, _, _, cx| cx.emit(HomeStatsEvent::OpenUsage)));
        panel(&theme, "home-spend")
            .child(header(
                &theme,
                icons::CALENDAR,
                "Spend",
                Some(open.into_any_element()),
            ))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px(px(14.0))
                    .pb(px(8.0))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(44.0))
                            .flex()
                            .gap(px(24.0))
                            .child(figure("today", today))
                            .child(figure("7 days", week)),
                    )
                    .children(list)
                    .children(message.map(|message| note(&theme, &message))),
            )
            .into_any_element()
    }

    // ---- customize ----

    /// The Customize chip and its menu of panel toggles.
    pub(super) fn render_customize(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let mut trigger = chip(&theme, "home-customize")
            .child(
                icon(icons::TUNING)
                    .size(px(12.0))
                    .text_color(theme.text_muted),
            )
            .child("Customize")
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|stats, _, _, _| stats.customize_menu.note_trigger_press()),
            )
            .on_click(cx.listener(|stats, _, _, cx| {
                if stats.customize_menu.take_press_was_open() {
                    return;
                }
                if stats.customize_menu.is_open() {
                    stats.close_customize_menu(cx);
                } else {
                    stats.customize_menu.open(());
                    cx.notify();
                }
            }));
        if self.customize_menu.get().is_some() {
            let rows = HomePanel::ALL.into_iter().map(|panel| {
                let shown = panel_shown(panel, cx);
                let key = format!("home-customize-{}", panel.key());
                crate::popover::menu_row(&theme, false, key.clone())
                    .id(SharedString::from(key))
                    .child(div().flex_1().min_w_0().truncate().child(panel.label()))
                    .child(div().size(px(14.0)).flex_none().when(shown, |slot| {
                        slot.child(icon(icons::CHECK).size(px(14.0)).text_color(theme.text))
                    }))
                    .on_click(cx.listener(move |stats, _, _, cx| {
                        set_panel_shown(panel, !panel_shown(panel, cx), cx);
                        // A newly shown panel loads now, not on its next due tick.
                        stats.poll(cx);
                        cx.notify();
                    }))
            });
            let settings_row = crate::popover::menu_row(&theme, false, "home-customize-settings")
                .id("home-customize-settings")
                .child(div().flex_1().child("Home settings…"))
                .on_click(cx.listener(|stats, _, _, cx| {
                    stats.close_customize_menu(cx);
                    cx.emit(HomeStatsEvent::OpenSettings);
                }));
            let menu = crate::popover::popover_card(&theme)
                .min_w(px(200.0))
                .flex()
                .flex_col()
                .children(rows)
                .child(crate::popover::menu_separator())
                .child(settings_row)
                .on_mouse_down_out(cx.listener(|stats, _, _, cx| {
                    if stats.customize_menu.is_open() {
                        stats.close_customize_menu(cx);
                    }
                }))
                .into_any_element();
            trigger = trigger.child(crate::popover::anchored_menu_below_end(
                "home-customize-menu",
                menu,
                self.customize_menu.closing_since(),
            ));
        }
        trigger.into_any_element()
    }

    fn close_customize_menu(&mut self, cx: &mut Context<Self>) {
        if self.customize_menu.begin_close() {
            crate::popover::reap_popup(cx, |stats: &mut Self| &mut stats.customize_menu);
        }
        cx.notify();
    }
}

fn panel(theme: &Theme, id: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .debug_selector(move || id.into())
        .size_full()
        .flex()
        .flex_col()
        .rounded(px(super::working_now::PANEL_RADIUS))
        .border_1()
        .border_color(theme.border)
        .bg(super::working_now::panel_background(theme))
        .overflow_hidden()
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(theme.text_muted)
}

fn header(
    theme: &Theme,
    glyph: &'static str,
    title: &'static str,
    trailing: Option<AnyElement>,
) -> gpui::Div {
    div()
        .h(px(HEADER_HEIGHT))
        .flex_none()
        .pl(px(14.0))
        .pr(px(8.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(icon(glyph).size(px(14.0)).text_color(theme.text_muted))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(crate::typography::ui_rems(13.0))
                .text_color(theme.text)
                .child(title),
        )
        .children(trailing)
}

/// A small header control: trigger for a menu or a link.
fn chip(theme: &Theme, id: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .relative()
        .h(px(24.0))
        .px(px(8.0))
        .flex()
        .items_center()
        .gap(px(4.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(theme.border)
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(theme.text)
        .cursor_pointer()
        .hover(|chip| chip.bg(theme.element_hover))
}

fn note(theme: &Theme, text: &str) -> gpui::Div {
    div()
        .py(px(8.0))
        .text_size(crate::typography::ui_rems(12.5))
        .text_color(theme.text_muted)
        .truncate()
        .child(text.to_owned())
}

fn device_row(theme: &Theme, host: &Host, reading: Option<&Reading<DeviceHealth>>) -> AnyElement {
    let health = reading.and_then(|reading| reading.value.as_ref());
    let glyph = if host.platform.eq_ignore_ascii_case("linux") {
        icons::REMOTE_SERVER
    } else {
        icons::LAPTOP
    };
    let meta = match health {
        Some(health) => {
            let mut parts = vec![
                format!("{} CPU", health.cpu_count),
                gigabytes(health.memory_total),
            ];
            if let Some(disk) = health.disk_total {
                parts.push(format!("{} disk", gigabytes(disk)));
            }
            if let Some(started) = health
                .engine_started_at
                .and_then(|at| DateTime::from_timestamp(at, 0))
            {
                parts.push(format!("up {}", duration(Utc::now() - started)));
            }
            parts.join(" · ")
        }
        None => match reading.and_then(|reading| reading.error.as_deref()) {
            Some(error) => error.to_owned(),
            None => "Reading…".to_owned(),
        },
    };
    let tooltip = health.map(|health| {
        let mut lines = vec![format!("Engine v{}", health.engine_version)];
        if let Some(started) = health
            .engine_started_at
            .and_then(|at| Local.timestamp_opt(at, 0).single())
        {
            lines.push(format!("Started {}", started.format("%b %-d, %H:%M")));
        }
        if health.swap_total > 0 {
            lines.push(format!(
                "Swap {} of {}",
                gigabytes(health.swap_used),
                gigabytes(health.swap_total)
            ));
        }
        if let Some(pressure) = health.memory_pressure {
            lines.push(format!("Memory pressure {pressure:.1}%"));
        }
        lines.join("\n")
    });
    let meters = health.map(|health| {
        let memory = fraction(health.memory_used, health.memory_total);
        // Memory stalls are what get services killed; flag them even when
        // the used share still looks fine.
        let memory_level = if health
            .memory_pressure
            .is_some_and(|pressure| pressure >= 10.0)
        {
            Some(theme.danger)
        } else {
            None
        };
        div()
            .flex()
            .gap(px(12.0))
            .child(meter(theme, "CPU", health.cpu_percent / 100.0, None))
            .child(meter(theme, "Mem", memory, memory_level))
            .children(
                health
                    .disk_total
                    .zip(health.disk_used)
                    .map(|(total, used)| meter(theme, "Disk", fraction(used, total), None)),
            )
    });
    div()
        .id(SharedString::from(format!("home-device-{}", host.id)))
        .h(px(DEVICE_ROW_HEIGHT))
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(6.0))
        .when_some(tooltip, |row, tooltip| row.tooltip(text_tooltip(tooltip)))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .min_w_0()
                .child(icon(glyph).size(px(13.0)).text_color(theme.text_muted))
                .child(
                    div()
                        .flex_none()
                        .max_w(px(180.0))
                        .truncate()
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text)
                        .child(host.name.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(crate::typography::ui_rems(11.5))
                        .child(meta),
                ),
        )
        .children(meters)
        .into_any_element()
}

/// Label, bar, and percentage; warning and critical shares change colour.
fn meter(
    theme: &Theme,
    label: &'static str,
    fraction: f32,
    color: Option<gpui::Hsla>,
) -> gpui::Div {
    let fraction = fraction.clamp(0.0, 1.0);
    let fill = color.unwrap_or_else(|| usage_color(usage_level(fraction), theme));
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .child(
            div()
                .flex()
                .text_size(crate::typography::ui_rems(11.0))
                .child(div().flex_1().child(label))
                .child(format!("{:.0}%", fraction * 100.0)),
        )
        .child(
            div()
                .h(px(4.0))
                .rounded_full()
                .overflow_hidden()
                .bg(theme.wash(0.08))
                .when(fraction > 0.0, |bar| {
                    bar.child(
                        div()
                            .h_full()
                            .w(gpui::relative(fraction.max(0.015)))
                            .rounded_full()
                            .bg(fill.opacity(0.85)),
                    )
                }),
        )
}

fn run_state(run: &CiRun, theme: &Theme) -> (gpui::Hsla, &'static str) {
    match (run.status.as_str(), run.conclusion.as_deref()) {
        ("completed", Some("success")) => (theme.success, "Passed"),
        ("completed", Some("failure" | "startup_failure" | "timed_out")) => {
            (theme.danger, "Failed")
        }
        ("completed", Some("cancelled")) => (theme.text_muted, "Cancelled"),
        ("completed", _) => (theme.text_muted, "Done"),
        ("queued" | "waiting" | "pending" | "requested", _) => (theme.text_muted, "Queued"),
        _ => (theme.warning, "Running"),
    }
}

fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn ago(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    format!("{} ago", duration(now - at))
}

fn duration(span: chrono::TimeDelta) -> String {
    let seconds = span.num_seconds().max(0);
    if seconds >= 86_400 {
        format!("{}d", seconds / 86_400)
    } else if seconds >= 3600 {
        format!("{}h {}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

fn weekday_letter(day: chrono::Weekday) -> &'static str {
    match day {
        chrono::Weekday::Mon => "M",
        chrono::Weekday::Tue | chrono::Weekday::Thu => "T",
        chrono::Weekday::Wed => "W",
        chrono::Weekday::Fri => "F",
        chrono::Weekday::Sat | chrono::Weekday::Sun => "S",
    }
}

fn fraction(used: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64) as f32
    }
}

fn gigabytes(bytes: u64) -> String {
    let value = bytes as f64 / 1_073_741_824.0;
    if value >= 100.0 {
        format!("{value:.0} GB")
    } else {
        format!("{value:.1} GB")
    }
}

fn compact(value: u64) -> String {
    crate::usage_dashboard::compact(value as f64)
}

fn money(value: f64) -> String {
    crate::usage_dashboard::money(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_spans_and_sizes() {
        assert_eq!(duration(chrono::TimeDelta::seconds(42)), "42s");
        assert_eq!(
            duration(chrono::TimeDelta::seconds(3 * 3600 + 120)),
            "3h 2m"
        );
        assert_eq!(duration(chrono::TimeDelta::seconds(-5)), "0s");
        assert_eq!(gigabytes(16 * 1_073_741_824), "16.0 GB");
        assert_eq!(fraction(1, 0), 0.0);
    }
}
