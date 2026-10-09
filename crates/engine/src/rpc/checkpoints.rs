//! Checkpoint RPCs run on the source host and never mutate its transcript.
use super::*;
use zeron_doc::{MessageRole, MessageStatus, SessionMessageEntry};
use zeron_proto::{
    Chat, CheckpointParams, CheckpointPreview, RestoreCheckpointParams, RestoreCheckpointResult,
};

fn failed(error: impl std::fmt::Display) -> RpcError {
    RpcError::Failed(error.to_string())
}

#[derive(Clone)]
struct RestoreHost {
    checkpoints: crate::checkpoints::Checkpoints,
    sessions: SessionsEngine,
    docs: DocHost,
    workspace: WorkspaceHost,
    repos: Repos,
    files: crate::WorkspaceFiles,
}

impl EngineRpc {
    fn restore_host(&self) -> Result<RestoreHost, RpcError> {
        Ok(RestoreHost {
            checkpoints: self
                .checkpoints
                .clone()
                .ok_or_else(|| failed("Checkpoint service is unavailable"))?,
            sessions: self.sessions.clone(),
            docs: self.doc_host.clone(),
            workspace: self.workspace.clone(),
            repos: self.repos.clone(),
            files: self.workspace_files.clone(),
        })
    }

    pub(super) async fn preview_checkpoint(
        &self,
        params: CheckpointParams,
    ) -> Result<CheckpointPreview, RpcError> {
        let host = self.restore_host()?;
        let admission = host.checkpoints.gate.clone().lock_owned().await;
        let (source, _) = host.source_prefix(&params)?;
        let conversation_available =
            params.backup_id.is_none() && !host.sessions.turn_in_flight(&source.id);
        let unavailable = if host.sessions.checkpoint_busy() {
            Some("Stop active turns and background agents before restoring files".to_owned())
        } else {
            None
        };
        let identity = if unavailable.is_none() {
            Some(host.identity(&source).await)
        } else {
            None
        };
        let file_guard = if let Some(Ok(identity)) = &identity {
            Some(host.files.mutation_gate(&identity.id).write_owned().await)
        } else {
            None
        };
        tokio::task::spawn_blocking(move || {
            let (_admission, _file_guard) = (admission, file_guard);
            let backups = host.checkpoints.backups(&source.id).map_err(failed)?;
            let plan = match (unavailable, identity) {
                (Some(reason), _) => Err(reason),
                (_, Some(Ok(identity))) => host.checkpoints.plan(
                    &source.id,
                    &params.message_id,
                    params.backup_id.as_deref(),
                    &identity.root,
                ),
                (_, Some(Err(error))) => Err(error.to_string()),
                _ => Err("File checkpoint unavailable".into()),
            };
            Ok(match plan {
                Ok(plan) => CheckpointPreview {
                    conversation_available,
                    files_available: true,
                    files_error: None,
                    root: Some(plan.root()),
                    token: Some(plan.token),
                    files: plan.files,
                    backups,
                },
                Err(reason) => CheckpointPreview {
                    conversation_available,
                    files_available: false,
                    files_error: Some(reason),
                    root: None,
                    token: None,
                    files: vec![],
                    backups,
                },
            })
        })
        .await
        .map_err(failed)?
    }

    pub(super) async fn restore_checkpoint(
        &self,
        params: RestoreCheckpointParams,
    ) -> Result<RestoreCheckpointResult, RpcError> {
        if !uuid::Uuid::parse_str(&params.operation_id)
            .is_ok_and(|id| id.to_string() == params.operation_id)
        {
            return Err(RpcError::BadParams(
                "Restore operation id must be a canonical UUID".into(),
            ));
        }
        if params.checkpoint.backup_id.is_some() && params.mode.includes_conversation() {
            return Err(RpcError::BadParams(
                "A recovery backup restores files only".into(),
            ));
        }
        let host = self.restore_host()?;
        // Keep ownership independent of the connection: a lost reply must not
        // release either lock while the filesystem worker is still running.
        tokio::spawn(async move {
            let admission = host.checkpoints.gate.clone().lock_owned().await;
            let request = serde_json::to_string(&params).map_err(failed)?;
            if let Some(previous) = host.checkpoints.previous_restore(&params.operation_id, &request).map_err(failed)? {
                return Ok(previous);
            }
            let (source, prefix) = host.source_prefix(&params.checkpoint)?;
            if host.sessions.turn_in_flight(&source.id) {
                return Err(failed("Wait for this turn to finish or stop it before restoring"));
            }
            let identity = if params.mode.includes_files() {
                // Retire only idle children; active work is never cancelled by restore.
                host.sessions.retire_for_file_restore().await.map_err(failed)?;
                Some(host.identity(&source).await?)
            } else {
                None
            };
            let file_guard = if let Some(identity) = &identity {
                Some(host.files.mutation_gate(&identity.id).write_owned().await)
            } else {
                None
            };
            tokio::task::spawn_blocking(move || {
                let (_admission, _file_guard) = (admission, file_guard);
                // Chat deletion or a checkout change while awaiting locks is not consent.
                let (fresh_source, fresh_prefix) = host.source_prefix(&params.checkpoint)?;
                if fresh_source.cwd != source.cwd || fresh_source.space_id != source.space_id
                    || fresh_source.checkout_id != source.checkout_id || fresh_source.device_id != source.device_id
                    || fresh_prefix != prefix
                {
                    return Err(failed("Conversation changed; refresh the checkpoint preview"));
                }
                if params.mode.includes_conversation()
                    && host.workspace.chat(&format!("restore-{}", params.operation_id)).map_err(failed)?.is_some()
                {
                    return Err(failed("This restore's new conversation already exists. Use a fresh restore operation."));
                }
                let plan = if let Some(identity) = identity {
                    let plan = host.checkpoints.plan(&source.id, &params.checkpoint.message_id, params.checkpoint.backup_id.as_deref(), &identity.root).map_err(failed)?;
                    if params.token.as_deref() != Some(plan.token.as_str()) {
                        return Err(failed("Files changed after the preview. Refresh it before restoring; no files were changed."));
                    }
                    Some(plan)
                } else {
                    None
                };
                let backup_id = plan.as_ref().map(|plan| host.checkpoints.save_backup(&source.id, &params.checkpoint.message_id, plan)).transpose().map_err(failed)?;
                host.checkpoints.begin_restore(&params.operation_id, &request, backup_id.as_deref()).map_err(failed)?;
                let mut result = RestoreCheckpointResult { chat_id: None, chat: None, backup_id, error: None };
                let outcome = (|| -> Result<(), String> {
                    if let Some(plan) = &plan {
                        host.checkpoints.apply(plan)?;
                    }
                    if params.mode.includes_conversation() {
                        let chat = host.create_restored_chat(&source, prefix, &params.operation_id)
                            .map_err(|e| if plan.is_some() {
                                format!("Files were restored, but the new conversation could not be created: {e}. The original thread and file backup are intact.")
                            } else { e.to_string() })?;
                        result.chat_id = Some(chat.id.clone());
                        result.chat = Some(chat);
                    }
                    Ok(())
                })();
                if let Err(error) = outcome { result.error = Some(error); }
                host.checkpoints.finish_restore(&params.operation_id, &result).map_err(failed)?;
                Ok(result)
            }).await.map_err(failed)?
        }).await.map_err(failed)?
    }
}

