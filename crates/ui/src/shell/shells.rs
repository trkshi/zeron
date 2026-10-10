//! A compact Home preview and on-demand sidebar for read-only agent shells.

use gpui::{
    AnyElement, App, ClipboardItem, Context, ElementId, Entity, FocusHandle, Render, ScrollHandle,
    SharedString, Task, Window, div, prelude::*, px,
};
use std::collections::HashSet;
use std::time::Duration;
use zeron_proto::{ShellTask, ShellTaskOutput, ShellTaskStatus, ShellTasksSnapshot};
use zeron_rpc::methods;

use crate::icons::{self, icon};
use crate::settings::widgets::text_tooltip;
use crate::state::AppState;
use crate::theme::Theme;

const HEADER_HEIGHT: f32 = 30.0;
const ROW_HEIGHT: f32 = 50.0;
const PREVIEW_LIMIT: usize = 3;
const SIDEBAR_WIDTH: f32 = 460.0;

fn dismissible(task: &ShellTask) -> bool {
    task.status != ShellTaskStatus::Running
        && (task.finished_at.is_some()
            || matches!(
                task.status,
                ShellTaskStatus::Completed | ShellTaskStatus::Failed | ShellTaskStatus::Stopped
            ))
}

pub(super) struct Shells {
    state: Entity<AppState>,
    target: Option<String>,
    visible: bool,
    online: bool,
    connected: bool,
    interactive: bool,
    generation: u64,
    snapshot: ShellTasksSnapshot,
    dismissed: HashSet<String>,
    error: Option<String>,
    sidebar_open: bool,
    sidebar_focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    sidebar_scroll: ScrollHandle,
    output_scroll: ScrollHandle,
    selected: Option<ShellTask>,
    output: Option<ShellTaskOutput>,
    watch_task: Option<Task<()>>,
    output_task: Option<Task<()>>,
    clock_task: Option<Task<()>>,
    pub(super) scroll: ScrollHandle,
}

