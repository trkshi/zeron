//! Preview and confirmation stay bound to the source conversation's host.
use super::*;
use std::sync::Arc;
use zeron_proto::{
    CheckpointParams, CheckpointPreview, RestoreCheckpointParams, RestoreCheckpointResult,
    RestoreMode, capabilities,
};

#[derive(Clone)]
pub(super) struct CheckpointDialog {
    source: zeron_proto::Chat,
    engine: Option<crate::state::EngineHandle>,
    params: CheckpointParams,
    mode: RestoreMode,
    preview: Loadable<Arc<CheckpointPreview>>,
    error: Option<String>,
    operation: Option<RestoreCheckpointParams>,
    busy: bool,
    focus: FocusHandle,
    focus_pending: bool,
    generation: u64,
}

impl CheckpointDialog {
    fn can_restore(&self) -> bool {
        !self.busy
            && self.preview.ready().is_some_and(|preview| {
                (!self.mode.includes_conversation() || preview.conversation_available)
                    && (!self.mode.includes_files() || preview.files_available)
            })
    }
}

impl Shell {
    pub(super) fn open_checkpoint_dialog(
        &mut self,
        chat_id: String,
        message_id: String,
        cx: &mut Context<Self>,
    ) {
        if self
            .checkpoint_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.busy)
        {
            return;
        }
        let Some((_, state)) = self.link_owner(&chat_id, cx) else {
            return;
        };
        let Some(source) = state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .cloned()
        else {
            return;
        };
        let engine = state.read(cx).engine().cloned();
        let supported = engine
            .as_ref()
            .is_some_and(|engine| engine.engine_info().supports(capabilities::CHECKPOINTS_V1))
            && state
                .read(cx)
                .device_supports(&source.device_id, capabilities::CHECKPOINTS_V1);
        self.checkpoint_dialog = Some(CheckpointDialog {
            source,
            engine,
            params: CheckpointParams {
                chat_id,
                message_id,
                backup_id: None,
            },
            mode: RestoreMode::Conversation,
            preview: Loadable::Idle,
            error: None,
            operation: None,
            busy: false,
            focus: cx.focus_handle(),
            focus_pending: true,
            generation: 0,
        });
        if supported {
            self.refresh_checkpoint_preview(cx);
        } else if let Some(dialog) = &mut self.checkpoint_dialog {
            dialog.preview = Loadable::Error("Update both the desktop app's engine and the source device's Zeron engine to use checkpoints.".into());
        }
        cx.notify();
    }

    pub(super) fn close_checkpoint_dialog(&mut self, cx: &mut Context<Self>) {
        if self
            .checkpoint_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.busy)
        {
            return;
        }
        self.checkpoint_dialog = None;
        self.checkpoint_task = None;
        self.focus_composer(cx);
        cx.notify();
    }

    fn refresh_checkpoint_preview(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.checkpoint_dialog else {
            return;
        };
        if dialog.busy {
            return;
        }
        let Some(engine) = dialog.engine.clone() else {
            dialog.preview = Loadable::Error("Engine is not connected.".into());
            cx.notify();
            return;
        };
        self.checkpoint_generation = self.checkpoint_generation.wrapping_add(1);
        dialog.generation = self.checkpoint_generation;
        dialog.preview = Loadable::Loading;
        dialog.operation = None;
        dialog.error = None;
        let generation = dialog.generation;
        let mut params = serde_json::to_value(&dialog.params).expect("checkpoint params serialize");
        params["targetDeviceId"] = serde_json::json!(dialog.source.device_id);
        self.checkpoint_task = Some(cx.spawn(async move |shell, cx| {
            let result = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                methods::PREVIEW_CHECKPOINT,
                params,
                Duration::from_secs(150),
            )
            .await
            .and_then(|value| {
                serde_json::from_value::<CheckpointPreview>(value).map_err(|e| e.to_string())
            });
            let _ = shell.update(cx, |shell, cx| {
                if let Some(dialog) = &mut shell.checkpoint_dialog
                    && dialog.generation == generation
                {
                    dialog.preview = match result {
                        Ok(preview) => Loadable::Ready(Arc::new(preview)),
                        Err(error) => Loadable::Error(error),
                    };
                    shell.checkpoint_task = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn select_checkpoint_backup(&mut self, backup: Option<String>, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.checkpoint_dialog else {
            return;
        };
        if dialog.busy || dialog.params.backup_id == backup {
            return;
        }
        dialog.params.backup_id = backup;
        dialog.mode = if dialog.params.backup_id.is_some() {
            RestoreMode::Files
        } else {
            RestoreMode::Conversation
        };
        self.refresh_checkpoint_preview(cx);
    }

    fn set_checkpoint_mode(&mut self, mode: RestoreMode, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.checkpoint_dialog
            && !dialog.busy
            && dialog.params.backup_id.is_none()
        {
            dialog.mode = mode;
            dialog.operation = None;
            dialog.error = None;
            cx.notify();
        }
    }

    fn confirm_checkpoint_restore(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self
            .checkpoint_dialog
            .clone()
            .filter(CheckpointDialog::can_restore)
        else {
            return;
        };
        let Some(engine) = dialog.engine.clone() else {
            return;
        };
        if !self
            .state
            .read(cx)
            .engine()
            .is_some_and(|current| current.same_connection(&engine))
        {
            if let Some(dialog) = &mut self.checkpoint_dialog {
                dialog.error = Some(
                    "The engine connection changed. Close and reopen this dialog before restoring."
                        .into(),
                );
            }
            cx.notify();
            return;
        }
        let surfaces = if dialog.mode.includes_files() {
            self.files
                .values()
                .chain(self.file_surfaces.values())
                .cloned()
                .collect::<Vec<_>>()
        } else {
            vec![]
        };
        if let Some(reason) = surfaces
            .iter()
            .find_map(|surface| surface.read(cx).checkpoint_restore_blocker())
        {
            self.checkpoint_dialog.as_mut().unwrap().error = Some(reason.into());
            cx.notify();
            return;
        }
        for surface in &surfaces {
            surface.update(cx, |files, cx| files.hold_mutation(Some(String::new()), cx));
        }
        let request = dialog.operation.unwrap_or_else(|| RestoreCheckpointParams {
            checkpoint: dialog.params.clone(),
            mode: dialog.mode,
            operation_id: uuid::Uuid::new_v4().to_string(),
            token: dialog
                .preview
                .ready()
                .and_then(|preview| preview.token.clone()),
        });
        let mut params = serde_json::to_value(&request).expect("restore params serialize");
        params["targetDeviceId"] = serde_json::json!(dialog.source.device_id);
        let flow = self.checkpoint_dialog.as_mut().unwrap();
        flow.busy = true;
        flow.error = None;
        flow.operation = Some(request);
        let generation = flow.generation;
        self.checkpoint_task = Some(cx.spawn(async move |shell, cx| {
            let result = crate::attachments::call_with_timeout(&engine, cx.background_executor(), methods::RESTORE_CHECKPOINT, params, Duration::from_secs(150)).await
                .and_then(|value| serde_json::from_value::<RestoreCheckpointResult>(value).map_err(|e| e.to_string()));
            for surface in surfaces {
                let _ = surface.update(cx, |files, cx| files.finish_checkpoint_restore(cx));
            }
            let _ = shell.update(cx, |shell, cx| {
                shell.checkpoint_task = None;
                if !shell.state.read(cx).engine().is_some_and(|current| current.same_connection(&engine)) {
                    shell.checkpoint_dialog = None;
                    cx.notify();
                    return;
                }
                let Some(flow) = &mut shell.checkpoint_dialog else { return; };
                if flow.generation != generation { return; }
                flow.busy = false;
                match result {
                    Ok(result) if result.error.is_none() => {
                        shell.checkpoint_dialog = None;
                        if let Some(chat) = result.chat {
                            let id = chat.id.clone();
                            shell.state.update(cx, |state, cx| state.accept_restored_chat(chat, cx));
                            shell.open_chat(id, cx);
                        } else {
                            shell.focus_composer(cx);
                        }
                        shell.sidebar_notice = Some(if result.backup_id.is_some() {
                            "Checkpoint restored. A recovery backup is available in the original thread's restore dialog.".into()
                        } else { "Conversation restored in a new thread. The original is unchanged.".into() });
                    }
                    Ok(result) => {
                        flow.error = result.error;
                        flow.operation = None;
                        flow.preview = Loadable::Idle;
                    }
                    Err(error) => {
                        // A lost reply reuses the operation id; it must never restore twice.
                        flow.error = Some(format!("{error}. Retry uses the same restore operation, or refresh to review the current state."));
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn render_checkpoint_dialog(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.checkpoint_dialog.is_none() {
            if !self.checkpoint_enter_down {
                self.checkpoint_keys = None;
            }
            return None;
        }
        if self.checkpoint_keys.is_none() {
            let owner = window.window_handle();
            let shell = cx.entity().downgrade();
            self.checkpoint_keys = Some(cx.intercept_keystrokes(move |event, window, cx| {
                if window.window_handle() != owner || event.keystroke.key != "enter" {
                    return;
                }
                let _ = shell.update(cx, |shell, cx| {
                    if shell.checkpoint_dialog.is_some() {
                        if shell.sync_flow.has_visible_overlay()
                            || shell.delete_confirm.is_some()
                            || crate::app_update::AppUpdate::global(cx)
                                .is_some_and(|update| update.read(cx).prompt().is_some())
                        {
                            return;
                        }
                        if !shell.checkpoint_enter_down && !event.keystroke.modifiers.modified() {
                            shell.checkpoint_enter_down = true;
                            shell.confirm_checkpoint_restore(cx);
                        }
                        cx.stop_propagation();
                    } else if shell.checkpoint_enter_down {
                        cx.stop_propagation();
                    }
                });
            }));
        }
        let flow = self.checkpoint_dialog.as_mut()?;
        if std::mem::take(&mut flow.focus_pending) {
            window.focus(&flow.focus, cx);
        }
        let dialog = flow.clone();
        let theme = Theme::of(cx).for_popup();
        let can_restore = dialog.can_restore();
        let mut body = div()
            .id("checkpoint-dialog-body")
            .min_h_0()
            .min_w_0()
            .overflow_y_scroll()
            .max_h(px((f32::from(viewport.height) - 210.0).max(80.0)))
            .flex()
            .flex_col()
            .gap(px(12.0));
        match &dialog.preview {
            Loadable::Idle => {}
            Loadable::Loading => {
                body = body.child(popover::dialog_body(
                    &theme,
                    "Loading checkpoint preview...",
                ));
            }
            Loadable::Error(error) => {
                body = body.child(popover::dialog_body(&theme, error.clone()));
            }
            Loadable::Ready(preview) => {
                if !preview.conversation_available && dialog.params.backup_id.is_none() {
                    body = body.child(popover::dialog_body(
                        &theme,
                        "Finish or stop this turn before restoring its conversation.",
                    ));
                }
                if let Some(reason) = &preview.files_error {
                    body = body.child(popover::dialog_body(
                        &theme,
                        format!("Files unavailable: {reason}"),
                    ));
                }
                if dialog.mode.includes_files() && preview.files_available {
                    body = body.child(popover::dialog_body(
                        &theme,
                        format!(
                            "{} file{} will change",
                            preview.files.len(),
                            if preview.files.len() == 1 { "" } else { "s" }
                        ),
                    ));
                    if let Some(root) = &preview.root {
                        body = body.child(
                            div()
                                .min_w_0()
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(theme.text_muted)
                                .child(root.clone()),
                        );
                    }
                    if !preview.files.is_empty() {
                        let files = preview.clone();
                        let row_theme = theme.clone();
                        body = body.child(
                            gpui::uniform_list(
                                "checkpoint-file-list",
                                files.files.len(),
                                move |range, _, _| {
                                    range
                                        .map(|index| {
                                            let file = &files.files[index];
                                            div()
                                                .h(px(28.0))
                                                .w_full()
                                                .min_w_0()
                                                .flex()
                                                .items_center()
                                                .gap(px(10.0))
                                                .text_size(crate::typography::ui_rems(12.0))
                                                .child(
                                                    div()
                                                        .w(px(58.0))
                                                        .flex_none()
                                                        .text_color(row_theme.text_muted)
                                                        .child(file.action.clone()),
                                                )
                                                .child(
                                                    div()
                                                        .id(SharedString::from(format!(
                                                            "checkpoint-path-{index}"
                                                        )))
                                                        .min_w_0()
                                                        .flex_1()
                                                        .truncate()
                                                        .tooltip(
                                                            crate::settings::widgets::text_tooltip(
                                                                file.path.clone(),
                                                            ),
                                                        )
                                                        .child(file.path.clone()),
                                                )
                                                .into_any_element()
                                        })
                                        .collect::<Vec<_>>()
                                },
                            )
                            .h(px((preview.files.len() as f32 * 28.0).min(196.0)))
                            .w_full(),
                        );
                    }
                    body = body.child(popover::dialog_body(&theme, "These project files are shared with other threads. Stop external editors and terminal jobs before restoring. A recovery backup is saved first."));
                    body = body.child(popover::dialog_body(&theme, "Ignored files, Git history, and external actions are not restored."));
                }
                if !preview.backups.is_empty() {
                    let mut backups = div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(popover::dialog_body(&theme, "Recovery backups"));
                    backups = backups.child(
                        popover::menu_row(
                            &theme,
                            dialog.params.backup_id.is_none(),
                            "checkpoint-original",
                        )
                        .id("checkpoint-original")
                        .when(dialog.busy, |row| row.opacity(0.5).cursor_default())
                        .on_click(
                            cx.listener(|this, _, _, cx| this.select_checkpoint_backup(None, cx)),
                        )
                        .child("Message checkpoint"),
                    );
                    for backup in &preview.backups {
                        let id = backup.id.clone();
                        let label = chrono::DateTime::from_timestamp_millis(backup.created_at)
                            .map(|date| {
                                date.with_timezone(&chrono::Local)
                                    .format("%b %d, %I:%M:%S %p")
                                    .to_string()
                            })
                            .unwrap_or_else(|| "Saved backup".into());
                        backups = backups.child(
                            popover::menu_row(
                                &theme,
                                dialog.params.backup_id.as_deref() == Some(&id),
                                format!("checkpoint-backup-{id}"),
                            )
                            .id(SharedString::from(format!("checkpoint-backup-{id}")))
                            .when(dialog.busy, |row| row.opacity(0.5).cursor_default())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_checkpoint_backup(Some(id.clone()), cx)
                            }))
                            .child(
                                icon(icons::CLOCK_CIRCLE)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(label),
                        );
                    }
                    body = body.child(backups);
                }
            }
        }
        if let Some(error) = &dialog.error {
            body = body.child(popover::dialog_body(&theme, error.clone()).text_color(theme.danger));
        }
        let mut modes = div().w_full().flex().gap(px(4.0));
        for (mode, label) in [
            (RestoreMode::Conversation, "Conversation"),
            (RestoreMode::Files, "Files"),
            (RestoreMode::Both, "Both"),
        ] {
            modes = modes.child(
                popover::menu_row(
                    &theme,
                    mode == dialog.mode,
                    format!("checkpoint-mode-{label}"),
                )
                .id(SharedString::from(format!("checkpoint-mode-{label}")))
                .flex_1()
                .justify_center()
                .when(dialog.busy || dialog.params.backup_id.is_some(), |row| {
                    row.opacity(0.5).cursor_default()
                })
                .on_click(cx.listener(move |this, _, _, cx| this.set_checkpoint_mode(mode, cx)))
                .child(label),
            );
        }
        let card = popover::dialog_card(&theme).id("checkpoint-dialog-card").track_focus(&dialog.focus)
            .w(px(560.0_f32.min(f32::from(viewport.width) - 32.0).max(200.0)))
            .gap(px(14.0))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => this.close_checkpoint_dialog(cx),
                    "up" | "down" => {
                        let backup = this.checkpoint_dialog.as_ref().and_then(|dialog| {
                            let preview = dialog.preview.ready()?;
                            let choices: Vec<_> = std::iter::once(None)
                                .chain(preview.backups.iter().map(|backup| Some(backup.id.clone()))).collect();
                            let current = choices.iter().position(|choice| choice == &dialog.params.backup_id).unwrap_or(0);
                            let delta = if event.keystroke.key == "up" { choices.len() - 1 } else { 1 };
                            Some(choices[(current + delta) % choices.len()].clone())
                        });
                        if let Some(backup) = backup { this.select_checkpoint_backup(backup, cx); }
                    }
                    "tab" | "left" | "right" => {
                        if let Some(dialog) = &this.checkpoint_dialog {
                            let modes = [RestoreMode::Conversation, RestoreMode::Files, RestoreMode::Both];
                            let index = modes.iter().position(|mode| *mode == dialog.mode).unwrap_or(0);
                            let backwards = event.keystroke.key == "left" || event.keystroke.modifiers.shift;
                            this.set_checkpoint_mode(modes[(index + if backwards { 2 } else { 1 }) % 3], cx);
                        }
                    }
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(popover::dialog_title(&theme, if dialog.params.backup_id.is_some() { "Restore recovery backup" } else { "Restore checkpoint" }))
            .child(popover::dialog_body(&theme, if dialog.params.backup_id.is_some() {
                "Restore the project files saved before an earlier restore."
            } else { "Return to before this message. Conversation restore opens a new thread; the original stays intact." }))
            .when(dialog.params.backup_id.is_none(), |card| card.child(modes))
            .child(body)
            .child(div().flex_none().flex().flex_wrap().justify_between().gap(px(8.0))
                .child(popover::btn_ghost(&theme, "Refresh", "checkpoint-refresh").id("checkpoint-refresh")
                    .when(dialog.busy, |button| button.opacity(0.5).cursor_default())
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_checkpoint_preview(cx))))
                .child(div().flex().flex_wrap().gap(px(8.0))
                    .child(popover::btn_ghost(&theme, "Cancel", "checkpoint-cancel").id("checkpoint-cancel")
                        .when(dialog.busy, |button| button.opacity(0.5).cursor_default())
                        .on_click(cx.listener(|this, _, _, cx| this.close_checkpoint_dialog(cx))))
                    .child(popover::btn_primary(&theme, if dialog.busy { "Restoring..." } else if dialog.operation.is_some() { "Retry restore" } else { "Restore" })
                        .id("checkpoint-confirm").when(!can_restore, |button| button.opacity(0.5).cursor_default())
                        .on_click(cx.listener(|this, _, _, cx| this.confirm_checkpoint_restore(cx))))));
        Some(popover::modal(
            "checkpoint-dialog",
            viewport,
            card.into_any_element(),
        ))
    }
}
