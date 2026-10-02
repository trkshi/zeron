//! Nonblocking questions have their own editor and durable delivery lifecycle.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui::{Context, Entity, Role, SharedString, Subscription, Task, Window, div, prelude::*, px};
use zeron_doc::{
    MessagePart, MessageRole, SessionCommandEntry, SessionCommandPayload, SessionCommandStatus,
    SessionMessageEntry,
};
use zeron_proto::{UserInputQuestion, capabilities};
use zeron_rpc::methods;

use super::{ComposerInput, ComposerInputEvent, Wizard, WizardStep};
use crate::state::AppState;
use crate::theme::Theme;

type RequestKey = (String, String);

fn pending_requests(entries: &[SessionMessageEntry]) -> Vec<(String, Vec<UserInputQuestion>)> {
    let mut seen = HashSet::new();
    entries
        .iter()
        .filter(|entry| entry.role == MessageRole::Assistant)
        .flat_map(|entry| &entry.parts)
        .filter_map(|part| match part {
            MessagePart::Input {
                request_id,
                questions,
                asynchronous: true,
                resolved: false,
                ..
            } if !questions.is_empty() && seen.insert(request_id.clone()) => {
                Some((request_id.clone(), questions.clone()))
            }
            _ => None,
        })
        .collect()
}

struct QuestionDraft {
    wizard: Wizard,
    command_id: Option<String>,
    sending: bool,
    error: Option<String>,
}

impl QuestionDraft {
    fn locked(&self) -> bool {
        self.sending || self.command_id.is_some()
    }

    fn can_advance(&self) -> bool {
        self.wizard.page_has_pick()
            || self
                .wizard
                .typed
                .get(self.wizard.page)
                .is_some_and(|text| !text.trim().is_empty())
    }
}

pub(super) struct AsyncQuestionPanel {
    state: Entity<AppState>,
    input: Entity<ComposerInput>,
    drafts: HashMap<RequestKey, QuestionDraft>,
    order: Vec<RequestKey>,
    accepted: HashSet<RequestKey>,
    current: Option<RequestKey>,
    pending_count: usize,
    tasks: HashMap<RequestKey, Task<()>>,
    _observe: Subscription,
    _input_events: Subscription,
}

