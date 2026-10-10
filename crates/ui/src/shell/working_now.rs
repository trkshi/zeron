//! Home's live thread list. Registry/session watches are the only data source;
//! opening a row uses the same navigation and draft preservation as the sidebar.

use gpui::{
    AvailableSpace, Bounds, Element, GlobalElementId, InspectorElementId, LayoutId, Pixels, point,
    size,
};
use std::collections::HashMap;
use zeron_proto::{Chat, Session, view::project_label};

use super::*;

const HEADER_HEIGHT: f32 = 30.0;
const ROW_HEIGHT: f32 = 44.0;
const PANEL_GAP: f32 = 16.0;
const MAX_PANEL_WIDTH: f32 = 400.0;
const MAX_PANEL_HEIGHT: f32 = HEADER_HEIGHT + 8.0 + ROW_HEIGHT * 4.0;

struct Activity {
    awaiting_input: bool,
    started_at: Option<DateTime<Utc>>,
    subagents: u32,
}

struct WorkingThread {
    chat: Chat,
    activity: Activity,
}

fn activity(
    state: &AppState,
    chat: &Chat,
    session: Option<&Session>,
    now: DateTime<Utc>,
) -> Option<Activity> {
    if !state.device_online(&chat.device_id, now)
        || state.send_queued(&chat.id, now)
        || state.send_undelivered(&chat.id, now)
    {
        return None;
    }
    let live = zeron_proto::view::effective_indicator(session, now);
    let indicator = if state.send_pending(&chat.id, now) {
        Indicator::Working
    } else {
        live
    };
    let subagents = zeron_proto::view::running_subagents(session, now);
    if !matches!(indicator, Indicator::Working | Indicator::AwaitingInput) && subagents == 0 {
        return None;
    }
    Some(Activity {
        awaiting_input: indicator == Indicator::AwaitingInput,
        // Pending sends and workers outliving their parent must not inherit
        // the previous turn's timer from an idle or stale session row.
        started_at: session
            .filter(|_| live == Indicator::Working)
            .and_then(|session| session.started_at),
        subagents,
    })
}

fn threads(state: &AppState, now: DateTime<Utc>) -> Vec<WorkingThread> {
    // Index once rather than scanning the full session registry for every chat.
    let sessions: HashMap<_, _> = state
        .sessions
        .iter()
        .map(|session| (session.chat_id.as_str(), session))
        .collect();
    let mut rows: Vec<_> = state
        .visible_chats()
        .filter_map(|chat| {
            activity(state, chat, sessions.get(chat.id.as_str()).copied(), now).map(|activity| {
                WorkingThread {
                    chat: chat.clone(),
                    activity,
                }
            })
        })
        .collect();
    // Attention first; newest turns next. Never reorder on heartbeat timestamps.
    rows.sort_by(|a, b| {
        b.activity
            .awaiting_input
            .cmp(&a.activity.awaiting_input)
            .then_with(|| b.activity.started_at.cmp(&a.activity.started_at))
            .then_with(|| a.chat.id.cmp(&b.chat.id))
    });
    rows
}

pub(super) fn has_running_clock(state: &AppState, now: DateTime<Utc>) -> bool {
    state.sessions.iter().any(|session| {
        session.started_at.is_some()
            && zeron_proto::view::effective_indicator(Some(session), now) == Indicator::Working
            && state
                .visible_chats()
                .find(|chat| chat.id == session.chat_id)
                .and_then(|chat| activity(state, chat, Some(session), now))
                .is_some_and(|activity| activity.started_at.is_some())
    })
}

