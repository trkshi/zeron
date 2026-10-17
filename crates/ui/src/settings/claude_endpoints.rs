//! Settings → Providers → Claude Code → Custom endpoints: run Claude Code
//! against an Anthropic-compatible API such as DeepSeek.
//!
//! The endpoints (keys included) live on the engine of the device that runs
//! the agents, in its `claude-endpoints.json`; this section edits them over
//! the forwarded `*ClaudeEndpoint*` RPCs and only ever receives a key's last
//! four characters back. Their models appear in the Claude Code picker.

use gpui::{
    AnyElement, Context, Entity, IntoElement, Render, SharedString, Subscription, Task, Window,
    div, prelude::*, px,
};

use zeron_harness::claude::endpoints::{EndpointInput, EndpointSummary};
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::{self, Loadable};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// DeepSeek's Anthropic-compatible API, per its Claude Code guide. The
/// `[1m]` suffix tells Claude Code the model has a 1M-token window; the CLI
/// strips it from the requests it sends.
const DEEPSEEK: (&str, &str, &str, &str) = (
    "DeepSeek",
    "https://api.deepseek.com/anthropic",
    "deepseek-v4-pro[1m], deepseek-flash[1m]",
    "deepseek-flash",
);

pub struct ClaudeEndpointsSection {
    state: Entity<AppState>,
    target_device: Option<String>,
    endpoints: Loadable<Vec<EndpointSummary>>,
    error: Option<String>,
    load_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    dialog: Option<EndpointDialog>,
}

struct EndpointDialog {
    /// The endpoint being edited; `None` adds one.
    editing: Option<EndpointSummary>,
    name: Entity<ComposerInput>,
    base_url: Entity<ComposerInput>,
    token: Entity<ComposerInput>,
    models: Entity<ComposerInput>,
    small_model: Entity<ComposerInput>,
    error: Option<String>,
    focus_pending: bool,
    _events: Vec<Subscription>,
}

impl ClaudeEndpointsSection {
    pub fn new(state: Entity<AppState>, target: Option<String>, cx: &mut Context<Self>) -> Self {
        let mut section = Self {
            state,
            target_device: target,
            endpoints: Loadable::Idle,
            error: None,
            load_task: None,
            action_task: None,
            dialog: None,
        };
        section.load(cx);
        section
    }

    pub(crate) fn set_target_device(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        if self.target_device == target {
            return;
        }
        self.target_device = target;
        self.dialog = None;
        self.action_task = None;
        self.error = None;
        self.load(cx);
        cx.notify();
    }

    /// Escape closes the dialog first. Returns whether it was consumed.
    pub(crate) fn dismiss_on_escape(&mut self, cx: &mut Context<Self>) -> bool {
        let open = self.dialog.take().is_some();
        if open {
            cx.notify();
        }
        open
    }