impl AsyncQuestionPanel {
    pub(super) fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            let mut input = ComposerInput::new("Custom answer", cx);
            input.viewport_height = Some(80.0);
            input.settled_viewport_height = Some(80.0);
            input
        });
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.sync(cx));
        let input_events = cx.subscribe(&input, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted | ComposerInputEvent::ModifiedSubmitted => {
                this.advance(cx)
            }
            ComposerInputEvent::Edited => {
                this.save_typed(cx);
                cx.notify();
            }
            ComposerInputEvent::ViewportChanged => cx.notify(),
            _ => {}
        });
        let mut panel = Self {
            state,
            input,
            drafts: HashMap::new(),
            order: Vec::new(),
            accepted: HashSet::new(),
            current: None,
            pending_count: 0,
            tasks: HashMap::new(),
            _observe: observe,
            _input_events: input_events,
        };
        panel.sync(cx);
        panel
    }

    pub(super) fn is_visible(&self) -> bool {
        self.current.is_some()
    }

    fn save_typed(&mut self, cx: &Context<Self>) {
        if let Some(draft) = self
            .current
            .as_ref()
            .and_then(|key| self.drafts.get_mut(key))
        {
            if !draft.locked() {
                draft
                    .wizard
                    .set_typed(self.input.read(cx).text().to_string());
            }
        }
    }

    fn load_page(&mut self, cx: &mut Context<Self>) {
        let draft = self.current.as_ref().and_then(|key| self.drafts.get(key));
        let text = draft
            .and_then(|draft| draft.wizard.typed.get(draft.wizard.page))
            .cloned()
            .unwrap_or_default();
        let read_only = draft.is_some_and(QuestionDraft::locked);
        self.input.update(cx, |input, cx| {
            input.set_text(text, cx);
            input.read_only = read_only;
            cx.notify();
        });
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let (chat_id, pending, resolved) = {
            let state = self.state.read(cx);
            let resolved: HashSet<_> = state
                .transcript
                .iter()
                .flat_map(|entry| &entry.parts)
                .filter_map(|part| match part {
                    MessagePart::Input {
                        request_id,
                        resolved: true,
                        ..
                    } => Some(request_id.clone()),
                    _ => None,
                })
                .collect();
            (
                state.selected_chat.clone(),
                pending_requests(&state.transcript),
                resolved,
            )
        };
        self.save_typed(cx);
        if let Some(chat_id) = &chat_id {
            self.drafts
                .retain(|(chat, request), _| chat != chat_id || !resolved.contains(request));
            self.accepted
                .retain(|(chat, request)| chat != chat_id || !resolved.contains(request));
            self.order
                .retain(|(chat, request)| chat != chat_id || !resolved.contains(request));
            self.tasks
                .retain(|(chat, request), _| chat != chat_id || !resolved.contains(request));
            for (request_id, questions) in pending {
                let key = (chat_id.clone(), request_id.clone());
                if !self.drafts.contains_key(&key)
                    && !self.accepted.contains(&key)
                    && !resolved.contains(&request_id)
                {
                    self.order.push(key.clone());
                    self.drafts.insert(
                        key,
                        QuestionDraft {
                            wizard: Wizard::new(request_id, questions),
                            command_id: None,
                            sending: false,
                            error: None,
                        },
                    );
                }
            }
        }
        // Keep loaded questions through transient sync gaps and newer messages.
        // Drafts and delivery tasks belong to a chat/request, not the visible page.
        let pending: Vec<_> = self
            .order
            .iter()
            .filter(|key| Some(&key.0) == chat_id.as_ref() && !self.accepted.contains(*key))
            .cloned()
            .collect();
        self.pending_count = pending.len();
        let next = pending.into_iter().next();
        if self.current != next {
            self.current = next;
            self.load_page(cx);
        }
        cx.notify();
    }

    fn capability_error(&self, cx: &gpui::App) -> Option<&'static str> {
        let state = self.state.read(cx);
        let Some(engine) = state.engine() else {
            return Some("Engine not connected.");
        };
        if !engine
            .engine_info()
            .supports(capabilities::ASYNC_QUESTIONS_V1)
        {
            return Some("Update the local engine to answer here.");
        }
        if let Some((chat, _)) = &self.current
            && !state.chat_host_supports(chat, capabilities::ASYNC_QUESTIONS_V1)
        {
            return Some("Update the chat host to answer here.");
        }
        None
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(draft) = self
            .current
            .as_ref()
            .and_then(|key| self.drafts.get_mut(key))
            && !draft.locked()
        {
            // Async choices never schedule the blocking wizard's auto-advance.
            let _ = draft.wizard.select(index);
            draft.wizard.set_typed(String::new());
            draft.error = None;
            self.input.update(cx, |input, cx| input.set_text("", cx));
            cx.notify();
        }
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        self.save_typed(cx);
        if let Some(draft) = self
            .current
            .as_ref()
            .and_then(|key| self.drafts.get_mut(key))
            && !draft.locked()
            && draft.wizard.back()
        {
            self.load_page(cx);
            cx.notify();
        }
    }

    fn advance(&mut self, cx: &mut Context<Self>) {
        self.save_typed(cx);
        let Some(draft) = self
            .current
            .as_ref()
            .and_then(|key| self.drafts.get_mut(key))
        else {
            return;
        };
        if draft.sending || !draft.can_advance() {
            return;
        }
        if draft.command_id.is_some() {
            self.submit(cx);
            return;
        }
        match draft.wizard.advance() {
            WizardStep::Done(_) => self.submit(cx),
            _ => self.load_page(cx),
        }
        cx.notify();
    }

    fn fail(&mut self, key: &RequestKey, error: String, terminal: bool, cx: &mut Context<Self>) {
        if let Some(draft) = self.drafts.get_mut(key) {
            draft.sending = false;
            draft.error = Some(error);
            if terminal {
                draft.command_id = None;
            }
        }
        if self.current.as_ref() == Some(key) {
            let read_only = self.drafts.get(key).is_some_and(QuestionDraft::locked);
            self.input.update(cx, |input, cx| {
                input.read_only = read_only;
                cx.notify();
            });
        }
        cx.notify();
    }

    fn acknowledge(&mut self, key: RequestKey, cx: &mut Context<Self>) {
        self.accepted.insert(key);
        self.sync(cx);
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.current.clone() else {
            return;
        };
        if let Some(error) = self.capability_error(cx) {
            self.fail(&key, error.into(), false, cx);
            return;
        }
        let (engine, host) = {
            let state = self.state.read(cx);
            let (Some(engine), Some(chat)) = (state.engine(), state.selected_chat_row()) else {
                return;
            };
            (engine.clone(), chat.device_id.clone())
        };
        let Some(draft) = self.drafts.get_mut(&key) else {
            return;
        };
        if draft.sending {
            return;
        }
        let answers = draft.wizard.answers();
        if answers.iter().any(|answer| {
            answer.labels.is_empty() || answer.labels.iter().any(|label| label.trim().is_empty())
        }) {
            self.fail(
                &key,
                "Answer every question before submitting.".into(),
                false,
                cx,
            );
            return;
        }
        let existing_command = draft.command_id.clone();
        draft.sending = true;
        draft.error = None;
        self.input.update(cx, |input, cx| {
            input.read_only = true;
            cx.notify();
        });
        let task_key = key.clone();
        let task = cx.spawn(async move |this, cx| {
            let (chat_id, request_id) = &task_key;
            let command_id = if let Some(command_id) = existing_command {
                // A retry nudges the existing durable command, not a second answer.
                let result = crate::attachments::call_with_timeout(
                    &engine, cx.background_executor(), methods::RETRY_DELIVERY,
                    serde_json::json!({"chatId": chat_id}), Duration::from_secs(15),
                ).await;
                if let Err(error) = result {
                    this.update(cx, |panel, cx| panel.fail(&task_key, error, false, cx)).ok();
                    return;
                }
                command_id
            } else {
                let command = SessionCommandPayload::RespondInput { request_id: request_id.clone(), answers };
                let result = crate::attachments::call_with_timeout(
                    &engine, cx.background_executor(), methods::QUEUE_COMMAND,
                    serde_json::json!({"chatId": chat_id, "command": command}), Duration::from_secs(15),
                ).await;
                let command_id = match result {
                    Ok(reply) => match reply.get("commandId").and_then(serde_json::Value::as_str) {
                        Some(id) => id.to_string(),
                        None => {
                            this.update(cx, |panel, cx| panel.fail(&task_key, "Engine did not return an answer command id.".into(), false, cx)).ok();
                            return;
                        }
                    },
                    Err(error) => {
                        this.update(cx, |panel, cx| panel.fail(&task_key, error, false, cx)).ok();
                        return;
                    }
                };
                let retained = this.update(cx, |panel, cx| {
                    let Some(draft) = panel.drafts.get_mut(&task_key) else { return false; };
                    draft.command_id = Some(command_id.clone());
                    cx.notify();
                    true
                }).unwrap_or(false);
                if !retained { return; }
                command_id
            };
            let deadline = Instant::now() + Duration::from_secs(90);
            loop {
                let result = crate::attachments::call_with_timeout(
                    &engine, cx.background_executor(), methods::GET_SESSION_COMMAND,
                    serde_json::json!({"chatId": chat_id, "commandId": command_id, "targetDeviceId": host}),
                    Duration::from_secs(15),
                ).await.and_then(|value| serde_json::from_value::<Option<SessionCommandEntry>>(value).map_err(|error| error.to_string()));
                match result {
                    Ok(Some(command)) if command.status == SessionCommandStatus::Applied => {
                        this.update(cx, |panel, cx| {
                            panel.acknowledge(task_key.clone(), cx);
                        }).ok();
                        return;
                    }
                    Ok(Some(command)) if command.status != SessionCommandStatus::Pending => {
                        let error = command.resolution.unwrap_or_else(|| match command.status {
                            SessionCommandStatus::Expired => "Answer expired before delivery. Submit again.".into(),
                            SessionCommandStatus::Cancelled => "Answer delivery was cancelled. Submit again.".into(),
                            _ => "The host rejected the answer. Submit again.".into(),
                        });
                        this.update(cx, |panel, cx| panel.fail(&task_key, error, true, cx)).ok();
                        return;
                    }
                    Err(error) => {
                        this.update(cx, |panel, cx| panel.fail(&task_key, format!("Could not confirm delivery: {error}"), false, cx)).ok();
                        return;
                    }
                    _ => {}
                }
                if Instant::now() >= deadline {
                    this.update(cx, |panel, cx| panel.fail(&task_key, "Answer is still queued. Retry delivery.".into(), false, cx)).ok();
                    return;
                }
                let retained = this.update(cx, |panel, _| panel.drafts.contains_key(&task_key)).unwrap_or(false);
                if !retained { return; }
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
        });
        self.tasks.insert(key, task);
        cx.notify();
    }
}