fn elapsed(started: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = now.signed_duration_since(started).num_seconds().max(0);
    if seconds >= 3600 {
        format!("{}h {}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

fn project_and_branch(state: &AppState, chat: &Chat) -> String {
    let project = state
        .space_for_chat(chat)
        .map(|space| state.representative_space(space).display_name().to_owned())
        .unwrap_or_else(|| project_label(chat.cwd.as_deref()));
    let branch = chat
        .source_context
        .as_ref()
        .map(|source| source.branch.as_str())
        .or(chat.branch.as_deref())
        .filter(|branch| !branch.trim().is_empty());
    match branch {
        Some(branch) => format!("{project} \u{00b7} {branch}"),
        None => project,
    }
}

impl Shell {
    pub(super) fn render_working_now(
        &self,
        width: f32,
        bottom_clearance: f32,
        opacity: f32,
        terminal: crate::terminal::dock::SharedGeometry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let now = Utc::now();
        let (threads, empty_text) = {
            let state = self.state.read(cx);
            (
                threads(state, now),
                if !state.chats_synced {
                    "Loading threads..."
                } else {
                    "No threads running"
                },
            )
        };
        let count = threads.len();
        let theme = Theme::of(cx).clone();
        let panel_width = (width - 2.0 * Theme::SPACE_LG).clamp(0.0, MAX_PANEL_WIDTH);
        let rows: Vec<_> = threads
            .iter()
            .map(|row| {
                self.render_working_thread(row, panel_width, now, opacity >= 0.95, &theme, cx)
            })
            .collect();
        let desired_height = HEADER_HEIGHT + 8.0 + ROW_HEIGHT * count.max(1) as f32;
        let panel = div()
            .id("home-working-now")
            .debug_selector(|| "home-working-now".into())
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .bg(crate::theme::flatten(theme.wash(0.02), theme.surface))
            .overflow_hidden()
            .opacity(opacity)
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
                    .child(
                        icon(icons::SPEEDOMETER)
                            .size(px(12.0))
                            .text_color(theme.text_muted),
                    )
                    .child("Working now")
                    .child(count.to_string()),
            )
            .child(
                div()
                    .id("home-working-now-list")
                    .debug_selector(|| "home-working-now-list".into())
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(4.0))
                    .pb(px(4.0))
                    .children(rows)
                    .when(count == 0, |list| {
                        list.child(
                            div()
                                .h(px(ROW_HEIGHT))
                                .px(px(8.0))
                                .flex()
                                .items_center()
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(theme.text_muted)
                                .child(empty_text),
                        )
                    }),
            )
            // Home controls reveal alongside the dock's selectors. Faded rows
            // must not accept clicks or keyboard focus during that handoff.
            .when(opacity < 0.95, |panel| {
                panel.child(div().absolute().inset_0().occlude())
            });
        div()
            .absolute()
            .inset_0()
            .child(BelowComposer {
                child: Some(panel.into_any_element()),
                desired_height,
                viewport_height: self.viewport_height,
                bottom_clearance,
                terminal,
                surface: self.composer.read(cx).surface_bounds(),
            })
            .into_any_element()
    }

    fn render_working_thread(
        &self,
        row: &WorkingThread,
        width: f32,
        now: DateTime<Utc>,
        interactive: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self.state.read(cx);
        let chat = &row.chat;
        let id = chat.id.clone();
        let keyboard_id = id.clone();
        let title = chat
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or("New thread");
        let project = project_and_branch(state, chat);
        let device = state
            .device_name(&chat.device_id)
            .unwrap_or("Unknown device");
        let config = chat.config.as_ref();
        let model = config.map(|config| {
            let pickers = self.composer.read(cx).pickers().read(cx);
            let mut name = config
                .model
                .as_deref()
                .map(|id| pickers.saved_model_label(config.harness, id))
                .unwrap_or_else(|| "Automatic".into());
            if let Some(reasoning) = config.reasoning {
                name.push(' ');
                name.push_str(crate::pickers::reasoning_label(reasoning));
            }
            (config.harness, name)
        });
        let status = if row.activity.awaiting_input {
            "Needs input".to_owned()
        } else if let Some(started) = row.activity.started_at {
            elapsed(started, now)
        } else if row.activity.subagents > 0 {
            format!("{} running", row.activity.subagents)
        } else {
            "Working".to_owned()
        };
        let tooltip = format!(
            "{title}\n{project}\n{device}\n{} / {status}{}",
            model
                .as_ref()
                .map(|(_, name)| name.as_str())
                .unwrap_or("Session agent"),
            if row.activity.subagents > 0 {
                format!(" / {} subagents", row.activity.subagents)
            } else {
                String::new()
            },
        );
        let color = if row.activity.awaiting_input {
            theme.warning
        } else {
            theme.text_muted
        };
        let marker = if row.activity.awaiting_input {
            icon(icons::CHAT_ROUND_LINE)
                .size(px(12.0))
                .text_color(theme.warning)
                .into_any_element()
        } else {
            loaders::mini_glyph_spinner(
                format!("home-working-now-activity-{id}"),
                2.0,
                theme.glyph,
                cx.entity_id(),
                cx,
            )
            .into_any_element()
        };
        let model = model.map(|(harness, model)| {
            let (path, tint) = crate::pickers::harness_brand_icon(harness);
            div()
                .flex()
                .items_center()
                .gap(px(5.0))
                .flex_none()
                .min_w_0()
                .max_w(px((width * 0.44).max(0.0)))
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_muted)
                .child(
                    icon(path)
                        .size(px(12.0))
                        .flex_none()
                        .text_color(tint.unwrap_or(theme.text_muted)),
                )
                .child(div().min_w_0().truncate().child(model))
                .into_any_element()
        });
        let compact = width < 320.0;
        let (top_model, bottom_model) = if compact {
            (None, model)
        } else {
            (model, None)
        };
        div()
            .id(SharedString::from(format!("home-working-now-thread-{id}")))
            .debug_selector({
                let id = id.clone();
                move || format!("home-working-now-thread-{id}")
            })
            .h(px(ROW_HEIGHT))
            .w_full()
            .px(px(7.0))
            .py(px(4.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(gpui::transparent_black())
            .flex()
            .flex_col()
            .justify_center()
            .gap(px(3.0))
            .cursor_pointer()
            .hover(|row| row.bg(theme.element_hover))
            .focus_visible(|row| row.border_color(theme.accent))
            .role(gpui::Role::Button)
            .aria_label(tooltip.clone())
            .tab_index(if interactive { 0 } else { -1 })
            .tooltip(crate::settings::widgets::text_tooltip(tooltip))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.open_chat(id.clone(), cx);
            }))
            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    cx.stop_propagation();
                    this.open_chat(keyboard_id.clone(), cx);
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .min_w_0()
                    .child(div().w(px(12.0)).flex_none().child(marker))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text)
                            .child(title.to_owned()),
                    )
                    .children(top_model)
                    .child(
                        div()
                            .flex_none()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(color)
                            .child(status),
                    ),
            )
            .child(
                div()
                    .pl(px(20.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .min_w_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_muted)
                            .child(project),
                    )
                    .children(bottom_model),
            )
            .into_any_element()
    }
}