impl RestoreHost {
    fn source_prefix(
        &self,
        params: &CheckpointParams,
    ) -> Result<(Chat, Vec<SessionMessageEntry>), RpcError> {
        if params.chat_id.is_empty()
            || params.chat_id.len() > 256
            || params.message_id.is_empty()
            || params.message_id.len() > 256
            || params
                .backup_id
                .as_ref()
                .is_some_and(|id| id.is_empty() || id.len() > 256)
        {
            return Err(RpcError::BadParams("Invalid checkpoint message".into()));
        }
        let source = self
            .workspace
            .chat(&params.chat_id)
            .map_err(failed)?
            .ok_or_else(|| failed("Source conversation no longer exists"))?;
        if source.device_id != self.docs.device_id() {
            return Err(failed(
                "Restore must run on the source conversation's device",
            ));
        }
        let doc = self.docs.open(&source.id).map_err(failed)?;
        let entries = doc.doc().read_entries().map_err(failed)?;
        let boundary = entries
            .iter()
            .position(|entry| entry.id == params.message_id && entry.role == MessageRole::User)
            .ok_or_else(|| failed("This user message no longer exists"))?;
        let prefix = entries[..boundary].to_vec();
        if prefix
            .iter()
            .any(|entry| entry.status == Some(MessageStatus::Streaming))
        {
            return Err(failed(
                "The preceding response is still streaming; wait for it to finish",
            ));
        }
        Ok((source, prefix))
    }

    async fn identity(&self, source: &Chat) -> Result<crate::repos::CheckoutIdentity, RpcError> {
        let cwd = source
            .cwd
            .clone()
            .or_else(|| {
                source
                    .space_id
                    .as_deref()
                    .and_then(|id| self.workspace.space(id).ok().flatten())
                    .map(|space| space.path)
            })
            .ok_or_else(|| failed("This conversation has no project folder"))?;
        let cwd = crate::repos::expand_home(&cwd).map_err(failed)?;
        self.repos
            .checkout_identity(std::path::Path::new(&cwd))
            .await
            .map_err(failed)
    }

    fn create_restored_chat(
        &self,
        source: &Chat,
        prefix: Vec<SessionMessageEntry>,
        operation: &str,
    ) -> Result<Chat, crate::EngineError> {
        let mut chat = source.clone();
        chat.id = format!("restore-{operation}");
        chat.parent_chat_id = None;
        chat.title = Some(format!(
            "{} (restored)",
            source.title.as_deref().unwrap_or("New session")
        ));
        chat.archived = false;
        chat.created_at = chrono::Utc::now();
        chat.last_message_at = None;
        chat.last_message_preview = None;
        chat.last_seen_at = None;
        chat.harness_session_id = None;
        chat.harness_session_cwd = None;
        chat.room_gen = Some(2);
        let target = self.docs.open(&chat.id)?;
        target.doc().set_restored_from_chat(&source.id)?;
        for mut entry in prefix {
            for part in &mut entry.parts {
                match part {
                    MessagePart::Input { resolved, .. } => *resolved = true,
                    MessagePart::Tool {
                        subagent_status: status @ Some(zeron_doc::SubagentStatus::Running),
                        ..
                    } => {
                        *status = Some(zeron_doc::SubagentStatus::Failed);
                    }
                    _ => {}
                }
            }
            target.doc().push_message(&entry)?;
        }
        let marker = format!("fork:{}", chat.id);
        target.doc().push_message(&SessionMessageEntry {
            id: marker.clone(),
            role: MessageRole::System,
            parts: vec![MessagePart::Fork {
                id: marker,
                source_chat_id: source.id.clone(),
                source_title: source.title.clone().unwrap_or_else(|| "New session".into()),
            }],
            created_at: crate::now_ms(),
            device_id: self.docs.device_id().to_owned(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
            duration_ms: None,
            token_usage: None,
        })?;
        self.docs.persist_fork(&target)?;
        self.workspace.import_chat_row(&chat)?;
        Ok(chat)
    }
}
