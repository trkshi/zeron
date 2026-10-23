//! Settings → Providers → DeepSeek Harness → API key: the DeepSeek key DSH
//! uses for its official provider.
//!
//! The key lives in DSH's own credentials file on the device that runs the
//! agent (shared with `dsh web`); this section edits it over the forwarded
//! `GetDshApiKey`/`SaveDshApiKey` RPCs and only ever receives the key's last
//! four characters back.

use gpui::{
    AnyElement, Context, Entity, IntoElement, Render, Subscription, Task, Window, div, prelude::*,
    px,
};

use zeron_harness::dsh_credentials::{DshKeyInput, DshKeyStatus};
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::{self, Loadable};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

pub struct DshKeySection {
    state: Entity<AppState>,
    target_device: Option<String>,
    status: Loadable<DshKeyStatus>,
    error: Option<String>,
    load_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    dialog: Option<KeyDialog>,
}

struct KeyDialog {
    key: Entity<ComposerInput>,
    error: Option<String>,
    focus_pending: bool,
    _events: Subscription,
}

impl DshKeySection {
    pub fn new(state: Entity<AppState>, target: Option<String>, cx: &mut Context<Self>) -> Self {
        let mut section = Self {
            state,
            target_device: target,
            status: Loadable::Idle,
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
            self.status = Loadable::Error("Engine not connected.".into());
            return;
        };
        let params = self.with_target(serde_json::json!({}));
        let target = self.target_device.clone();
        self.status = Loadable::Loading;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::GET_DSH_API_KEY, params)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<DshKeyStatus>(value).map_err(|error| error.to_string())
                });
            this.update(cx, |section, cx| {
                if section.target_device == target {
                    section.status = match result {
                        Ok(status) => Loadable::Ready(status),
                        Err(error) => Loadable::Error(error),
                    };
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Save (`Some`) or remove (`None`) the key, then show the resulting status.
    fn save(&mut self, key: Option<String>, cx: &mut Context<Self>) {
        if self.action_task.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected. Reconnect and try again.".into());
            cx.notify();
            return;
        };
        let params = self
            .with_target(serde_json::to_value(DshKeyInput { api_key: key }).unwrap_or_default());
        let target = self.target_device.clone();
        self.error = None;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SAVE_DSH_API_KEY, params)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<DshKeyStatus>(value).map_err(|error| error.to_string())
                });
            this.update(cx, |section, cx| {
                section.action_task = None;
                if section.target_device != target {
                    return;
                }
                match result {
                    Ok(status) => {
                        section.status = Loadable::Ready(status);
                        section.dialog = None;
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

    fn open_dialog(&mut self, cx: &mut Context<Self>) {
        let key = cx.new(|cx| {
            ComposerInput::new("sk-…", cx)
                .with_single_line()
                .with_tab_stop()
        });
        let events = cx.subscribe(&key, |section: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                section.submit(cx);
            }
        });
        self.dialog = Some(KeyDialog {
            key,
            error: None,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &self.dialog else {
            return;
        };
        let key = dialog.key.read(cx).text().trim().to_owned();
        if key.is_empty() {
            if let Some(dialog) = self.dialog.as_mut() {
                dialog.error = Some("Enter a DeepSeek API key.".into());
            }
            cx.notify();
            return;
        }
        self.save(Some(key), cx);
    }

    fn render_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::of(cx).for_popup();
        let saving = self.action_task.is_some();
        let dialog = self.dialog.as_mut()?;
        if std::mem::take(&mut dialog.focus_pending) {
            window.focus(&gpui::Focusable::focus_handle(dialog.key.read(cx), cx), cx);
        }
        let error = dialog.error.clone();
        let card = popover::dialog_card(&theme)
            .id("dsh-key-card")
            .w(px(420.0))
            .role(gpui::Role::Dialog)
            .aria_label("DeepSeek API key")
            .on_key_down(cx.listener(|section, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    section.dialog = None;
                    cx.notify();
                    cx.stop_propagation();
                }
            }))
            .child(popover::dialog_title(&theme, "DeepSeek API key"))
            .child(div().mt(px(6.0)).child(popover::dialog_body(
                &theme,
                "DeepSeek Harness uses this key for DeepSeek's official models. It is saved \
                 in DSH's own credentials on this device, so `dsh web` uses it too.",
            )))
            .child(
                div()
                    .mt(px(12.0))
                    .child(popover::dialog_field(dialog.key.clone().into_any_element())),
            )
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
                    .child(div().flex_1())
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "dsh-key-cancel")
                            .id("dsh-key-cancel")
                            .on_click(cx.listener(|section, _, _, cx| {
                                section.dialog = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        popover::btn_primary(&theme, if saving { "Saving…" } else { "Save" })
                            .id("dsh-key-save")
                            .when(saving, |el| el.opacity(0.5))
                            .on_click(cx.listener(|section, _, _, cx| section.submit(cx))),
                    ),
            )
            .into_any_element();
        Some(popover::modal(
            "dsh-key-dialog",
            window.viewport_size(),
            card,
        ))
    }
}

impl Render for DshKeySection {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).for_settings_surface();
        let busy = self.action_task.is_some();
        let (title, meta, saved) = match &self.status {
            Loadable::Ready(status) => match &status.key_hint {
                Some(hint) => (format!("Key …{hint}"), status.path.clone(), true),
                None => (
                    "No key saved".to_owned(),
                    "Free models from plugins still work; DeepSeek's official models need a key."
                        .to_owned(),
                    false,
                ),
            },
            Loadable::Error(_) => (
                "Unavailable".to_owned(),
                "Update or reconnect this device's engine to manage the DSH key.".to_owned(),
                false,
            ),
            _ => ("Loading…".to_owned(), String::new(), false),
        };
        let ready = self.status.ready().is_some();
        let dialog = self.render_dialog(window, cx);
        div()
            .flex()
            .flex_col()
            .child(widgets::details_label(&theme, "API key"))
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
                            .child(widgets::row_title(&theme, title))
                            .child(widgets::meta_line(
                                &theme,
                                vec![div().truncate().child(meta).into_any_element()],
                            )),
                    )
                    .child(
                        widgets::ghost_action(&theme)
                            .id("dsh-key-set")
                            .flex_none()
                            .when(!ready, |el| el.opacity(0.5))
                            .when(ready, |el| {
                                el.on_click(
                                    cx.listener(|section, _, _, cx| section.open_dialog(cx)),
                                )
                            })
                            .child(if saved { "Change" } else { "Set key" }),
                    )
                    .when(saved, |row| {
                        row.child(
                            widgets::ghost_action(&theme)
                                .id("dsh-key-remove")
                                .flex_none()
                                .text_color(theme.danger)
                                .when(busy, |el| el.opacity(0.5))
                                .when(!busy, |el| {
                                    el.on_click(
                                        cx.listener(|section, _, _, cx| section.save(None, cx)),
                                    )
                                })
                                .child("Remove"),
                        )
                    }),
            )
            .when_some(self.error.clone(), |el, error| {
                el.child(widgets::error_strip(&theme, error))
            })
            .children(dialog)
    }
}