fn panel_bounds(
    composer: Bounds<Pixels>,
    surface: Option<Bounds<Pixels>>,
    viewport_height: f32,
    bottom_clearance: f32,
    desired_height: f32,
) -> Option<Bounds<Pixels>> {
    let top = f32::from(composer.bottom()) + PANEL_GAP;
    let available = viewport_height - bottom_clearance - top;
    let height = available.min(desired_height).min(MAX_PANEL_HEIGHT);
    let (left, width) = match surface {
        Some(surface) => (surface.left(), f32::from(surface.size.width)),
        None => (
            composer.left() + px(Theme::SPACE_LG),
            f32::from(composer.size.width) - 2.0 * Theme::SPACE_LG,
        ),
    };
    let width = width.clamp(0.0, MAX_PANEL_WIDTH);
    // A short window prioritizes the composer over a clipped header-only card.
    (height >= HEADER_HEIGHT + ROW_HEIGHT && width > 0.0)
        .then(|| Bounds::new(point(left, px(top)), size(px(width), px(height))))
}

/// Lay out against the composer's actual prepaint bounds, including its dock
/// motion and multiline height. The card aligns with the input surface, not
/// its outer gutters, but stays below the footer. Only the thread list scrolls.
struct BelowComposer {
    child: Option<AnyElement>,
    desired_height: f32,
    viewport_height: f32,
    bottom_clearance: f32,
    terminal: crate::terminal::dock::SharedGeometry,
    surface: crate::new_thread_background_mask::SurfaceBounds,
}

