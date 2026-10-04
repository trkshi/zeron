use super::*;
use std::sync::Mutex;
use zeron_proto::{UserInputAnswer, UserInputQuestion};

struct QuestionHarness {
    live: bool,
    feed: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<AgentEvent>>>,
    requests: Arc<Mutex<Vec<RunRequest>>>,
    steers: Arc<Mutex<Vec<String>>>,
}

fn harness(
    live: bool,
) -> (
    Arc<QuestionHarness>,
    tokio::sync::mpsc::UnboundedSender<AgentEvent>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (
        Arc::new(QuestionHarness {
            live,
            feed: Mutex::new(Some(rx)),
            requests: Arc::new(Mutex::new(Vec::new())),
            steers: Arc::new(Mutex::new(Vec::new())),
        }),
        tx,
    )
}

#[async_trait]
impl Harness for QuestionHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Async question fixture"
    }
    fn supports_steering(&self) -> bool {
        self.live
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
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
        mut controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.requests.lock().unwrap().push(request.clone());
        let feed = self.feed.lock().unwrap().take();
        let Some(mut feed) = feed else {
            return MockHarness {
                script: vec![
                    AgentEvent::TextDelta {
                        text: request.prompt.clone(),
                    },
                    done(DoneStatus::Completed),
                ],
            }
            .run(request, controls)
            .await;
        };
        let steers = self.steers.clone();
        let live = self.live;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    event = feed.recv() => match event { Some(event) => event, None => break },
                    steer = controls.steering.recv() => match steer {
                        Some(steer) => {
                            steers.lock().unwrap().push(steer.prompt);
                            AgentEvent::Steered { assistant_message_id: None, next_assistant_message_id: None }
                        }
                        None => break,
                    },
                    _ = controls.interrupt.cancelled() => {
                        let _ = tx.send(Ok(done(DoneStatus::Interrupted))).await;
                        break;
                    }
                };
                let done = matches!(&event, AgentEvent::Done { .. });
                if tx.send(Ok(event)).await.is_err() || (done && !live) {
                    break;
                }
            }
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
}

fn questions() -> Vec<UserInputQuestion> {
    serde_json::from_value(serde_json::json!([
        {"id":"q1", "header":"Capture", "question":"Capture method?", "options":["Screenshots", "Streaming"]},
        {"id":"q2", "header":"Constraints", "question":"Any constraints?", "options":[]}
    ])).unwrap()
}

fn ask() -> AgentEvent {
    AgentEvent::AsyncInputRequested {
        request_id: "async-1".into(),
        questions: questions(),
    }
}

fn answer() -> SessionCommandPayload {
    SessionCommandPayload::RespondInput {
        request_id: "async-1".into(),
        answers: vec![
            UserInputAnswer {
                question_id: "q1".into(),
                labels: vec!["Screenshots".into()],
            },
            UserInputAnswer {
                question_id: "q2".into(),
                labels: vec!["Keep it local".into()],
            },
        ],
    }
}

fn input_present(core: &EngineCore, resolved: bool) -> bool {
    entries_now(core).iter().flat_map(|entry| &entry.parts).any(|part| {
        matches!(part, MessagePart::Input { request_id, asynchronous: true, resolved: value, .. }
            if request_id == "async-1" && *value == resolved)
    })
}