impl Render for AsyncQuestionPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(draft) = self.current.as_ref().and_then(|key| self.drafts.get(key)) else {
            return gpui::Empty.into_any_element();
        };
        let Some(question) = draft.wizard.current() else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        let accent = theme.accent;
        let locked = draft.locked();
        let sending = draft.sending;
        let last = draft.wizard.page + 1 == draft.wizard.questions.len();
        let can_submit = !sending && draft.can_advance() && self.capability_error(cx).is_none();
        let error = draft
            .error
            .clone()
            .or_else(|| self.capability_error(cx).map(str::to_owned));
        let label = if sending {
            "Sending..."
        } else if draft.error.is_some() {
            "Retry"
        } else if last {
            "Submit answer"
        } else {
            "Next"
        };
        let typed_empty = self.input.read(cx).text().trim().is_empty();
        let options: Vec<_> = question
            .options
            .iter()
            .enumerate()
            .map(|(index, label)| {
                let picked = typed_empty && draft.wizard.is_picked(index);
                div()
                    .id(("async-question-option", index))
                    .role(Role::RadioButton)
                    .aria_label(label.clone())
                    .aria_selected(picked)
                    .tab_index(if locked { -1 } else { 0 })
                    .flex()
                    .items_start()
                    .gap(px(10.0))
                    .px(px(10.0))
                    .py(px(8.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(if picked {
                        accent
                    } else {
                        gpui::transparent_black()
                    })
                    .bg(if picked {
                        crate::theme::ink(0.06)
                    } else {
                        gpui::transparent_black()
                    })
                    .focus_visible(move |style| style.border_color(accent))
                    .when(!locked, |el| {
                        el.cursor_pointer()
                            .hover(|style| style.bg(crate::theme::ink(0.06)))
                            .on_click(cx.listener(move |panel, _, _, cx| panel.select(index, cx)))
                            .on_key_down(cx.listener(
                                move |panel, event: &gpui::KeyDownEvent, _, cx| {
                                    if !event.keystroke.modifiers.modified()
                                        && matches!(event.keystroke.key.as_str(), "enter" | "space")
                                    {
                                        panel.select(index, cx);
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                    })
                    .child(
                        div()
                            .flex_none()
                            .mt(px(2.0))
                            .size(px(14.0))
                            .rounded_full()
                            .border_1()
                            .border_color(if picked { accent } else { theme.text_muted })
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(picked, |el| {
                                el.child(div().size(px(6.0)).rounded_full().bg(accent))
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(crate::typography::ui_rems(13.0))
                            .line_height(px(18.0))
                            .text_color(theme.text)
                            .child(SharedString::from(label.clone())),
                    )
            })
            .collect();
        div()
            .id("async-question-panel")
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .child(
                div()
                    .id("async-question-body")
                    .max_h(px(250.0))
                    .overflow_y_scroll()
                    .p(px(12.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(crate::typography::ui_rems(14.0))
                                    .line_height(px(20.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(SharedString::from(question.question.clone())),
                            )
                            .when(draft.wizard.questions.len() > 1, |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_size(crate::typography::ui_rems(12.0))
                                        .text_color(theme.text_muted)
                                        .child(SharedString::from(draft.wizard.counter())),
                                )
                            }),
                    )
                    .child(
                        div()
                            .role(Role::RadioGroup)
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .children(options),
                    )
                    .child(
                        div()
                            .border_t_1()
                            .border_color(theme.border)
                            .pt(px(8.0))
                            .child(self.input.clone()),
                    ),
            )
            .when_some(error, |el, error| {
                el.child(
                    div()
                        .px(px(12.0))
                        .pb(px(8.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .line_height(px(17.0))
                        .text_color(theme.warning)
                        .child(SharedString::from(error)),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .px(px(12.0))
                    .pb(px(12.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .when(draft.wizard.page > 0, |el| {
                                el.child(
                                    crate::popover::btn_ghost(
                                        &theme,
                                        "Back",
                                        "async-question-back",
                                    )
                                    .id("async-question-back")
                                    .role(Role::Button)
                                    .tab_index(if locked { -1 } else { 0 })
                                    .when(locked, |el| el.opacity(0.4))
                                    .focus_visible(move |style| style.bg(crate::theme::ink(0.09)))
                                    .when(!locked, |el| {
                                        el.on_click(cx.listener(|panel, _, _, cx| panel.back(cx)))
                                            .on_key_down(cx.listener(
                                                |panel, event: &gpui::KeyDownEvent, _, cx| {
                                                    if !event.keystroke.modifiers.modified()
                                                        && matches!(
                                                            event.keystroke.key.as_str(),
                                                            "enter" | "space"
                                                        )
                                                    {
                                                        panel.back(cx);
                                                        cx.stop_propagation();
                                                    }
                                                },
                                            ))
                                    }),
                                )
                            })
                            .when(self.pending_count > 1, |el| {
                                el.child(
                                    div()
                                        .text_size(crate::typography::ui_rems(12.0))
                                        .text_color(theme.text_muted)
                                        .child(SharedString::from(format!(
                                            "{} pending",
                                            self.pending_count
                                        ))),
                                )
                            }),
                    )
                    .child(
                        crate::popover::btn_primary(&theme, label)
                            .id("async-question-submit")
                            .role(Role::Button)
                            .tab_index(if can_submit { 0 } else { -1 })
                            .border_1()
                            .border_color(gpui::transparent_black())
                            .focus_visible(move |style| style.border_color(accent))
                            .when(!can_submit, |el| {
                                el.opacity(0.4).cursor(gpui::CursorStyle::Arrow)
                            })
                            .when(can_submit, |el| {
                                el.on_click(cx.listener(|panel, _, _, cx| panel.advance(cx)))
                                    .on_key_down(cx.listener(
                                        |panel, event: &gpui::KeyDownEvent, _, cx| {
                                            if !event.keystroke.modifiers.modified()
                                                && matches!(
                                                    event.keystroke.key.as_str(),
                                                    "enter" | "space"
                                                )
                                            {
                                                panel.advance(cx);
                                                cx.stop_propagation();
                                            }
                                        },
                                    ))
                            }),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext;
    use zeron_doc::MessageStatus;

    fn entry(request: &str, asynchronous: bool, resolved: bool) -> SessionMessageEntry {
        SessionMessageEntry {
            id: format!("assistant-{request}"), role: MessageRole::Assistant,
            parts: vec![MessagePart::Input {
                id: request.into(), request_id: request.into(), asynchronous, resolved,
                questions: serde_json::from_value(serde_json::json!([
                    {"id":"q1", "header":"Capture", "question":"Capture method?", "options":["Screenshots", "Streaming"]},
                    {"id":"q2", "header":"Constraints", "question":"Any constraints?", "options":[]}
                ])).unwrap(),
            }],
            created_at: 1, device_id: "host".into(), status: Some(MessageStatus::Complete),
            continuation_of: None, duration_ms: None,
        }
    }

    #[test]
    fn async_questions_are_not_superseded_by_newer_messages_or_blocking_inputs() {
        let mut entries = vec![entry("older", true, false), entry("blocking", false, false)];
        assert_eq!(pending_requests(&entries).len(), 1);
        assert_eq!(pending_requests(&entries)[0].0, "older");
        assert_eq!(
            super::super::pending_input_request(&entries).unwrap().0,
            "blocking"
        );
        entries.push(entry("newer", true, false));
        assert!(super::super::pending_input_request(&entries).is_none());
        assert_eq!(
            pending_requests(&entries)
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["older", "newer"]
        );
        entries[0] = entry("older", true, true);
        assert_eq!(pending_requests(&entries)[0].0, "newer");
    }

    #[gpui::test]
    fn selections_and_custom_answers_survive_navigation_without_replacing_chat_drafts(
        cx: &mut gpui::TestAppContext,
    ) {
        let state = cx.new(|_| AppState::new());
        let composer = cx.new(|cx| crate::composer::Composer::new(state.clone(), cx));
        let panel = composer.read_with(cx, |composer, _| composer.async_questions.clone());
        state.update(cx, |state, _| {
            state.selected_chat = Some("chat-a".into());
            state.transcript = vec![entry("async-a", true, false)];
        });
        composer.update(cx, |composer, cx| {
            composer.on_state_changed(cx);
            composer
                .input
                .update(cx, |input, cx| input.set_text("ordinary draft a", cx));
            assert!(composer.wizard.is_none());
        });
        panel.update(cx, |panel, cx| {
            panel.sync(cx);
            panel.input.update(cx, |input, cx| {
                input.set_text("discarded custom answer", cx)
            });
            panel.save_typed(cx);
            panel.select(1, cx);
            assert!(panel.input.read(cx).text().is_empty());
            let draft = panel.drafts.get(panel.current.as_ref().unwrap()).unwrap();
            assert_eq!(draft.wizard.page, 0);
            assert!(draft.wizard.is_picked(1));
            assert_eq!(draft.wizard.answers()[0].labels, ["Streaming"]);
            assert!(panel.tasks.is_empty());
            panel
                .input
                .update(cx, |input, cx| input.set_text("custom capture", cx));
            panel.advance(cx);
            panel
                .input
                .update(cx, |input, cx| input.set_text("keep it local", cx));
            panel.save_typed(cx);
        });
        state.update(cx, |state, _| {
            state.selected_chat = Some("chat-b".into());
            state.transcript.clear();
        });
        composer.update(cx, |composer, cx| {
            composer.on_state_changed(cx);
            composer
                .input
                .update(cx, |input, cx| input.set_text("ordinary draft b", cx));
        });
        panel.update(cx, |panel, cx| {
            panel.sync(cx);
            assert!(!panel.is_visible());
        });
        state.update(cx, |state, _| {
            state.selected_chat = Some("chat-a".into());
            state.transcript = vec![entry("async-a", true, false), entry("newer", true, false)];
        });
        composer.update(cx, |composer, cx| {
            composer.on_state_changed(cx);
            assert_eq!(composer.input.read(cx).text(), "ordinary draft a");
            assert_eq!(composer.drafts.get("chat-b").unwrap(), "ordinary draft b");
        });
        panel.update(cx, |panel, cx| {
            panel.sync(cx);
            assert_eq!(panel.pending_count, 2);
            assert_eq!(panel.current.as_ref().unwrap().1, "async-a");
            assert_eq!(panel.input.read(cx).text(), "keep it local");
            panel.back(cx);
            assert_eq!(panel.input.read(cx).text(), "custom capture");
            let draft = panel.drafts.get(panel.current.as_ref().unwrap()).unwrap();
            assert!(draft.wizard.is_picked(1));
            assert_eq!(draft.wizard.answers()[0].labels, ["custom capture"]);
            panel.advance(cx);
            panel.advance(cx); // No engine: report an error, retain the answers.
            let draft = panel.drafts.get(panel.current.as_ref().unwrap()).unwrap();
            assert!(draft.error.is_some());
            assert_eq!(draft.wizard.answers()[1].labels, ["keep it local"]);
            assert!(panel.accepted.is_empty());
        });
        composer.read_with(cx, |composer, cx| {
            assert_eq!(composer.input.read(cx).text(), "ordinary draft a");
        });
    }

    #[gpui::test]
    fn queued_answers_remain_visible_until_acknowledged_and_rejections_keep_choices(
        cx: &mut gpui::TestAppContext,
    ) {
        let state = cx.new(|_| AppState::new());
        state.update(cx, |state, _| {
            state.selected_chat = Some("chat".into());
            state.transcript = vec![entry("async", true, false)];
        });
        let panel = cx.new(|cx| AsyncQuestionPanel::new(state.clone(), cx));
        panel.update(cx, |panel, cx| {
            panel.select(0, cx);
            let key = panel.current.clone().unwrap();
            let draft = panel.drafts.get_mut(&key).unwrap();
            draft.command_id = Some("cmd-1".into());
            draft.sending = true;
            panel.sync(cx);
            assert!(panel.is_visible());
            panel.fail(&key, "Host unreachable".into(), false, cx);
            assert_eq!(panel.drafts[&key].command_id.as_deref(), Some("cmd-1"));
            assert!(panel.drafts[&key].wizard.is_picked(0));
            assert!(panel.drafts[&key].locked()); // Retry must reuse the command.
            panel.fail(&key, "Host rejected the answer".into(), true, cx);
            assert!(panel.drafts[&key].command_id.is_none());
            assert!(panel.drafts[&key].wizard.is_picked(0));
            assert!(!panel.drafts[&key].locked());
            panel.acknowledge(key, cx);
            assert!(!panel.is_visible());
            panel.sync(cx); // A lagging doc snapshot cannot reopen an accepted answer.
            assert!(!panel.is_visible());
        });
    }
}