impl IntoElement for BelowComposer {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for BelowComposer {
    type RequestLayoutState = ();
    type PrepaintState = Option<AnyElement>;
    fn id(&self) -> Option<gpui::ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (
            div()
                .size_full()
                .into_any_element()
                .request_layout(window, cx),
            (),
        )
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        composer: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let bounds = panel_bounds(
            composer,
            self.surface.get(),
            self.viewport_height,
            self.bottom_clearance + self.terminal.get().height,
            self.desired_height,
        )?;
        let mut child = div()
            .w(bounds.size.width)
            .h(bounds.size.height)
            .child(self.child.take()?)
            .into_any_element();
        child.prepaint_as_root(
            bounds.origin,
            bounds.size.map(AvailableSpace::Definite),
            window,
            cx,
        );
        Some(child)
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        child: &mut Option<AnyElement>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(child) = child {
            child.paint(window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::SessionStatus;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn chat(id: &str) -> Chat {
        Chat {
            id: id.into(),
            device_id: "host".into(),
            title: Some(format!("Thread {id}")),
            archived: false,
            cwd: Some("/work/project".into()),
            branch: Some("feature".into()),
            checkout_id: None,
            source_context: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
            room_gen: None,
            parent_chat_id: None,
        }
    }

    fn session(id: &str, status: SessionStatus) -> Session {
        Session {
            last_completed_turn: None,
            chat_id: id.into(),
            device_id: "host".into(),
            status,
            started_at: Some(now() - chrono::TimeDelta::seconds(90)),
            updated_at: now(),
            running_subagents: 0,
        }
    }

    #[test]
    fn only_live_top_level_threads_appear_across_projects_and_devices() {
        let mut state = AppState::new();
        state.selected_device = Some("different-device".into());
        state.chats = [
            "working", "question", "idle", "failed", "stale", "archived", "child",
        ]
        .into_iter()
        .map(chat)
        .collect();
        state.chats[5].archived = true;
        state.chats[6].parent_chat_id = Some("working".into());
        state.sessions = vec![
            session("working", SessionStatus::Working),
            session("question", SessionStatus::AwaitingInput),
            session("idle", SessionStatus::Idle),
            session("failed", SessionStatus::Errored),
            session("stale", SessionStatus::Working),
            session("archived", SessionStatus::Working),
            session("child", SessionStatus::Working),
            session("missing-chat", SessionStatus::Working),
        ];
        state.sessions[4].updated_at = now() - chrono::TimeDelta::seconds(46);
        let rows = threads(&state, now());
        assert_eq!(
            rows.iter()
                .map(|row| row.chat.id.as_str())
                .collect::<Vec<_>>(),
            ["question", "working"]
        );
        assert!(rows[0].activity.awaiting_input);
        assert!(rows[0].activity.started_at.is_none());
        assert!(has_running_clock(&state, now()));
        assert!(threads(&state, now() + chrono::TimeDelta::seconds(46)).is_empty());
        assert!(!has_running_clock(
            &state,
            now() + chrono::TimeDelta::seconds(46)
        ));
    }

    #[test]
    fn workers_stay_on_the_parent_without_reusing_its_previous_timer() {
        let mut state = AppState::new();
        state.chats = vec![chat("parent")];
        let mut parent = session("parent", SessionStatus::Idle);
        parent.running_subagents = 2;
        state.sessions = vec![parent];
        let rows = threads(&state, now());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].activity.subagents, 2);
        assert!(rows[0].activity.started_at.is_none());
        assert!(!has_running_clock(&state, now()));
        assert!(threads(&state, now() + chrono::TimeDelta::seconds(46)).is_empty());
    }

    #[test]
    fn missing_start_time_and_waiting_questions_do_not_tick_every_second() {
        let mut state = AppState::new();
        state.chats = vec![chat("thread")];
        state.sessions = vec![session("thread", SessionStatus::Working)];
        state.sessions[0].started_at = None;
        assert_eq!(threads(&state, now()).len(), 1);
        assert!(!has_running_clock(&state, now()));
        state.sessions[0] = session("thread", SessionStatus::AwaitingInput);
        assert!(!has_running_clock(&state, now()));
    }

    #[test]
    fn offline_hosts_and_queued_sends_are_not_reported_as_working() {
        let mut state = AppState::new();
        state.chats = vec![chat("thread")];
        state.sessions = vec![session("thread", SessionStatus::Working)];
        state.devices = vec![zeron_proto::Device {
            id: "host".into(),
            name: "Ubuntu".into(),
            platform: "linux".into(),
            last_seen_at: Some(now() - chrono::TimeDelta::hours(1)),
            created_at: None,
            version: None,
            cursor_sdk_version: None,
            capabilities: vec![],
        }];
        assert!(threads(&state, now()).is_empty());
        state.devices[0].last_seen_at = Some(now());
        state.begin_pending_send("thread", "message", now());
        state.connectivity.state = zeron_proto::ConnectivityState::Reconnecting;
        assert!(threads(&state, now()).is_empty());
        state.connectivity.state = zeron_proto::ConnectivityState::Disabled;
        assert_eq!(threads(&state, now()).len(), 1);
        state.sessions[0].status = SessionStatus::Idle;
        assert!(threads(&state, now())[0].activity.started_at.is_none());
    }

