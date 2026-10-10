//! Home's read-only agent shells; subscriptions exist only while visible.

use gpui::{
    AnyElement, ClipboardItem, Context, Entity, Render, ScrollHandle, SharedString, Task, Window,
    div, prelude::*, px,
};
use std::time::Duration;
use zeron_proto::{ShellTask, ShellTaskOutput, ShellTaskStatus, ShellTasksSnapshot};
use zeron_rpc::methods;

use crate::icons::{self, icon};
use crate::settings::widgets::text_tooltip;
use crate::state::AppState;
use crate::theme::Theme;

const HEADER_HEIGHT: f32 = 30.0;
const ROW_HEIGHT: f32 = 46.0;

pub(super) struct Shells {
    state: Entity<AppState>,
    target: Option<String>,
    visible: bool,
    online: bool,
    connected: bool,
    interactive: bool,
    generation: u64,
    snapshot: ShellTasksSnapshot,
    error: Option<String>,
    selected: Option<ShellTask>,
    output: Option<ShellTaskOutput>,
    watch_task: Option<Task<()>>,
    output_task: Option<Task<()>>,
    clock_task: Option<Task<()>>,
    pub(super) scroll: ScrollHandle,
}

impl Shells {
    pub(super) fn new(state: Entity<AppState>, _: &mut Context<Self>) -> Self {
        Self {
            state,
            target: None,
            visible: false,
            online: false,
            connected: false,
            interactive: false,
            generation: 0,
            snapshot: ShellTasksSnapshot::default(),
            error: None,
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

    pub(super) fn desired_height(&self, cx: &gpui::App) -> f32 {
        let state = self.state.read(cx);
        let count = self
            .snapshot
            .tasks
            .iter()
            .filter(|task| state.visible_chats().any(|chat| chat.id == task.chat_id))
            .count();
        HEADER_HEIGHT
            + 6.0
            + ROW_HEIGHT * count.clamp(1, 4) as f32
            + if self.selected.is_some() { 168.0 } else { 0.0 }
    }

    fn toggle_output(&mut self, task: ShellTask, cx: &mut Context<Self>) {
        if !self.interactive {
            return;
        }
        self.output_task = None;
        self.output = None;
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| selected.id == task.id)
        {
            self.selected = None;
        } else {
            self.selected = Some(task);
            self.watch_output(cx);
            self.scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    fn watch_output(&mut self, cx: &mut Context<Self>) {
        if !self.visible || !self.online {
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
            .focus_visible(|row| row.border_color(theme.accent))
            .cursor_pointer()
            .role(gpui::Role::Button)
            .aria_label(tooltip.clone())
            .tab_index(if self.interactive { 0 } else { -1 })
            .tooltip(text_tooltip(tooltip))
            .on_click(cx.listener(move |shells, _, _, cx| {
                cx.stop_propagation();
                shells.toggle_output(click.clone(), cx);
            }))
            .on_key_down(
                cx.listener(move |shells, event: &gpui::KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        cx.stop_propagation();
                        shells.toggle_output(keyboard.clone(), cx);
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
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .line_clamp(1)
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text)
                            .child(task.command.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(crate::typography::ui_rems(10.0))
                            .text_color(color)
                            .child(status),
                    ),
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

impl Render for Shells {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let state = self.state.read(cx);
        let ids: std::collections::HashSet<_> =
            state.visible_chats().map(|chat| chat.id.as_str()).collect();
        let mut tasks: Vec<_> = self
            .snapshot
            .tasks
            .iter()
            .filter(|task| ids.contains(task.chat_id.as_str()))
            .cloned()
            .collect();
        tasks.sort_by_key(|task| {
            (
                task.status != ShellTaskStatus::Running,
                std::cmp::Reverse(task.started_at),
            )
        });
        let rows: Vec<_> = tasks
            .iter()
            .map(|task| self.row(task, &theme, cx))
            .collect();
        let count = tasks
            .iter()
            .filter(|task| task.status == ShellTaskStatus::Running)
            .count();
        let empty = self.error.as_deref().unwrap_or(if !self.online {
            "Device offline"
        } else if !self.connected {
            "Connecting..."
        } else {
            "No agent shells running"
        });
        let mut panel = div()
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
                    .when(self.snapshot.truncated, |header| {
                        header.child(div().flex_1()).child("Recent tasks")
                    }),
            )
            .child(
                div()
                    .id("home-shells-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
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
                                .child(empty.to_owned()),
                        )
                    }),
            )
            .when(!tasks.is_empty() && self.error.is_some(), |panel| {
                panel.child(
                    div()
                        .px(px(12.0))
                        .pb(px(6.0))
                        .text_size(crate::typography::ui_rems(10.0))
                        .text_color(theme.text_muted)
                        .child(self.error.clone().unwrap_or_default()),
                )
            });
        if self.selected.is_some() {
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
            let copy = output.is_some_and(|output| !output.text.is_empty());
            panel = panel.child(
                div()
                    .h(px(168.0))
                    .flex_none()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .border_t_1()
                    .border_color(theme.border)
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
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(message.to_owned()),
                            )
                            .when(copy, |header| {
                                header.child(
                                    div()
                                        .id("copy-shell-output")
                                        .size(px(24.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(4.0))
                                        .cursor_pointer()
                                        .role(gpui::Role::Button)
                                        .aria_label("Copy shell output")
                                        .tab_index(if self.interactive { 0 } else { -1 })
                                        .tooltip(text_tooltip("Copy output"))
                                        .hover(|button| button.bg(theme.element_hover))
                                        .focus_visible(|button| button.bg(theme.element_hover))
                                        .on_click(cx.listener(|shells, _, _, cx| {
                                            cx.stop_propagation();
                                            if shells.interactive
                                                && let Some(output) = &shells.output
                                            {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    output.text.clone(),
                                                ));
                                            }
                                        }))
                                        .on_key_down(cx.listener(
                                            |shells, event: &gpui::KeyDownEvent, _, cx| {
                                                if shells.interactive
                                                    && matches!(
                                                        event.keystroke.key.as_str(),
                                                        "enter" | "space"
                                                    )
                                                    && let Some(output) = &shells.output
                                                {
                                                    cx.stop_propagation();
                                                    cx.write_to_clipboard(
                                                        ClipboardItem::new_string(
                                                            output.text.clone(),
                                                        ),
                                                    );
                                                }
                                            },
                                        ))
                                        .child(icon(icons::COPY).size(px(12.0))),
                                )
                            })
                            .child(
                                div()
                                    .id("close-shell-output")
                                    .size(px(24.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(4.0))
                                    .cursor_pointer()
                                    .role(gpui::Role::Button)
                                    .aria_label("Close shell output")
                                    .tab_index(if self.interactive { 0 } else { -1 })
                                    .tooltip(text_tooltip("Close output"))
                                    .hover(|button| button.bg(theme.element_hover))
                                    .focus_visible(|button| button.bg(theme.element_hover))
                                    .on_click(cx.listener(|shells, _, _, cx| {
                                        if shells.interactive {
                                            cx.stop_propagation();
                                            shells.selected = None;
                                            shells.output_task = None;
                                            shells.output = None;
                                            cx.notify();
                                        }
                                    }))
                                    .on_key_down(cx.listener(
                                        |shells, event: &gpui::KeyDownEvent, _, cx| {
                                            if shells.interactive
                                                && matches!(
                                                    event.keystroke.key.as_str(),
                                                    "enter" | "space"
                                                )
                                            {
                                                cx.stop_propagation();
                                                shells.selected = None;
                                                shells.output_task = None;
                                                shells.output = None;
                                                cx.notify();
                                            }
                                        },
                                    ))
                                    .child(icon(icons::CLOSE).size(px(12.0))),
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
                            .whitespace_nowrap()
                            .px(px(10.0))
                            .pb(px(8.0))
                            .font_family(theme.font_mono)
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text)
                            .child(if text.is_empty() {
                                "No output yet".into()
                            } else {
                                text
                            }),
                    ),
            );
        }
        panel
    }
}
