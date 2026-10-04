//! Credential-free Claude steering regressions against the shared fake CLI.

#![cfg(unix)]

use std::{path::PathBuf, sync::Arc, time::Duration};

use zeron_doc::{MessagePart, MessageRole, SessionCommandPayload, SessionMessageEntry};
use zeron_engine::{EngineCore, HarnessRegistry, JournaledEvent};
use zeron_harness::ClaudeHarness;
use zeron_proto::{AgentEvent, ChatConfig, HarnessId, RunRequest, SandboxLevel, SessionStatus};

const CHAT: &str = "claude-quiet-steer";

async fn wait_for(mut ready: impl FnMut() -> bool, label: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label}"));
}

fn entries(core: &EngineCore) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(CHAT)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap()
}

fn events(core: &EngineCore) -> Vec<JournaledEvent> {
    core.sessions.subscribe(CHAT, 0).unwrap().0
}

fn has_text(entry: &SessionMessageEntry, needle: &str) -> bool {
    entry
        .parts
        .iter()
        .any(|part| matches!(part, MessagePart::Text { text, .. } if text.contains(needle)))
}

#[derive(Debug)]
struct QuietSteerObservation {
    status_before_release: Option<SessionStatus>,
    dones_before_release: usize,
    queued_before_release: usize,
    same_response_entry: bool,
    tool_resolved: bool,
    processes: usize,
}

async fn observe_quiet_steer(scenario: &str, queue_follow_up: bool) -> QuietSteerObservation {
    let dir = tempfile::tempdir().unwrap();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../harness/tests/fixtures/fake-claude.sh");
    let harness = ClaudeHarness::new()
        .with_executable(fixture)
        .with_graces(Duration::from_millis(50), Duration::from_millis(100));
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(harness));
    let core = EngineCore::assemble(
        &dir.path().join("engine"),
        Arc::new(registry),
        HarnessId::ClaudeCode,
        None,
    )
    .unwrap();
    let cwd = dir.path().to_str().unwrap().to_owned();
    core.workspace
        .create_space(CHAT, &core.device_id, &cwd, None, false)
        .unwrap();
    core.workspace
        .create_chat(CHAT, Some(CHAT), None, None, None)
        .unwrap();
    // Avoid unrelated automatic title-generation runs.
    core.workspace
        .rename_chat(CHAT, "Steering regression")
        .unwrap();
    core.workspace
        .set_chat_config(
            CHAT,
            &ChatConfig {
                harness: HarnessId::ClaudeCode,
                model: None,
                reasoning: None,
                model_options: Default::default(),
                sandbox: SandboxLevel::DangerFullAccess,
            },
        )
        .unwrap();
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                message_id: "opening".into(),
                request: RunRequest {
                    mcp: None,
                    prompt: scenario.into(),
                    harness: Some(HarnessId::ClaudeCode),
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd,
                    sandbox: SandboxLevel::DangerFullAccess,
                    auto_approve: true,
                    resume: None,
                    attachments: Vec::new(),
                    worktree: None,
                },
            },
        )
        .unwrap();
    wait_for(
        || {
            entries(&core)
                .iter()
                .any(|entry| has_text(entry, "old response"))
        },
        "the opening response",
    )
    .await;

    // Exercise the actual UI path: queue text, then activate its Steer action.
    let steer = core
        .doc_host
        .queue_message_with_behavior(CHAT, "redirect please", Vec::new(), true)
        .unwrap();
    assert!(core.doc_host.steer_queued_now(CHAT, &steer).await.unwrap());
    wait_for(
        || {
            entries(&core).iter().any(|entry| {
                entry
                    .parts
                    .iter()
                    .any(|part| matches!(part, MessagePart::Tool { id, .. } if id == "quiet-tool"))
            })
        },
        "the steered tool",
    )
    .await;
    assert_eq!(
        core.sessions
            .session_status(CHAT)
            .map(|session| session.status),
        Some(SessionStatus::Working)
    );
    if queue_follow_up {
        core.doc_host
            .queue_message_with_behavior(CHAT, "ordinary follow-up", Vec::new(), true)
            .unwrap();
    }

    tokio::time::sleep(Duration::from_secs(7)).await;
    let status_before_release = core
        .sessions
        .session_status(CHAT)
        .map(|session| session.status);
    let dones_before_release = events(&core)
        .iter()
        .filter(|event| matches!(&event.event, AgentEvent::Done { .. }))
        .count();
    let queued_before_release = core
        .doc_host
        .open(CHAT)
        .unwrap()
        .doc()
        .read_queue()
        .unwrap()
        .len();
    std::fs::write(dir.path().join("release-quiet-tool"), "").unwrap();
    wait_for(
        || {
            let events = events(&core);
            let done = |result: &str| {
                events.iter().any(|event| {
                    matches!(
                        &event.event,
                        AgentEvent::Done { result: Some(text), .. } if text == result
                    )
                })
            };
            let final_result = if queue_follow_up {
                "queued-finished"
            } else {
                "steered-finished"
            };
            done(final_result)
                && entries(&core)
                    .iter()
                    .any(|entry| has_text(entry, "after quiet tool"))
                && core
                    .sessions
                    .session_status(CHAT)
                    .is_some_and(|session| session.status == SessionStatus::Idle)
        },
        "the real completion frames",
    )
    .await;

    let entries = entries(&core);
    let same_response_entry = entries.iter().any(|entry| {
        entry.role == MessageRole::Assistant
            && has_text(entry, "before quiet tool")
            && has_text(entry, "after quiet tool")
    });
    let tool_resolved = entries.iter().any(|entry| {
        entry.parts.iter().any(|part| {
            matches!(
                part,
                MessagePart::Tool { id, resolved: true, .. } if id == "quiet-tool"
            )
        })
    });
    let processes = std::fs::read_to_string(dir.path().join("quiet-steer-processes"))
        .unwrap()
        .lines()
        .count();
    core.shutdown().await;
    QuietSteerObservation {
        status_before_release,
        dones_before_release,
        queued_before_release,
        same_response_entry,
        tool_resolved,
        processes,
    }
}

#[tokio::test]
async fn a_confirmed_claude_steer_stays_working_and_keeps_one_response_entry() {
    let observed = observe_quiet_steer("scenario:quiet-steer-old-result", false).await;
    println!("confirmed steer: {observed:#?}");
    assert_eq!(observed.status_before_release, Some(SessionStatus::Working));
    assert_eq!(observed.dones_before_release, 0);
    assert!(observed.same_response_entry);
    assert!(observed.tool_resolved);
    assert_eq!(observed.processes, 1);
}

#[tokio::test]
async fn a_confirmed_claude_steer_does_not_drain_the_ordinary_queue_early() {
    let observed = observe_quiet_steer("scenario:quiet-steer-old-result", true).await;
    println!("confirmed steer with queue: {observed:#?}");
    assert_eq!(
        observed.queued_before_release, 1,
        "follow-up must wait for the real turn end"
    );
    assert_eq!(observed.dones_before_release, 0);
    assert_eq!(observed.processes, 1);
}

#[tokio::test]
async fn a_quiet_claude_steer_without_an_old_result_keeps_the_queue_held() {
    let observed = observe_quiet_steer("scenario:quiet-steer-no-old-result", true).await;
    println!("control without old result: {observed:#?}");
    assert_eq!(observed.status_before_release, Some(SessionStatus::Working));
    assert_eq!(observed.dones_before_release, 0);
    assert_eq!(observed.queued_before_release, 1);
    assert!(observed.same_response_entry);
    assert!(observed.tool_resolved);
    assert_eq!(observed.processes, 1);
}