impl Shells {
    pub(super) fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        Self {
            state,
            target: None,
            visible: false,
            online: false,
            connected: false,
            interactive: false,
            generation: 0,
            snapshot: ShellTasksSnapshot::default(),
            dismissed: HashSet::new(),
            error: None,
            sidebar_open: false,
            sidebar_focus: cx.focus_handle(),
            previous_focus: None,
            sidebar_scroll: ScrollHandle::new(),
            output_scroll: ScrollHandle::new(),
            selected: None,
            output: None,
            watch_task: None,
            output_task: None,
            clock_task: None,
            scroll: ScrollHandle::new(),
        }
    }

    pub(super) fn track(&mut self, visible: bool, window_active: bool, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let device = state.effective_device_id();
        let target = device
            .clone()
            .filter(|id| Some(id) != state.local_device_id.as_ref());
        let online = matches!(state.connection, zeron_proto::view::ConnectionStatus::Ready)
            && state.engine().is_some()
            && device
                .as_ref()
                .is_none_or(|id| state.device_online(id, chrono::Utc::now()));
        let visible = visible && window_active;
        let changed = target != self.target || visible != self.visible || online != self.online;
        if target != self.target {
            self.snapshot = ShellTasksSnapshot::default();
            self.dismissed.clear();
            self.selected = None;
            self.output = None;
        }
        self.target = target;
        self.visible = visible;
        self.online = online;
        if changed {
            self.generation += 1;
            self.watch_task = None;
            self.output_task = None;
            self.clock_task = None;
            self.connected = false;
            self.error = None;
            cx.notify();
        }
        if !visible || !online || self.watch_task.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let target = self.target.clone();
        let generation = self.generation;
        self.watch_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let request = engine.client().subscribe_checked(
                    methods::WATCH_SHELL_TASKS,
                    serde_json::json!({"targetDeviceId": target}),
                );
                let result = match futures::future::select(
                    Box::pin(request),
                    Box::pin(cx.background_executor().timer(Duration::from_secs(15))),
                )
                .await
                {
                    futures::future::Either::Left((result, _)) => result,
                    futures::future::Either::Right(_) => Err(zeron_rpc::RpcError::Failed(
                        "Shell monitor connection timed out".into(),
                    )),
                };
                let unsupported = matches!(&result, Err(zeron_rpc::RpcError::UnknownMethod(_)));
                if let Ok(mut stream) = result {
                    while let Some(value) = stream.recv().await {
                        let Ok(snapshot) = serde_json::from_value::<ShellTasksSnapshot>(value)
                        else {
                            break;
                        };
                        if this
                            .update(cx, |shells, cx| {
                                if shells.generation != generation {
                                    return;
                                }
                                shells.connected = true;
                                shells.error = None;
                                shells.snapshot = snapshot;
                                // Forget expired dismissals and let a restarted task reappear.
                                shells.dismissed.retain(|id| {
                                    shells
                                        .snapshot
                                        .tasks
                                        .iter()
                                        .any(|task| task.id == *id && dismissible(task))
                                });
                                if let Some(selected) = &shells.selected {
                                    if let Some(task) = shells
                                        .snapshot
                                        .tasks
                                        .iter()
                                        .find(|task| task.id == selected.id)
                                    {
                                        shells.selected = Some(task.clone());
                                    } else {
                                        shells.output_task = None;
                                        shells.selected = None;
                                        shells.output = None;
                                    }
                                }
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                if this
                    .update(cx, |shells, cx| {
                        if shells.generation != generation {
                            return;
                        }
                        shells.connected = false;
                        shells.error = Some(
                            if unsupported {
                                "Update this device's engine to monitor shells"
                            } else {
                                "Shell monitor disconnected; reconnecting..."
                            }
                            .into(),
                        );
                        cx.notify();
                    })
                    .is_err()
                    || unsupported
                {
                    return;
                }
                cx.background_executor().timer(Duration::from_secs(5)).await;
            }
        }));
        self.clock_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this
                    .update(cx, |shells, cx| {
                        if shells.connected
                            && shells
                                .snapshot
                                .tasks
                                .iter()
                                .any(|task| task.status == ShellTaskStatus::Running)
                        {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
        }));
        if self.selected.is_some() {
            self.watch_output(cx);
        }
    }

    pub(super) fn set_interactive(&mut self, interactive: bool, cx: &mut Context<Self>) {
        if self.interactive != interactive {
            self.interactive = interactive;
            cx.notify();
        }
    }

    fn visible_tasks(&self, cx: &App) -> Vec<ShellTask> {
        let state = self.state.read(cx);
        let ids: HashSet<_> = state.visible_chats().map(|chat| chat.id.as_str()).collect();
        let mut tasks: Vec<_> = self
            .snapshot
            .tasks
            .iter()
            .filter(|task| {
                ids.contains(task.chat_id.as_str()) && !self.dismissed.contains(&task.id)
            })
            .cloned()
            .collect();
        tasks.sort_by_key(|task| {
            (
                task.status != ShellTaskStatus::Running,
                std::cmp::Reverse(task.started_at),
            )
        });
        tasks
    }

    pub(super) fn desired_height(&self, cx: &App) -> f32 {
        HEADER_HEIGHT
            + 6.0
            + ROW_HEIGHT * self.visible_tasks(cx).len().clamp(1, PREVIEW_LIMIT) as f32
    }

    pub(super) fn sidebar_open(&self) -> bool {
        self.sidebar_open
    }

    fn open_sidebar(
        &mut self,
        task: Option<ShellTask>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.interactive && !self.sidebar_open {
            return;
        }
        let opening = !self.sidebar_open;
        if opening {
            self.previous_focus = window.focused(cx);
            self.sidebar_open = true;
        }
        if let Some(task) = task
            && self
                .selected
                .as_ref()
                .is_none_or(|selected| selected.id != task.id)
        {
            self.output_task = None;
            self.output = None;
            self.output_scroll = ScrollHandle::new();
            self.selected = Some(task);
            self.watch_output(cx);
        }
        if opening {
            window.focus(&self.sidebar_focus, cx);
        }
        cx.notify();
    }

    pub(super) fn close_sidebar(&mut self, cx: &mut Context<Self>) -> Option<FocusHandle> {
        self.sidebar_open = false;
        self.output_task = None;
        self.output = None;
        self.selected = None;
        cx.notify();
        self.previous_focus.take()
    }

    fn dismiss_task(&mut self, id: &str, cx: &mut Context<Self>) {
        if (!self.interactive && !self.sidebar_open)
            || !self
                .snapshot
                .tasks
                .iter()
                .any(|task| task.id == id && dismissible(task))
        {
            return;
        }
        self.dismissed.insert(id.to_owned());
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| selected.id == id)
        {
            self.selected = None;
            self.output_task = None;
            self.output = None;
        }
        cx.notify();
    }

    fn dismiss_finished(&mut self, cx: &mut Context<Self>) {
        for task in self.visible_tasks(cx).into_iter().filter(dismissible) {
            self.dismiss_task(&task.id, cx);
        }
    }

    fn watch_output(&mut self, cx: &mut Context<Self>) {
        if !self.sidebar_open || !self.visible || !self.online {
            return;
        }
        let Some(task) = self.selected.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let target = self.target.clone();
        let generation = self.generation;
        self.output_task = Some(cx.spawn(async move |this, cx| {
            let request = engine.client().subscribe_checked(
                methods::WATCH_SHELL_TASK_OUTPUT,
                serde_json::json!({
                    "targetDeviceId":target, "chatId":task.chat_id, "taskId":task.id,
                }),
            );
            let result = match futures::future::select(
                Box::pin(request),
                Box::pin(cx.background_executor().timer(Duration::from_secs(15))),
            )
            .await
            {
                futures::future::Either::Left((result, _)) => result,
                futures::future::Either::Right(_) => Err(zeron_rpc::RpcError::Failed(
                    "Shell output connection timed out".into(),
                )),
            };
            if let Ok(mut stream) = result {
                while let Some(value) = stream.recv().await {
                    let Ok(output) = serde_json::from_value::<ShellTaskOutput>(value) else {
                        break;
                    };
                    if this
                        .update(cx, |shells, cx| {
                            if shells.generation == generation
                                && shells
                                    .selected
                                    .as_ref()
                                    .is_some_and(|selected| selected.id == task.id)
                            {
                                shells.output = Some(output);
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }
            let _ = this.update(cx, |shells, cx| {
                if shells.generation == generation
                    && shells
                        .selected
                        .as_ref()
                        .is_some_and(|selected| selected.id == task.id)
                {
                    shells
                        .output
                        .get_or_insert_with(ShellTaskOutput::default)
                        .error = Some("Shell output disconnected; reopen to retry".into());
                    cx.notify();
                }
            });
        }));
    }

    fn action_button(
        &self,
        id: impl Into<ElementId>,
        label: &'static str,
        theme: &Theme,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .h(px(24.0))
            .flex_none()
            .px(px(4.0))
            .flex()
            .items_center()
            .justify_center()
            .gap(px(4.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .role(gpui::Role::Button)
            .aria_label(label)
            .tab_index(if self.interactive || self.sidebar_open {
                0
            } else {
                -1
            })
            .tooltip(text_tooltip(label))
            .hover(|button| button.bg(theme.element_hover))
            .focus_visible(|button| button.bg(theme.element_hover).text_color(theme.accent))
            .text_size(crate::typography::ui_rems(10.0))
    }

    fn row(&self, task: &ShellTask, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let state = self.state.read(cx);
        let chat = state.chats.iter().find(|chat| chat.id == task.chat_id);
        let title = chat
            .and_then(|chat| chat.title.as_deref())
            .unwrap_or("New thread");
        let project = zeron_proto::view::project_label(chat.and_then(|chat| chat.cwd.as_deref()));
        let provider = match task.harness {
            zeron_proto::HarnessId::ClaudeCode => "Claude",
            zeron_proto::HarnessId::Codex => "Codex",
            _ => "Agent",
        };
        let disconnected = (!self.online || !self.connected)
            && matches!(
                task.status,
                ShellTaskStatus::Running | ShellTaskStatus::Unknown
            );
        let status = if disconnected {
            "Disconnected"
        } else {
            match task.status {
                ShellTaskStatus::Running => "Running",
                ShellTaskStatus::Completed => "Completed",
                ShellTaskStatus::Failed => "Failed",
                ShellTaskStatus::Stopped => "Stopped",
                ShellTaskStatus::Unknown => "Unknown",
            }
        };
        let color = if disconnected {
            theme.text_muted
        } else {
            match task.status {
                ShellTaskStatus::Completed => theme.success,
                ShellTaskStatus::Failed => theme.danger,
                _ => theme.text_muted,
            }
        };
        let end = task
            .finished_at
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let seconds = end.saturating_sub(task.started_at).max(0) / 1000;
        let elapsed = if seconds >= 60 {
            format!("{}m {}s", seconds / 60, seconds % 60)
        } else {
            format!("{seconds}s")
        };
        let tooltip = format!(
            "{}\n{project} / {title}\n{provider} / {status}{}{}",
            task.command,
            task.exit_code
                .map(|code| format!(" / exit {code}"))
                .unwrap_or_default(),
            if task.start_estimated {
                "\nElapsed time is estimated from first observation"
            } else {
                ""
            }
        );
        let selected = self
            .selected
            .as_ref()
            .is_some_and(|selected| selected.id == task.id);
        let status_element = if dismissible(task) {
            let click_id = task.id.clone();
            let keyboard_id = task.id.clone();
            self.action_button(
                SharedString::from(format!("dismiss-shell-{}", task.id)),
                "Dismiss finished shell",
                theme,
            )
            .text_color(color)
            .on_click(cx.listener(move |shells, _, _, cx| {
                cx.stop_propagation();
                shells.dismiss_task(&click_id, cx);
            }))
            .on_key_down(
                cx.listener(move |shells, event: &gpui::KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        cx.stop_propagation();
                        shells.dismiss_task(&keyboard_id, cx);
                    }
                }),
            )
            .child(status)
            .child(icon(icons::CLOSE).size(px(10.0)))
            .into_any_element()
        } else {
            div()
                .flex_none()
                .text_size(crate::typography::ui_rems(10.0))
                .text_color(color)
                .child(status)
                .into_any_element()
        };
        let click = task.clone();
        let keyboard = task.clone();
        div()
            .id(SharedString::from(format!("home-shell-{}", task.id)))
            .h(px(ROW_HEIGHT))
            .flex_none()
            .w_full()
            .min_w_0()
            .px(px(8.0))
            .py(px(4.0))
            .flex()
            .flex_col()
            .justify_center()
            .gap(px(3.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(gpui::transparent_black())
            .when(selected, |row| row.bg(theme.element_hover))
            .hover(|row| row.bg(theme.element_hover))
            .cursor_pointer()
            .role(gpui::Role::Group)
            .tooltip(text_tooltip(tooltip))
            .on_click(cx.listener(move |shells, _, window, cx| {
                cx.stop_propagation();
                shells.open_sidebar(Some(click.clone()), window, cx);
            }))
            .on_key_down(
                cx.listener(move |shells, event: &gpui::KeyDownEvent, window, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        cx.stop_propagation();
                        shells.open_sidebar(Some(keyboard.clone()), window, cx);
                    }
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .min_w_0()
                    .gap(px(6.0))
                    .child(
                        icon(icons::TERMINAL)
                            .size(px(12.0))
                            .flex_none()
                            .text_color(color),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("shell-command-{}", task.id)))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .line_clamp(1)
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text)
                            .role(gpui::Role::Button)
                            .aria_label(format!("Show shell output: {}", task.command))
                            .tab_index(if self.interactive || self.sidebar_open {
                                0
                            } else {
                                -1
                            })
                            .focus_visible(|command| command.text_color(theme.accent))
                            .child(task.command.clone()),
                    )
                    .child(status_element),
            )
            .child(
                div()
                    .flex()
                    .min_w_0()
                    .items_center()
                    .gap(px(6.0))
                    .pl(px(18.0))
                    .text_size(crate::typography::ui_rems(10.0))
                    .text_color(theme.text_muted)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(format!("{project} / {title} / {provider}")),
                    )
                    .when(self.online && self.connected, |row| {
                        row.child(div().flex_none().child(format!(
                            "{}{elapsed}",
                            if task.start_estimated { "~" } else { "" }
                        )))
                    }),
            )
            .into_any_element()
    }
}

impl Shells {
    fn empty_message(&self) -> &str {
        self.error.as_deref().unwrap_or(if !self.online {
            "Device offline"
        } else if !self.connected {
            "Connecting..."
        } else {
            "No agent shells running"
        })
    }

    // Render outside the Home card so output never expands or clips the preview.
    pub(super) fn render_sidebar(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.sidebar_open {
            return None;
        }
        let theme = Theme::of(cx).clone();
        let tasks = self.visible_tasks(cx);
        let rows: Vec<_> = tasks
            .iter()
            .map(|task| self.row(task, &theme, cx))
            .collect();
        let has_finished = tasks.iter().any(dismissible);
        let running = tasks
            .iter()
            .filter(|task| task.status == ShellTaskStatus::Running)
            .count();
        let available = (f32::from(viewport.height) - Theme::TITLEBAR_HEIGHT - 40.0).max(0.0);
        let list_height = (tasks.len().max(1) as f32 * ROW_HEIGHT + 8.0).min(available * 0.4);
        let mut panel = div()
            .id("shell-sidebar")
            .debug_selector(|| "shell-sidebar".into())
            .absolute()
            .top(px(Theme::TITLEBAR_HEIGHT))
            .bottom_0()
            .right_0()
            .w(px(
                (f32::from(viewport.width) - 12.0).clamp(0.0, SIDEBAR_WIDTH)
            ))
            .min_w_0()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(theme.border)
            .bg(super::working_now::panel_background(&theme))
            .overflow_hidden()
            .occlude()
            .role(gpui::Role::Group)
            .aria_label("Agent shells")
            .track_focus(&self.sidebar_focus)
            .on_mouse_down_out(cx.listener(|shells, _, window, cx| {
                if let Some(focus) = shells.close_sidebar(cx) {
                    window.focus(&focus, cx);
                }
            }))
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(40.0))
                    .flex_none()
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text)
                    .child(
                        icon(icons::TERMINAL)
                            .size(px(14.0))
                            .text_color(theme.text_muted),
                    )
                    .child("Shells")
                    .child(
                        div()
                            .flex_1()
                            .text_size(crate::typography::ui_rems(10.0))
                            .text_color(theme.text_muted)
                            .child(format!("{running} running / {} total", tasks.len())),
                    )
                    .when(has_finished, |header| {
                        header.child(
                            self.action_button(
                                "dismiss-finished-shells",
                                "Dismiss all finished shells",
                                &theme,
                            )
                            .w(px(24.0))
                            .on_click(cx.listener(|shells, _, _, cx| {
                                cx.stop_propagation();
                                shells.dismiss_finished(cx);
                            }))
                            .on_key_down(cx.listener(
                                |shells, event: &gpui::KeyDownEvent, _, cx| {
                                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                        cx.stop_propagation();
                                        shells.dismiss_finished(cx);
                                    }
                                },
                            ))
                            .child(icon(icons::QUEUE_CLOSE).size(px(14.0))),
                        )
                    })
                    .child(
                        self.action_button("close-shell-sidebar", "Close shell sidebar", &theme)
                            .w(px(24.0))
                            .on_click(cx.listener(|shells, _, window, cx| {
                                cx.stop_propagation();
                                if let Some(focus) = shells.close_sidebar(cx) {
                                    window.focus(&focus, cx);
                                }
                            }))
                            .on_key_down(cx.listener(
                                |shells, event: &gpui::KeyDownEvent, window, cx| {
                                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                        cx.stop_propagation();
                                        if let Some(focus) = shells.close_sidebar(cx) {
                                            window.focus(&focus, cx);
                                        }
                                    }
                                },
                            ))
                            .child(icon(icons::CLOSE).size(px(14.0))),
                    ),
            )
            .child(
                div()
                    .id("shell-sidebar-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.sidebar_scroll)
                    .p(px(4.0))
                    .when(self.selected.is_some(), |list| {
                        list.flex_none().h(px(list_height))
                    })
                    .children(rows)
                    .when(tasks.is_empty(), |list| {
                        list.child(
                            div()
                                .px(px(8.0))
                                .py(px(12.0))
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(theme.text_muted)
                                .child(self.empty_message().to_owned()),
                        )
                    }),
            )
            .when(self.snapshot.truncated, |panel| {
                panel.child(
                    div()
                        .flex_none()
                        .px(px(12.0))
                        .py(px(4.0))
                        .text_size(crate::typography::ui_rems(10.0))
                        .text_color(theme.text_muted)
                        .child("Recent tasks"),
                )
            });
        if let Some(selected) = &self.selected {
            panel = panel.child(self.render_output(selected, &theme, cx));
        }
        Some(panel.into_any_element())
    }

    fn render_output(
        &self,
        selected: &ShellTask,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let output = self.output.as_ref();
        let text = output
            .map(|output| output.text.clone())
            .unwrap_or_else(|| "Loading output...".into());
        let message = output.and_then(|output| output.error.as_deref()).unwrap_or(
            if output.is_some_and(|output| output.truncated) {
                "Last 64 KiB"
            } else {
                "Output"
            },
        );
        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_none()
                    .px(px(12.0))
                    .py(px(8.0))
                    .line_clamp(2)
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text)
                    .child(selected.command.clone()),
            )
            .child(
                div()
                    .h(px(28.0))
                    .flex_none()
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .text_size(crate::typography::ui_rems(10.0))
                    .text_color(theme.text_muted)
                    .child(
                        div()
                            .id("shell-output-caption")
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .tooltip(text_tooltip(message.to_owned()))
                            .child(message.to_owned()),
                    )
                    .when(
                        output.is_some_and(|output| !output.text.is_empty()),
                        |header| {
                            header.child(
                                self.action_button("copy-shell-output", "Copy shell output", theme)
                                    .w(px(24.0))
                                    .on_click(cx.listener(|shells, _, _, cx| {
                                        cx.stop_propagation();
                                        if shells.sidebar_open
                                            && let Some(output) = &shells.output
                                        {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                output.text.clone(),
                                            ));
                                        }
                                    }))
                                    .on_key_down(cx.listener(
                                        |shells, event: &gpui::KeyDownEvent, _, cx| {
                                            if shells.sidebar_open
                                                && matches!(
                                                    event.keystroke.key.as_str(),
                                                    "enter" | "space"
                                                )
                                                && let Some(output) = &shells.output
                                            {
                                                cx.stop_propagation();
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    output.text.clone(),
                                                ));
                                            }
                                        },
                                    ))
                                    .child(icon(icons::COPY).size(px(12.0))),
                            )
                        },
                    ),
            )
            .child(
                div()
                    .id("shell-output-tail")
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_y_scroll()
                    .overflow_x_scroll()
                    .track_scroll(&self.output_scroll)
                    .whitespace_nowrap()
                    .px(px(12.0))
                    .pb(px(8.0))
                    .font_family(theme.font_mono.clone())
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text)
                    .child(if text.is_empty() {
                        "No output yet".into()
                    } else {
                        text
                    }),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(status: ShellTaskStatus, finished_at: Option<i64>) -> ShellTask {
        ShellTask {
            id: "shell".into(),
            chat_id: "chat".into(),
            device_id: "device".into(),
            harness: zeron_proto::HarnessId::Codex,
            command: "echo hello".into(),
            status,
            started_at: 0,
            start_estimated: false,
            finished_at,
            exit_code: None,
            output_available: false,
        }
    }

    #[test]
    fn finished_shells_can_be_dismissed() {
        for status in [
            ShellTaskStatus::Completed,
            ShellTaskStatus::Failed,
            ShellTaskStatus::Stopped,
        ] {
            assert!(dismissible(&task(status, None)));
        }
    }

    #[test]
    fn running_shells_cannot_be_dismissed_even_with_a_stale_finish_time() {
        assert!(!dismissible(&task(ShellTaskStatus::Running, None)));
        assert!(!dismissible(&task(ShellTaskStatus::Running, Some(1))));
    }

    #[test]
    fn unknown_shells_can_only_be_dismissed_after_the_runtime_ends() {
        assert!(!dismissible(&task(ShellTaskStatus::Unknown, None)));
        assert!(dismissible(&task(ShellTaskStatus::Unknown, Some(1))));
    }
}