    #[test]
    fn row_order_does_not_follow_heartbeats() {
        let mut state = AppState::new();
        state.chats = vec![chat("older"), chat("newer"), chat("tied")];
        state.sessions = state
            .chats
            .iter()
            .map(|chat| session(&chat.id, SessionStatus::Working))
            .collect();
        state.sessions[0].started_at = Some(now() - chrono::TimeDelta::minutes(5));
        state.sessions[1].updated_at = now() - chrono::TimeDelta::seconds(10);
        assert_eq!(
            threads(&state, now())
                .iter()
                .map(|row| row.chat.id.as_str())
                .collect::<Vec<_>>(),
            ["newer", "tied", "older"]
        );
        state.sessions[1].updated_at = now();
        assert_eq!(
            threads(&state, now())
                .iter()
                .map(|row| row.chat.id.as_str())
                .collect::<Vec<_>>(),
            ["newer", "tied", "older"]
        );
    }

    #[test]
    fn elapsed_uses_turn_start_and_clamps_clock_skew() {
        for (seconds, expected) in [
            (0, "0s"),
            (19, "19s"),
            (288, "4m 48s"),
            (3661, "1h 1m"),
            (-10, "0s"),
        ] {
            assert_eq!(
                elapsed(now() - chrono::TimeDelta::seconds(seconds), now()),
                expected
            );
        }
    }

    #[test]
    fn row_branch_uses_the_conversations_run_context_not_the_live_checkout() {
        let state = AppState::new();
        let mut chat = chat("thread");
        assert_eq!(
            project_and_branch(&state, &chat),
            "project \u{00b7} feature"
        );
        chat.source_context = Some(zeron_proto::ConversationSourceContext {
            checkout_id: "checkout".into(),
            repo_root: "/work/project".into(),
            cwd: "/work/project".into(),
            branch: "original-branch".into(),
            head_sha: None,
            observed_at: now(),
        });
        assert_eq!(
            project_and_branch(&state, &chat),
            "project \u{00b7} original-branch"
        );
        chat.source_context = None;
        chat.branch = None;
        chat.cwd = None;
        assert_eq!(project_and_branch(&state, &chat), "No project");
    }

    #[test]
    fn panel_follows_measured_composer_and_keeps_clear_of_bottom_chrome() {
        let composer = Bounds::new(point(px(200.0), px(250.0)), size(px(600.0), px(180.0)));
        let surface = Bounds::new(point(px(224.0), px(250.0)), size(px(552.0), px(140.0)));
        let desired_height = HEADER_HEIGHT + 8.0 + ROW_HEIGHT * 2.0;
        let panel = panel_bounds(composer, Some(surface), 800.0, 24.0, desired_height).unwrap();
        assert_eq!(panel.left(), surface.left());
        assert_eq!(panel.top(), composer.bottom() + px(PANEL_GAP));
        assert_eq!(panel.size.width, px(MAX_PANEL_WIDTH));
        assert_eq!(panel.size.height, px(desired_height));
        let panel = panel_bounds(composer, Some(surface), 800.0, 240.0, 1_000.0).unwrap();
        assert_eq!(panel.bottom(), px(560.0));
        assert!(panel_bounds(composer, Some(surface), 550.0, 80.0, desired_height).is_none());
        let panel = panel_bounds(composer, Some(surface), 1_200.0, 24.0, 1_000.0).unwrap();
        assert_eq!(panel.size.height, px(MAX_PANEL_HEIGHT));
    }

    #[test]
    fn panel_shrinks_to_the_surface_and_fallback_respects_composer_gutters() {
        let composer = Bounds::new(point(px(20.0), px(100.0)), size(px(300.0), px(140.0)));
        let surface = Bounds::new(point(px(44.0), px(100.0)), size(px(252.0), px(100.0)));
        let desired_height = HEADER_HEIGHT + 8.0 + ROW_HEIGHT;
        let panel = panel_bounds(composer, Some(surface), 800.0, 24.0, desired_height).unwrap();
        assert_eq!(panel.left(), surface.left());
        assert_eq!(panel.size.width, surface.size.width);
        let panel = panel_bounds(composer, None, 800.0, 24.0, desired_height).unwrap();
        assert_eq!(panel.left(), composer.left() + px(Theme::SPACE_LG));
        assert_eq!(
            panel.size.width,
            composer.size.width - px(2.0 * Theme::SPACE_LG)
        );
        let empty = Bounds::new(surface.origin, size(px(0.0), surface.size.height));
        assert!(panel_bounds(composer, Some(empty), 800.0, 24.0, desired_height).is_none());
    }