    fn with_target(&self, mut value: serde_json::Value) -> serde_json::Value {
        if let (Some(target), Some(object)) = (&self.target_device, value.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(target));
        }
        value
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.endpoints = Loadable::Error("Engine not connected.".into());
            return;
        };
        let params = self.with_target(serde_json::json!({}));
        let target = self.target_device.clone();
        self.endpoints = Loadable::Loading;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_CLAUDE_ENDPOINTS, params)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<Vec<EndpointSummary>>(value)
                        .map_err(|error| error.to_string())
                });
            this.update(cx, |section, cx| {
                if section.target_device == target {
                    section.endpoints = match result {
                        Ok(list) => Loadable::Ready(list),
                        Err(error) => Loadable::Error(error),
                    };
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Save or remove, then show the engine's resulting list.
    fn call(&mut self, method: &'static str, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected. Reconnect and try again.".into());
            cx.notify();
            return;
        };
        let params = self.with_target(params);
        let target = self.target_device.clone();
        self.error = None;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(method, params)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<Vec<EndpointSummary>>(value)
                        .map_err(|error| error.to_string())
                });
            this.update(cx, |section, cx| {
                section.action_task = None;
                if section.target_device != target {
                    return;
                }
                match result {
                    Ok(list) => {
                        section.endpoints = Loadable::Ready(list);
                        section.dialog = None;
                        // New endpoint models join the Claude Code picker.
                        crate::pickers::bump_harness_catalog(cx);
                    }
                    Err(error) => match section.dialog.as_mut() {
                        Some(dialog) => dialog.error = Some(error),
                        None => section.error = Some(error),
                    },
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn open_dialog(&mut self, editing: Option<EndpointSummary>, cx: &mut Context<Self>) {
        let field = |placeholder: &'static str, cx: &mut Context<Self>| {
            cx.new(|cx| {
                ComposerInput::new(placeholder, cx)
                    .with_single_line()
                    .with_tab_stop()
            })
        };
        let name = field("Name, e.g. DeepSeek", cx);
        let base_url = field("https://api.example.com/anthropic", cx);
        let token = field(
            if editing.is_some() {
                "Leave empty to keep the saved key"
            } else {
                "API key"
            },
            cx,
        );
        let models = field("Models, comma-separated", cx);
        let small_model = field("Small model (optional)", cx);
        if let Some(endpoint) = &editing {
            name.update(cx, |input, cx| input.set_text(endpoint.label.clone(), cx));
            base_url.update(cx, |input, cx| input.set_text(endpoint.base_url.clone(), cx));
            models.update(cx, |input, cx| input.set_text(endpoint.models.join(", "), cx));
            if let Some(small) = &endpoint.small_model {
                small_model.update(cx, |input, cx| input.set_text(small.clone(), cx));
            }
        }
        let events = [&name, &base_url, &token, &models, &small_model]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |section: &mut Self, _, event, cx| {
                    if matches!(event, ComposerInputEvent::Submitted) {
                        section.submit(cx);
                    }
                })
            })
            .collect();
        self.dialog = Some(EndpointDialog {
            editing,
            name,
            base_url,
            token,
            models,
            small_model,
            error: None,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    fn fill_deepseek(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &self.dialog else {
            return;
        };
        let (name, base_url, models, small) = DEEPSEEK;
        dialog.name.update(cx, |input, cx| input.set_text(name, cx));
        dialog.base_url.update(cx, |input, cx| input.set_text(base_url, cx));
        dialog.models.update(cx, |input, cx| input.set_text(models, cx));
        dialog.small_model.update(cx, |input, cx| input.set_text(small, cx));
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.action_task.is_some() {
            return;
        }
        let Some(dialog) = &self.dialog else {
            return;
        };
        let text = |input: &Entity<ComposerInput>| input.read(cx).text().trim().to_owned();
        let token = text(&dialog.token);
        let small_model = text(&dialog.small_model);
        let input = EndpointInput {
            replaces: dialog.editing.as_ref().map(|old| old.id.clone()),
            label: text(&dialog.name),
            base_url: text(&dialog.base_url),
            token: (!token.is_empty()).then_some(token),
            models: text(&dialog.models)
                .split(',')
                .map(|model| model.trim().to_owned())
                .filter(|model| !model.is_empty())
                .collect(),
            small_model: (!small_model.is_empty()).then_some(small_model),
        };
        let params = serde_json::to_value(&input).unwrap_or_default();
        self.call(methods::SAVE_CLAUDE_ENDPOINT, params, cx);
    }

    fn render_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::of(cx).for_popup();
        let saving = self.action_task.is_some();
        let dialog = self.dialog.as_mut()?;
        if std::mem::take(&mut dialog.focus_pending) {
            window.focus(&gpui::Focusable::focus_handle(dialog.name.read(cx), cx), cx);
        }
        let title = if dialog.editing.is_some() {
            "Edit endpoint"
        } else {
            "Add endpoint"
        };
        let label = |text: &'static str| {
            div()
                .mt(px(12.0))
                .mb(px(4.0))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_muted)
                .child(text)
        };
        let fields = [
            ("Name", dialog.name.clone()),
            ("Base URL", dialog.base_url.clone()),
            ("API key", dialog.token.clone()),
            ("Models", dialog.models.clone()),
            ("Small model", dialog.small_model.clone()),
        ];
        let error = dialog.error.clone();
        let card = popover::dialog_card(&theme)
            .id("claude-endpoint-card")
            .w(px(420.0))
            .role(gpui::Role::Dialog)
            .aria_label(title)
            .on_key_down(cx.listener(|section, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    section.dialog = None;
                    cx.notify();
                    cx.stop_propagation();
                }
            }))
            .child(popover::dialog_title(&theme, title))
            .child(div().mt(px(6.0)).child(popover::dialog_body(
                &theme,
                "Claude Code sends this chat's requests to an Anthropic-compatible API. \
                 The key stays on this device. A chat keeps the backend it started on.",
            )))
            .children(fields.into_iter().map(|(text, input)| {
                div()
                    .child(label(text))
                    .child(popover::dialog_field(input.into_any_element()))
            }))
            .when_some(error, |card, error| {
                card.child(
                    div()
                        .mt(px(8.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.danger)
                        .child(error),
                )
            })
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(&theme, "Use DeepSeek", "claude-endpoint-deepseek")
                            .id("claude-endpoint-deepseek")
                            .on_click(cx.listener(|section, _, _, cx| section.fill_deepseek(cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "claude-endpoint-cancel")
                            .id("claude-endpoint-cancel")
                            .on_click(cx.listener(|section, _, _, cx| {
                                section.dialog = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        popover::btn_primary(&theme, if saving { "Saving…" } else { "Save" })
                            .id("claude-endpoint-save")
                            .when(saving, |el| el.opacity(0.5))
                            .on_click(cx.listener(|section, _, _, cx| section.submit(cx))),
                    ),
            )
            .into_any_element();
        Some(popover::modal(
            "claude-endpoint-dialog",
            window.viewport_size(),
            card,
        ))
    }

    fn render_row(
        &self,
        endpoint: &EndpointSummary,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.action_task.is_some();
        let meta = format!(
            "{} · {} · key …{}",
            endpoint.base_url,
            endpoint.models.join(", "),
            endpoint.token_hint
        );
        let edit = endpoint.clone();
        let id = endpoint.id.clone();
        // The mark its models show in the picker.
        let (mark, tint) = crate::pickers::model_brand_icon(
            zeron_proto::HarnessId::ClaudeCode,
            Some(zeron_harness::claude::endpoints::model_id(&endpoint.id, "-").as_str()),
        );
        div()
            .min_h(px(48.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .child(
                crate::icons::icon(mark)
                    .size(px(16.0))
                    .text_color(tint.unwrap_or(theme.text)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(widgets::row_title(theme, endpoint.label.clone()))
                    .child(widgets::meta_line(
                        theme,
                        vec![div().truncate().child(meta).into_any_element()],
                    )),
            )
            .child(
                widgets::ghost_action(theme)
                    .id(SharedString::from(format!("claude-endpoint-edit-{}", endpoint.id)))
                    .flex_none()
                    .on_click(cx.listener(move |section, _, _, cx| {
                        section.open_dialog(Some(edit.clone()), cx);
                    }))
                    .child("Edit"),
            )
            .child(
                widgets::ghost_action(theme)
                    .id(SharedString::from(format!("claude-endpoint-remove-{}", endpoint.id)))
                    .flex_none()
                    .text_color(theme.danger)
                    .when(busy, |el| el.opacity(0.5))
                    .when(!busy, |el| {
                        el.on_click(cx.listener(move |section, _, _, cx| {
                            section.call(
                                methods::REMOVE_CLAUDE_ENDPOINT,
                                serde_json::json!({ "id": id }),
                                cx,
                            );
                        }))
                    })
                    .child("Remove"),
            )
            .into_any_element()
    }
}

impl Render for ClaudeEndpointsSection {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).for_settings_surface();
        let note = match &self.endpoints {
            Loadable::Error(_) => {
                "Update or reconnect this device's engine to manage custom endpoints."
            }
            _ => "Run Claude Code on an Anthropic-compatible API such as DeepSeek.",
        };
        let rows: Vec<AnyElement> = self
            .endpoints
            .ready()
            .map(|list| {
                list.iter()
                    .map(|endpoint| self.render_row(endpoint, &theme, cx))
                    .collect()
            })
            .unwrap_or_default();
        let can_add = self.endpoints.ready().is_some();
        let dialog = self.render_dialog(window, cx);
        div()
            .flex()
            .flex_col()
            .child(widgets::details_label(&theme, "Custom endpoints"))
            .children(rows)
            .child(
                div()
                    .min_h(px(48.0))
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(widgets::meta_line(
                                &theme,
                                vec![div().child(note).into_any_element()],
                            )),
                    )
                    .child(
                        widgets::ghost_action(&theme)
                            .id("claude-endpoint-add")
                            .flex_none()
                            .when(!can_add, |el| el.opacity(0.5))
                            .when(can_add, |el| {
                                el.on_click(
                                    cx.listener(|section, _, _, cx| section.open_dialog(None, cx)),
                                )
                            })
                            .child("Add endpoint"),
                    ),
            )
            .when_some(self.error.clone(), |el, error| {
                el.child(widgets::error_strip(&theme, error))
            })
            .children(dialog)
    }
}