impl Render for Shells {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let tasks = self.visible_tasks(cx);
        let rows: Vec<_> = tasks
            .iter()
            .take(PREVIEW_LIMIT)
            .map(|task| self.row(task, &theme, cx))
            .collect();
        let count = tasks
            .iter()
            .filter(|task| task.status == ShellTaskStatus::Running)
            .count();
        let all_label = if tasks.len() > PREVIEW_LIMIT {
            format!("View all ({})", tasks.len())
        } else {
            "View all".to_owned()
        };
        div()
            .id("home-shells")
            .debug_selector(|| "home-shells".into())
            .size_full()
            .min_w_0()
            .flex()
            .flex_col()
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .bg(super::working_now::panel_background(&theme))
            .overflow_hidden()
            .child(
                div()
                    .h(px(HEADER_HEIGHT))
                    .flex_none()
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text_muted)
                    .child(icon(icons::TERMINAL).size(px(12.0)))
                    .child("Shells")
                    .child(count.to_string())
                    .child(div().flex_1())
                    .child(
                        self.action_button("open-shell-sidebar", "Open shell sidebar", &theme)
                            .on_click(cx.listener(|shells, _, window, cx| {
                                cx.stop_propagation();
                                shells.open_sidebar(None, window, cx);
                            }))
                            .on_key_down(cx.listener(
                                |shells, event: &gpui::KeyDownEvent, window, cx| {
                                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                        cx.stop_propagation();
                                        shells.open_sidebar(None, window, cx);
                                    }
                                },
                            ))
                            .child(all_label)
                            .child(icon(icons::ALT_ARROW_RIGHT).size(px(12.0))),
                    ),
            )
            .child(
                div()
                    .id("home-shells-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .px(px(4.0))
                    .pb(px(4.0))
                    .children(rows)
                    .when(tasks.is_empty(), |list| {
                        list.child(
                            div()
                                .min_h(px(ROW_HEIGHT))
                                .px(px(8.0))
                                .py(px(8.0))
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(theme.text_muted)
                                .child(self.empty_message().to_owned()),
                        )
                    }),
            )
    }
}