    type Measurement = std::rc::Rc<std::cell::Cell<Option<Bounds<Pixels>>>>;

    struct LayoutProbe {
        dock: crate::composer_dock::SharedDock,
        composer_height: f32,
        composer: Measurement,
        surface: Measurement,
        panel: Measurement,
        clicks: usize,
    }

    impl Render for LayoutProbe {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.panel.set(None);
            let viewport = f32::from(window.viewport_size().height);
            let now = std::time::Instant::now();
            self.dock.borrow_mut().tick(false, true, now);
            let composer_measurement = self.composer.clone();
            let surface_measurement = self.surface.clone();
            let panel_measurement = self.panel.clone();
            let panel = div()
                .id("probe-panel")
                .relative()
                .size_full()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.clicks += 1;
                    cx.notify();
                }))
                .child(
                    gpui::canvas(
                        move |bounds, _, _| panel_measurement.set(Some(bounds)),
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .inset_0(),
                );
            let composer = div()
                .w(px(600.0))
                .h(px(self.composer_height))
                .mx_auto()
                .relative()
                .child(
                    gpui::canvas(
                        move |bounds, _, _| composer_measurement.set(Some(bounds)),
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .inset_0(),
                )
                .child(
                    div().absolute().inset_0().px(px(Theme::SPACE_LG)).child(
                        gpui::canvas(
                            move |bounds, _, _| surface_measurement.set(Some(bounds)),
                            |_, _, _, _| {},
                        )
                        .size_full(),
                    ),
                )
                .child(div().absolute().inset_0().child(BelowComposer {
                    child: Some(panel.into_any_element()),
                    desired_height: HEADER_HEIGHT + 8.0 + ROW_HEIGHT * 2.0,
                    viewport_height: viewport,
                    bottom_clearance: 24.0,
                    terminal: Default::default(),
                    surface: self.surface.clone(),
                }));
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0())
                .child(crate::composer_dock::docked_composer(
                    composer,
                    self.dock.clone(),
                    viewport,
                    true,
                    now,
                ))
        }
    }

    #[gpui::test]
    fn panel_hitboxes_follow_dock_geometry_without_moving_or_covering_the_input(
        cx: &mut gpui::TestAppContext,
    ) {
        let composer: Measurement = Default::default();
        let surface: Measurement = Default::default();
        let panel: Measurement = Default::default();
        let (probe, cx) = cx.add_window_view(|_, _| LayoutProbe {
            dock: Default::default(),
            composer_height: 180.0,
            composer: composer.clone(),
            surface: surface.clone(),
            panel: panel.clone(),
            clicks: 0,
        });
        cx.refresh();
        let input = composer.get().unwrap();
        let card = panel.get().unwrap();
        assert_eq!(card.top(), input.bottom() + px(PANEL_GAP));
        assert_eq!(card.left(), surface.get().unwrap().left());
        assert_eq!(card.size.width, px(MAX_PANEL_WIDTH));
        cx.simulate_click(card.center(), gpui::Modifiers::default());
        assert_eq!(probe.read_with(cx, |probe, _| probe.clicks), 1);
        cx.simulate_click(
            point(card.right() + px(16.0), card.center().y),
            gpui::Modifiers::default(),
        );
        assert_eq!(probe.read_with(cx, |probe, _| probe.clicks), 1);
        cx.simulate_click(input.center(), gpui::Modifiers::default());
        assert_eq!(probe.read_with(cx, |probe, _| probe.clicks), 1);
        probe.update(cx, |probe, cx| {
            probe.composer_height = 260.0;
            cx.notify();
        });
        cx.refresh();
        assert_eq!(
            panel.get().unwrap().top(),
            composer.get().unwrap().bottom() + px(PANEL_GAP)
        );
        let viewport = cx.update(|window, _| f32::from(window.viewport_size().height));
        probe.update(cx, |probe, cx| {
            probe.composer_height = viewport;
            cx.notify();
        });
        cx.refresh();
        assert!(
            panel.get().is_none(),
            "too little room: no hidden card hitboxes"
        );
        cx.simulate_click(card.center(), gpui::Modifiers::default());
        assert_eq!(probe.read_with(cx, |probe, _| probe.clicks), 1);
    }
}
