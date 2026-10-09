//! Temporary projects and an in-process harness; never contact a real agent/server.
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::{
    AgentEvent, CheckpointPreview, DoneStatus, HarnessId, Model, ReasoningLevel,
    RestoreCheckpointResult, RunRequest, SandboxLevel, SteeringMode,
};
use zeron_rpc::methods;

struct EditorHarness {
    requests: Arc<Mutex<Vec<RunRequest>>>,
    pause: Option<Arc<tokio::sync::Notify>>,
    persistent: bool,
    background_work: bool,
}

#[async_trait]
impl Harness for EditorHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Checkpoint fixture"
    }
    fn supports_steering(&self) -> bool {
        self.persistent
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        if let Some(pause) = &self.pause {
            pause.notified().await;
        }
        if self.background_work {
            controls.turn.set_background(1);
        }
        self.requests.lock().unwrap().push(request.clone());
        std::fs::write(Path::new(&request.cwd).join("app.txt"), "agent edit").unwrap();
        let first = futures::stream::iter(vec![
            Ok(AgentEvent::TextDelta {
                text: "Edited the file.".into(),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("fixture-session".into()),
            }),
        ]);
        if !self.persistent {
            return Ok(first.boxed());
        }
        let requests = self.requests.clone();
        let later = futures::stream::unfold(controls.steering, move |mut steering| {
            let requests = requests.clone();
            let mut request = request.clone();
            async move {
                let message = steering.recv().await?;
                request.prompt = message.prompt;
                std::fs::write(Path::new(&request.cwd).join("app.txt"), &request.prompt).unwrap();
                requests.lock().unwrap().push(request);
                let events = vec![
                    Ok(AgentEvent::Steered {
                        assistant_message_id: None,
                        next_assistant_message_id: Some(uuid::Uuid::new_v4().to_string()),
                    }),
                    Ok(AgentEvent::TextDelta {
                        text: "Edited again.".into(),
                    }),
                    Ok(AgentEvent::Done {
                        status: DoneStatus::Completed,
                        result: None,
                        error: None,
                        session_id: Some("fixture-session".into()),
                    }),
                ];
                Some((futures::stream::iter(events), steering))
            }
        })
        .flatten();
        Ok(first.chain(later).boxed())
    }
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn message(id: &str, role: MessageRole, text: &str, device: &str) -> SessionMessageEntry {
    SessionMessageEntry {
        id: id.into(),
        role,
        parts: vec![MessagePart::Text {
            id: format!("text-{id}"),
            text: text.into(),
        }],
        created_at: 1,
        device_id: device.into(),
        status: Some(MessageStatus::Complete),
        continuation_of: None,
        duration_ms: None,
        token_usage: None,
    }
}

fn request(root: &Path, prompt: &str) -> RunRequest {
    RunRequest {
        mcp: None,
        prompt: prompt.into(),
        harness: Some(HarnessId::Mock),
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: root.to_string_lossy().into_owned(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        resume: None,
        attachments: vec![],
        worktree: None,
    }
}

fn core(
    data: &Path,
    root: &Path,
    requests: Arc<Mutex<Vec<RunRequest>>>,
    pause: Option<Arc<tokio::sync::Notify>>,
) -> EngineCore {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(EditorHarness {
        requests,
        pause,
        persistent: false,
        background_work: false,
    }));
    let core = EngineCore::assemble(data, Arc::new(registry), HarnessId::Mock, None).unwrap();
    core.workspace
        .create_chat(
            "main",
            None,
            Some(&core.device_id),
            None,
            Some(root.to_string_lossy().into_owned()),
        )
        .unwrap();
    let doc = core.doc_host.open("main").unwrap();
    doc.doc()
        .push_message(&message(
            "u1",
            MessageRole::User,
            "Remember PINEAPPLE",
            &core.device_id,
        ))
        .unwrap();
    doc.doc()
        .push_message(&message(
            "a1",
            MessageRole::Assistant,
            "I remember PINEAPPLE",
            &core.device_id,
        ))
        .unwrap();
    core.workspace
        .set_chat_harness_session("main", "source-session", &root.to_string_lossy());
    core
}

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    std::fs::write(dir.path().join("app.txt"), "before turn").unwrap();
    git(dir.path(), &["add", "app.txt"]);
    git(
        dir.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--no-gpg-sign",
            "-qm",
            "base",
        ],
    );
    dir
}