async fn start(core: &EngineCore, feed: &tokio::sync::mpsc::UnboundedSender<AgentEvent>) {
    core.sessions
        .dispatch(
            CHAT,
            HarnessId::Mock,
            run_request("opening"),
            Some("opening".into()),
        )
        .await
        .unwrap();
    feed.send(mock_script()[0].clone()).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn async_answers_steer_without_blocking_and_only_one_device_wins() {
    let (harness, feed) = harness(true);
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), harness.clone());
    start(&core, &feed).await;
    feed.send(ask()).unwrap();
    feed.send(AgentEvent::TextDelta {
        text: "Still working".into(),
    })
    .unwrap();
    wait_for(|| input_present(&core, false), "async input to publish").await;
    assert_eq!(
        core.sessions.session_status(CHAT).unwrap().status,
        SessionStatus::Working
    );
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "invalid-answer",
        SessionCommandPayload::RespondInput {
            request_id: "async-1".into(),
            answers: vec![],
        },
    );
    wait_for(
        || {
            matches!(
                command_status(&core, "invalid-answer"),
                Some((SessionCommandStatus::Rejected, _))
            )
        },
        "invalid answer rejection",
    )
    .await;
    assert!(input_present(&core, false));
    queue_as_viewer(handle.doc(), "device-a-answer", answer());
    queue_as_viewer(handle.doc(), "device-b-answer", answer());
    wait_for(
        || {
            ["device-a-answer", "device-b-answer"].iter().all(|id| {
                command_status(&core, id)
                    .is_some_and(|(status, _)| status != SessionCommandStatus::Pending)
            })
        },
        "competing answers to settle",
    )
    .await;
    let statuses: Vec<_> = ["device-a-answer", "device-b-answer"]
        .iter()
        .map(|id| command_status(&core, id).unwrap().0)
        .collect();
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == SessionCommandStatus::Applied)
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == SessionCommandStatus::Rejected)
            .count(),
        1
    );
    wait_for(
        || harness.steers.lock().unwrap().len() == 1,
        "one live steer",
    )
    .await;
    assert_eq!(harness.requests.lock().unwrap().len(), 1);
    let prompt = harness.steers.lock().unwrap()[0].clone();
    assert!(prompt.contains("Capture method?"));
    assert!(prompt.contains("Screenshots"));
    assert!(prompt.contains("Any constraints?"));
    assert!(prompt.contains("Keep it local"));
    feed.send(ask()).unwrap(); // Provider replay must not reopen the resolved part.
    feed.send(AgentEvent::TextDelta {
        text: "More work after answering".into(),
    })
    .unwrap();
    feed.send(done(DoneStatus::Completed)).unwrap();
    wait_for(
        || core.sessions.session_status(CHAT).unwrap().status == SessionStatus::Idle,
        "live turn completion",
    )
    .await;
    assert!(input_present(&core, true));
    assert_eq!(entries(&core).iter().flat_map(|entry| &entry.parts)
        .filter(|part| matches!(part, MessagePart::Input { request_id, .. } if request_id == "async-1")).count(), 1);
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn late_async_questions_do_not_reopen_finished_turns() {
    let (harness, feed) = harness(true);
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), harness);
    start(&core, &feed).await;
    feed.send(AgentEvent::TextDelta {
        text: "Finished".into(),
    })
    .unwrap();
    feed.send(done(DoneStatus::Completed)).unwrap();
    wait_for(
        || core.sessions.session_status(CHAT).unwrap().status == SessionStatus::Idle,
        "idle before late question",
    )
    .await;
    feed.send(ask()).unwrap();
    wait_for(
        || input_present(&core, false),
        "late async question to publish",
    )
    .await;
    assert_eq!(
        core.sessions.session_status(CHAT).unwrap().status,
        SessionStatus::Idle
    );
    assert!(
        entries(&core)
            .iter()
            .filter(|entry| entry
                .parts
                .iter()
                .any(|part| matches!(part, MessagePart::Input { .. })))
            .all(|entry| entry.status == Some(MessageStatus::Complete))
    );
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn async_question_survives_done_and_restart_then_resumes_its_session() {
    let (harness, feed) = harness(false);
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), harness.clone());
    core.workspace
        .create_chat(CHAT, None, Some(&core.device_id), None, Some("/tmp".into()))
        .unwrap();
    start(&core, &feed).await;
    feed.send(ask()).unwrap();
    feed.send(done(DoneStatus::Completed)).unwrap();
    wait_for(
        || core.sessions.session_status(CHAT).unwrap().status == SessionStatus::Idle,
        "question turn completion",
    )
    .await;
    assert!(input_present(&core, false));
    core.shutdown().await;
    drop(core);
    let core = assemble(dir.path(), harness.clone());
    assert!(input_present(&core, false));
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(handle.doc(), "answer-after-restart", answer());
    wait_for(
        || {
            matches!(
                command_status(&core, "answer-after-restart"),
                Some((SessionCommandStatus::Applied, _))
            )
        },
        "resumed answer acceptance",
    )
    .await;
    wait_for(
        || harness.requests.lock().unwrap().len() == 2,
        "resumed harness invocation",
    )
    .await;
    let request = harness.requests.lock().unwrap()[1].clone();
    assert_eq!(request.resume.as_deref(), Some("hs-1"));
    assert!(request.prompt.contains("Screenshots"));
    assert!(request.prompt.contains("Keep it local"));
    assert!(input_present(&core, true));
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_async_delivery_stays_open_and_rpc_reports_the_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(MockHarness {
            script: mock_script(),
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();
    handle
        .doc()
        .push_message(&SessionMessageEntry {
            id: "old-assistant".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Input {
                id: "async-1".into(),
                request_id: "async-1".into(),
                questions: questions(),
                asynchronous: true,
                resolved: false,
            }],
            created_at: 1,
            device_id: core.device_id.clone(),
            status: Some(MessageStatus::Aborted),
            continuation_of: None,
            duration_ms: None,
            token_usage: None,
        })
        .unwrap();
    queue_as_viewer(handle.doc(), "answer-without-config", answer());
    wait_for(
        || {
            matches!(
                command_status(&core, "answer-without-config"),
                Some((SessionCommandStatus::Rejected, _))
            )
        },
        "delivery rejection",
    )
    .await;
    assert!(input_present(&core, false));
    assert!(
        !entries(&core)
            .iter()
            .any(|entry| entry.role == MessageRole::User)
    );
    let client = zeron_rpc::memory_client(core.rpc_service());
    let reply = client
        .call(
            zeron_rpc::methods::GET_SESSION_COMMAND,
            serde_json::json!({"chatId":CHAT, "commandId":"answer-without-config"}),
        )
        .await
        .unwrap();
    let command: Option<SessionCommandEntry> = serde_json::from_value(reply).unwrap();
    assert_eq!(command.unwrap().status, SessionCommandStatus::Rejected);
    assert!(
        client
            .call(
                zeron_rpc::methods::GET_SESSION_COMMAND,
                serde_json::json!({"chatId":CHAT, "commandId":"missing"})
            )
            .await
            .unwrap()
            .is_null()
    );
    core.workspace
        .create_chat(CHAT, None, Some(&core.device_id), None, Some("/tmp".into()))
        .unwrap();
    queue_as_viewer(handle.doc(), "retry-answer-with-config", answer());
    wait_for(
        || {
            matches!(
                command_status(&core, "retry-answer-with-config"),
                Some((SessionCommandStatus::Applied, _))
            )
        },
        "retry acceptance",
    )
    .await;
    assert!(input_present(&core, true));
    core.shutdown().await;
}