async fn idle(core: &EngineCore, chat: &str) {
    let mut sessions = core.sessions.watch_sessions();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while core.sessions.turn_in_flight(chat) {
            sessions.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn both_restores_before_the_prompt_keeps_original_history_and_can_recover_the_backup() {
    let data = tempfile::tempdir().unwrap();
    let root = project();
    let requests = Arc::new(Mutex::new(vec![]));
    let core = core(data.path(), root.path(), requests.clone(), None);
    core.sessions
        .dispatch(
            "main",
            HarnessId::Mock,
            request(root.path(), "DISCARDED FUTURE"),
            Some("u2".into()),
        )
        .await
        .unwrap();
    idle(&core, "main").await;
    let original = core
        .doc_host
        .open("main")
        .unwrap()
        .doc()
        .read_entries()
        .unwrap();
    let client = zeron_rpc::memory_client(core.rpc_service());
    let preview: CheckpointPreview = client
        .call_as(
            methods::PREVIEW_CHECKPOINT,
            serde_json::json!({ "chatId": "main", "messageId": "u2" }),
        )
        .await
        .unwrap();
    assert!(preview.files_available);
    assert_eq!(preview.files.len(), 1);
    let operation = uuid::Uuid::new_v4().to_string();
    let params = serde_json::json!({ "chatId": "main", "messageId": "u2", "mode": "both", "operationId": operation, "token": preview.token });
    let restored: RestoreCheckpointResult = client
        .call_as(methods::RESTORE_CHECKPOINT, params.clone())
        .await
        .unwrap();
    assert!(restored.error.is_none());
    assert_eq!(
        std::fs::read_to_string(root.path().join("app.txt")).unwrap(),
        "before turn"
    );
    assert_eq!(
        core.doc_host
            .open("main")
            .unwrap()
            .doc()
            .read_entries()
            .unwrap(),
        original
    );
    let chat = restored.chat.unwrap();
    assert_eq!(chat.parent_chat_id, None);
    assert_eq!(chat.harness_session_id, None);
    assert_eq!(
        core.doc_host
            .open(&chat.id)
            .unwrap()
            .doc()
            .restored_from_chat()
            .as_deref(),
        Some("main"),
    );
    let prefix = core
        .doc_host
        .open(&chat.id)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap();
    assert_eq!(prefix[..2], original[..2]);
    assert_eq!(prefix.len(), 3);
    let retry: RestoreCheckpointResult = client
        .call_as(methods::RESTORE_CHECKPOINT, params)
        .await
        .unwrap();
    assert_eq!(retry.chat_id.as_deref(), Some(chat.id.as_str()));
    let backup = restored.backup_id.unwrap();
    let preview: CheckpointPreview = client
        .call_as(
            methods::PREVIEW_CHECKPOINT,
            serde_json::json!({ "chatId": "main", "messageId": "u2", "backupId": backup }),
        )
        .await
        .unwrap();
    assert!(!preview.conversation_available);
    let recovered: RestoreCheckpointResult = client.call_as(methods::RESTORE_CHECKPOINT, serde_json::json!({
        "chatId": "main", "messageId": "u2", "backupId": backup, "mode": "files", "operationId": uuid::Uuid::new_v4().to_string(), "token": preview.token,
    })).await.unwrap();
    assert!(recovered.error.is_none());
    assert!(recovered.chat_id.is_none());
    assert_eq!(
        std::fs::read_to_string(root.path().join("app.txt")).unwrap(),
        "agent edit"
    );
    core.sessions
        .dispatch(
            &chat.id,
            HarnessId::Mock,
            request(root.path(), "Continue here"),
            Some("restored-user".into()),
        )
        .await
        .unwrap();
    idle(&core, &chat.id).await;
    let delivered = requests.lock().unwrap()[1].clone();
    assert_eq!(delivered.resume, None);
    assert!(delivered.prompt.contains("PINEAPPLE"));
    assert!(!delivered.prompt.contains("DISCARDED FUTURE"));
    core.shutdown().await;
}

#[tokio::test]
async fn old_messages_restore_conversation_only_and_the_result_survives_engine_restart() {
    let data = tempfile::tempdir().unwrap();
    let root = project();
    let requests = Arc::new(Mutex::new(vec![]));
    let core = core(data.path(), root.path(), requests.clone(), None);
    let doc = core.doc_host.open("main").unwrap();
    doc.doc()
        .push_message(&message(
            "u2",
            MessageRole::User,
            "old prompt",
            &core.device_id,
        ))
        .unwrap();
    let client = zeron_rpc::memory_client(core.rpc_service());
    let preview: CheckpointPreview = client
        .call_as(
            methods::PREVIEW_CHECKPOINT,
            serde_json::json!({ "chatId": "main", "messageId": "u2" }),
        )
        .await
        .unwrap();
    assert!(preview.conversation_available);
    assert!(!preview.files_available);
    let params = serde_json::json!({ "chatId": "main", "messageId": "u2", "mode": "conversation", "operationId": uuid::Uuid::new_v4().to_string() });
    let result: RestoreCheckpointResult = client
        .call_as(methods::RESTORE_CHECKPOINT, params.clone())
        .await
        .unwrap();
    let restored_id = result.chat_id.unwrap();
    let source_rows = doc.doc().read_entries().unwrap();
    core.shutdown().await;
    drop(doc);
    drop(client);
    drop(core);
    let reopened = EngineCore::assemble(
        data.path(),
        Arc::new(HarnessRegistry::new()),
        HarnessId::Mock,
        None,
    )
    .unwrap();
    let client = zeron_rpc::memory_client(reopened.rpc_service());
    let retry: RestoreCheckpointResult = client
        .call_as(methods::RESTORE_CHECKPOINT, params)
        .await
        .unwrap();
    assert_eq!(retry.chat_id.as_deref(), Some(restored_id.as_str()));
    assert_eq!(
        reopened
            .doc_host
            .open(&restored_id)
            .unwrap()
            .doc()
            .read_entries()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        reopened
            .doc_host
            .open("main")
            .unwrap()
            .doc()
            .read_entries()
            .unwrap(),
        source_rows
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn stale_preview_never_overwrites_an_external_edit() {
    let data = tempfile::tempdir().unwrap();
    let root = project();
    let core = core(data.path(), root.path(), Arc::new(Mutex::new(vec![])), None);
    core.sessions
        .dispatch(
            "main",
            HarnessId::Mock,
            request(root.path(), "Edit"),
            Some("u2".into()),
        )
        .await
        .unwrap();
    idle(&core, "main").await;
    let client = zeron_rpc::memory_client(core.rpc_service());
    let preview: CheckpointPreview = client
        .call_as(
            methods::PREVIEW_CHECKPOINT,
            serde_json::json!({ "chatId": "main", "messageId": "u2" }),
        )
        .await
        .unwrap();
    std::fs::write(root.path().join("app.txt"), "manual edit").unwrap();
    let error = client.call(methods::RESTORE_CHECKPOINT, serde_json::json!({
        "chatId": "main", "messageId": "u2", "mode": "files", "operationId": uuid::Uuid::new_v4().to_string(), "token": preview.token,
    })).await.unwrap_err();
    assert!(error.to_string().contains("Files changed"));
    assert_eq!(
        std::fs::read_to_string(root.path().join("app.txt")).unwrap(),
        "manual edit"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn a_canonical_user_message_written_before_dispatch_still_gets_a_checkpoint() {
    let data = tempfile::tempdir().unwrap();
    let root = project();
    let core = core(data.path(), root.path(), Arc::new(Mutex::new(vec![])), None);
    core.doc_host
        .open("main")
        .unwrap()
        .doc()
        .push_message(&message(
            "u2",
            MessageRole::User,
            "new prompt",
            &core.device_id,
        ))
        .unwrap();
    core.sessions
        .dispatch(
            "main",
            HarnessId::Mock,
            request(root.path(), "new prompt"),
            Some("u2".into()),
        )
        .await
        .unwrap();
    idle(&core, "main").await;
    let client = zeron_rpc::memory_client(core.rpc_service());
    let preview: CheckpointPreview = client
        .call_as(
            methods::PREVIEW_CHECKPOINT,
            serde_json::json!({ "chatId": "main", "messageId": "u2" }),
        )
        .await
        .unwrap();
    assert!(preview.conversation_available);
    assert!(preview.files_available);
    assert_eq!(preview.files.len(), 1);
    core.shutdown().await;
}

#[tokio::test]
async fn parked_runtime_dispatch_and_steering_capture_before_each_delivery() {
    let data = tempfile::tempdir().unwrap();
    let root = project();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(EditorHarness {
        requests: Arc::new(Mutex::new(vec![])),
        pause: None,
        persistent: true,
        background_work: false,
    }));
    let core =
        EngineCore::assemble(data.path(), Arc::new(registry), HarnessId::Mock, None).unwrap();
    core.workspace
        .create_chat(
            "warm",
            None,
            Some(&core.device_id),
            None,
            Some(root.path().to_string_lossy().into_owned()),
        )
        .unwrap();
    let first = core
        .sessions
        .dispatch(
            "warm",
            HarnessId::Mock,
            request(root.path(), "first turn"),
            Some("u1".into()),
        )
        .await
        .unwrap();
    idle(&core, "warm").await;
    let second = core
        .sessions
        .dispatch(
            "warm",
            HarnessId::Mock,
            request(root.path(), "second turn"),
            Some("u2".into()),
        )
        .await
        .unwrap();
    assert_eq!(first, second);
    idle(&core, "warm").await;
    assert_eq!(
        core.sessions
            .steer("warm", "third turn", Some("u3".into()))
            .await
            .unwrap(),
        zeron_engine::sessions::SteerOutcome::Accepted
    );
    idle(&core, "warm").await;

    let client = zeron_rpc::memory_client(core.rpc_service());
    for (message, expected) in [("u2", "agent edit"), ("u3", "second turn")] {
        let preview: CheckpointPreview = client
            .call_as(
                methods::PREVIEW_CHECKPOINT,
                serde_json::json!({ "chatId": "warm", "messageId": message }),
            )
            .await
            .unwrap();
        assert!(preview.files_available, "{:?}", preview.files_error);
        let result: RestoreCheckpointResult = client.call_as(
            methods::RESTORE_CHECKPOINT,
            serde_json::json!({ "chatId": "warm", "messageId": message, "mode": "files", "operationId": uuid::Uuid::new_v4().to_string(), "token": preview.token }),
        ).await.unwrap();
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(
            std::fs::read_to_string(root.path().join("app.txt")).unwrap(),
            expected
        );
    }
    core.shutdown().await;
}

#[tokio::test]
async fn restoring_files_refuses_parked_background_work_without_killing_the_runtime() {
    let data = tempfile::tempdir().unwrap();
    let root = project();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(EditorHarness {
        requests: Arc::new(Mutex::new(vec![])),
        pause: None,
        persistent: true,
        background_work: true,
    }));
    let core =
        EngineCore::assemble(data.path(), Arc::new(registry), HarnessId::Mock, None).unwrap();
    core.workspace
        .create_chat(
            "background",
            None,
            Some(&core.device_id),
            None,
            Some(root.path().to_string_lossy().into_owned()),
        )
        .unwrap();
    core.sessions
        .dispatch(
            "background",
            HarnessId::Mock,
            request(root.path(), "Edit"),
            Some("u1".into()),
        )
        .await
        .unwrap();
    idle(&core, "background").await;
    assert!(core.sessions.holds_background_work("background"));
    let client = zeron_rpc::memory_client(core.rpc_service());
    let preview: CheckpointPreview = client
        .call_as(
            methods::PREVIEW_CHECKPOINT,
            serde_json::json!({ "chatId": "background", "messageId": "u1" }),
        )
        .await
        .unwrap();
    assert!(preview.conversation_available);
    assert!(!preview.files_available);
    assert!(preview.files_error.unwrap().contains("background"));
    let error = client
        .call(
            methods::RESTORE_CHECKPOINT,
            serde_json::json!({
                "chatId": "background", "messageId": "u1", "mode": "files",
                "operationId": uuid::Uuid::new_v4().to_string(), "token": "stale",
            }),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("background"));
    assert!(core.sessions.holds_background_work("background"));
    assert_eq!(
        std::fs::read_to_string(root.path().join("app.txt")).unwrap(),
        "agent edit"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn restoring_files_refuses_active_work_without_interrupting_it() {
    let data = tempfile::tempdir().unwrap();
    let root = project();
    let pause = Arc::new(tokio::sync::Notify::new());
    let core = core(
        data.path(),
        root.path(),
        Arc::new(Mutex::new(vec![])),
        Some(pause.clone()),
    );
    core.sessions
        .dispatch(
            "main",
            HarnessId::Mock,
            request(root.path(), "Edit"),
            Some("u2".into()),
        )
        .await
        .unwrap();
    let client = zeron_rpc::memory_client(core.rpc_service());
    let preview: CheckpointPreview = client
        .call_as(
            methods::PREVIEW_CHECKPOINT,
            serde_json::json!({ "chatId": "main", "messageId": "u2" }),
        )
        .await
        .unwrap();
    assert!(!preview.conversation_available && !preview.files_available);
    assert!(client.call(methods::RESTORE_CHECKPOINT, serde_json::json!({
        "chatId": "main", "messageId": "u2", "mode": "files", "operationId": uuid::Uuid::new_v4().to_string(), "token": "stale",
    })).await.is_err());
    assert!(core.sessions.turn_in_flight("main"));
    assert_eq!(
        std::fs::read_to_string(root.path().join("app.txt")).unwrap(),
        "before turn"
    );
    pause.notify_one();
    idle(&core, "main").await;
    core.shutdown().await;
}
