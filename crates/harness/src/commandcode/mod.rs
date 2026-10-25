//! Command Code harness: drives `commandcode -p --output-format json`, one
//! process per message, continuing the chat with `--resume <session>`.
//!
//! Command Code has no ACP or RPC mode. Its print mode streams the agent
//! loop's events as NDJSON (`{type: "event", event}` lines, then a final
//! `{type: "result"}`), takes the prompt on stdin, and saves the session after
//! every model round, so a killed process loses at most the round in flight.
//!
//! - A runtime lives across turns; each message is a fresh process. A message
//!   sent while one runs waits for it to finish, then continues the same turn.
//! - Stopping a turn kills its process; the session stays resumable.
//! - Runs are unattended (`--yolo`, or `--permission-mode plan` read-only).
//!   Questions to the user are not available in print mode.
//! - `--effort` is model specific and only its rejection names the accepted
//!   values; the driver then retries with the nearest one and remembers it.
//! - Zeron's own MCP tools come in through a per-run mod (`mcp.mjs`, loaded
//!   with `--mod`), since Command Code reads MCP servers only from its config.
mod normalize;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use normalize::Normalizer;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::sync::mpsc;
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SteeringMode,
};

use crate::process::{ChildStdout, Command, Stdio, owned::Child};
use crate::{Harness, HarnessError, RunControls, SteerMessage};

const NAME: &str = "Command Code";
/// Rounds one message may run before Command Code stops it (its default is 100).
const MAX_TURNS: &str = "500";

/// Accepted efforts learned per model from rejected runs.
type Efforts = Arc<Mutex<HashMap<String, Vec<String>>>>;

pub struct CommandCodeHarness {
    executable: Option<PathBuf>,
    models_cache: crate::catalog::Catalog,
    efforts: Efforts,
    kill_grace: Duration,
}

impl Default for CommandCodeHarness {
    fn default() -> Self {
        Self {
            executable: None,
            models_cache: Default::default(),
            efforts: Default::default(),
            kill_grace: Duration::from_secs(3),
        }
    }
}

fn install_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = crate::executable::home_dir() {
        for dir in [".npm-global/bin", ".local/bin", ".bun/bin"] {
            paths.push(home.join(dir).join("commandcode"));
        }
    }
    paths.push(PathBuf::from("/opt/homebrew/bin/commandcode"));
    paths.push(PathBuf::from("/usr/local/bin/commandcode"));
    paths
}

impl CommandCodeHarness {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    /// The `commandcode` alias of the `command-code` npm package: its `cmd`
    /// alias would resolve to `cmd.exe` on Windows.
    pub fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(path) = self
            .executable
            .clone()
            .or_else(|| std::env::var_os("COMMANDCODE_EXECUTABLE").map(PathBuf::from))
        {
            return crate::executable::validate_native_override(&path);
        }
        crate::executable::find_on_paths("commandcode", install_paths()).ok_or_else(|| {
            HarnessError::NotInstalled(
                "Command Code CLI: install it with `npm install -g command-code` and sign in \
                 once with `commandcode login`, or set COMMANDCODE_EXECUTABLE"
                    .into(),
            )
        })
    }

    fn command(&self, cwd: &Path) -> Result<Command, HarnessError> {
        let exe = self.resolve_executable()?;
        let mut cmd = Command::new(&exe);
        crate::process::owned::configure(&mut cmd);
        crate::compose_child_path(&mut cmd, &exe);
        cmd.current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        Ok(cmd)
    }

    async fn list_models(&self) -> Result<Vec<Model>, HarnessError> {
        let mut cmd = self.command(&std::env::current_dir()?)?;
        cmd.args(["--list-models", "--no-auto-update"])
            .stdin(Stdio::null());
        let output = tokio::time::timeout(Duration::from_secs(60), cmd.output())
            .await
            .map_err(|_| HarnessError::Protocol("Command Code model listing timed out".into()))??;
        let models = normalize::parse_models(&String::from_utf8_lossy(&output.stdout));
        if models.is_empty() {
            return Err(HarnessError::Protocol(format!(
                "Command Code listed no models: {}",
                crate::redact_secrets(String::from_utf8_lossy(&output.stderr).trim())
            )));
        }
        let mut all = self.fallback_models();
        all.extend(models);
        Ok(all)
    }
}

#[async_trait]
impl Harness for CommandCodeHarness {
    fn id(&self) -> HarnessId {
        HarnessId::CommandCode
    }
    fn display_name(&self) -> &str {
        NAME
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }
    fn installed(&self) -> bool {
        self.resolve_executable().is_ok()
    }
    fn executable_path(&self) -> Option<PathBuf> {
        self.resolve_executable().ok()
    }
    fn deterministic_turn_end(&self) -> bool {
        true
    }
    fn stops_turn_in_place(&self) -> bool {
        true
    }
    /// Every message starts its own process, which reads the whole config.
    fn reconfigures_in_place(&self, live: &RunRequest, next: &RunRequest) -> bool {
        live.cwd == next.cwd
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(self.model_catalog(false).await?.models)
    }
    fn model_context(&self) -> Result<Option<crate::ModelContext>, HarnessError> {
        crate::model_context::context(HarnessId::CommandCode, &self.resolve_executable()?, &[])
            .map(Some)
    }
    async fn model_catalog(&self, force: bool) -> Result<crate::ModelCatalog, HarnessError> {
        self.models_cache
            .get_with_timeout(
                force,
                Duration::from_secs(65),
                || {
                    Ok(self
                        .model_context()?
                        .expect("Command Code model context")
                        .key())
                },
                || self.list_models(),
            )
            .await
    }
    fn fallback_models(&self) -> Vec<Model> {
        vec![Model {
            id: "default".into(),
            label: "Command Code default".into(),
            description: Some("The model selected in Command Code".into()),
            reasoning_levels: vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::Max,
            ],
            options: Vec::new(),
        }]
    }
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        // Fail fast (not installed) before the stream opens, like the others.
        self.resolve_executable()?;
        let (tx, rx) = mpsc::channel(256);
        let mut runner = Runner {
            spawner: Spawner {
                harness: CommandCodeHarness {
                    executable: self.executable.clone(),
                    models_cache: Default::default(),
                    efforts: self.efforts.clone(),
                    kill_grace: self.kill_grace,
                },
            },
            session: request.resume.clone(),
            request,
            tx,
            norm: Normalizer::default(),
            assistant: uuid::Uuid::new_v4().to_string(),
            started: false,
            turn: None,
            queued: VecDeque::new(),
        };
        tokio::spawn(async move {
            let RunControls {
                realtime: _,
                execution_lease: _lease,
                request_input: _,
                steering,
                interrupt,
                turn,
            } = controls;
            runner.run(steering, interrupt, turn).await;
        });
        Ok(
            futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) })
                .boxed(),
        )
    }
}

/// What spawns each message's process; a cut-down harness for the runner task.
struct Spawner {
    harness: CommandCodeHarness,
}

/// One message's `commandcode` process.
struct Process {
    child: Child,
    lines: Lines<BufReader<ChildStdout>>,
    tail: crate::StderrTail,
    stderr_task: tokio::task::JoinHandle<()>,
    /// Holds the Zeron MCP mod for the process's lifetime.
    _scratch: Option<crate::scratch::ScratchDir>,
    /// Non-JSON stdout (preflight errors print plain text).
    plain: String,
    /// What it was started with, to retry with a narrower config.
    prompt: String,
    effort: Option<String>,
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stderr_task.abort();
    }
}

impl Spawner {
    fn spawn(
        &self,
        request: &RunRequest,
        session: Option<&str>,
        prompt: &str,
        effort: Option<String>,
    ) -> Result<Process, HarnessError> {
        let mut cmd = self.harness.command(Path::new(&request.cwd))?;
        cmd.args(["-p", "--output-format", "json", "--max-turns", MAX_TURNS])
            .args(["--skip-onboarding", "--trust", "--no-auto-update"]);
        if request.sandbox == SandboxLevel::ReadOnly {
            cmd.args(["--permission-mode", "plan"]);
        } else {
            cmd.arg("--yolo");
        }
        if let Some(model) = request.model.as_deref().filter(|m| *m != "default") {
            cmd.args(["--model", model]);
        }
        if let Some(effort) = &effort {
            cmd.args(["--effort", effort]);
        }
        if let Some(session) = session {
            cmd.args(["--resume", session]);
        }
        let scratch = match &request.mcp {
            Some(config) => {
                let scratch = crate::scratch::ScratchDir::new("commandcode-mcp")?;
                let module = scratch.path().join("zeron-mcp.mjs");
                std::fs::write(&module, include_str!("mcp.mjs"))?;
                cmd.arg("--mod").arg(&module).env(
                    "ZERON_COMMANDCODE_MCP",
                    serde_json::to_string(config).expect("serializable MCP config"),
                );
                Some(scratch)
            }
            None => None,
        };
        let mut child = Child::new(cmd.spawn()?);
        let mut stdin = child.stdin.take().expect("piped stdin");
        let text = prompt.to_owned();
        tokio::spawn(async move {
            let _ = stdin.write_all(text.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
        let tail = crate::StderrTail::default();
        let mut errors = BufReader::new(child.stderr.take().expect("piped stderr")).lines();
        let stderr = tail.clone();
        let stderr_task = tokio::spawn(async move {
            while let Ok(Some(line)) = errors.next_line().await {
                stderr.push(&line);
            }
            stderr.close();
        });
        Ok(Process {
            lines: BufReader::new(child.stdout.take().expect("piped stdout")).lines(),
            child,
            tail,
            stderr_task,
            _scratch: scratch,
            plain: String::new(),
            prompt: prompt.to_owned(),
            effort,
        })
    }

    /// The `--effort` for a run: the level's own, or the nearest the model
    /// was found to accept.
    fn effort(&self, request: &RunRequest) -> Option<String> {
        let wanted = normalize::effort(request.reasoning?);
        let model = request.model.clone().unwrap_or_default();
        let known = self.harness.efforts.lock().ok()?.get(&model).cloned();
        match known {
            Some(supported) => normalize::nearest(wanted, &supported),
            None => Some(wanted.to_owned()),
        }
    }

    fn learn_efforts(&self, request: &RunRequest, supported: Vec<String>) {
        if let Ok(mut efforts) = self.harness.efforts.lock() {
            efforts.insert(request.model.clone().unwrap_or_default(), supported);
        }
    }
}

struct Runner {
    spawner: Spawner,
    request: RunRequest,
    session: Option<String>,
    tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    norm: Normalizer,
    assistant: String,
    started: bool,
    turn: Option<Process>,
    queued: VecDeque<SteerMessage>,
}

impl Runner {
    async fn emit(&self, event: AgentEvent) -> bool {
        self.tx.send(Ok(event)).await.is_ok()
    }

    async fn run(
        &mut self,
        mut steering: mpsc::Receiver<SteerMessage>,
        interrupt: tokio_util::sync::CancellationToken,
        control: crate::TurnControl,
    ) {
        let prompt = self.request.prompt.clone();
        if !self.start(&prompt).await {
            return;
        }
        let consumer = self.tx.clone();
        let mut open = true;
        loop {
            if self.turn.is_none() && !open && self.queued.is_empty() {
                break;
            }
            tokio::select! {
                line = async {
                    match self.turn.as_mut() {
                        Some(process) => process.lines.next_line().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let alive = match line {
                        Ok(Some(line)) => self.line(&line).await,
                        // End of output: the process is finishing.
                        _ => self.exited().await,
                    };
                    if !alive {
                        break;
                    }
                }
                steer = steering.recv(), if open => match steer {
                    Some(steer) if self.turn.is_some() => self.queued.push_back(steer),
                    Some(steer) => {
                        if !self.next(steer).await {
                            break;
                        }
                    }
                    None => open = false,
                },
                _ = control.stop_requested() => {
                    self.queued.clear();
                    self.kill().await;
                    self.norm.interrupted = true;
                    if !self.finish().await {
                        break;
                    }
                }
                _ = interrupt.cancelled() => {
                    self.queued.clear();
                    let running = self.turn.is_some();
                    self.kill().await;
                    if running {
                        self.norm.interrupted = true;
                        self.finish().await;
                    }
                    break;
                }
                _ = consumer.closed() => break,
            }
        }
        self.kill().await;
    }

    /// Start a process for `prompt`; on a spawn failure the turn ends errored.
    async fn start(&mut self, prompt: &str) -> bool {
        let effort = self.spawner.effort(&self.request);
        self.launch(prompt, effort).await
    }

    async fn launch(&mut self, prompt: &str, effort: Option<String>) -> bool {
        match self
            .spawner
            .spawn(&self.request, self.session.as_deref(), prompt, effort)
        {
            Ok(process) => {
                self.turn = Some(process);
                true
            }
            Err(error) => {
                self.norm.error = Some(error.to_string());
                self.finish().await
            }
        }
    }

    /// The next queued or newly sent message, as a continuation of the turn.
    async fn next(&mut self, steer: SteerMessage) -> bool {
        let previous = std::mem::replace(&mut self.assistant, uuid::Uuid::new_v4().to_string());
        if !self
            .emit(AgentEvent::Steered {
                assistant_message_id: Some(previous),
                next_assistant_message_id: Some(self.assistant.clone()),
            })
            .await
        {
            return false;
        }
        if let Some(config) = steer.config {
            self.request = *config;
        }
        self.start(&steer.prompt).await
    }

    async fn line(&mut self, line: &str) -> bool {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            if let Some(process) = self.turn.as_mut() {
                process.plain.push_str(line);
                process.plain.push('\n');
            }
            return true;
        };
        let events = self.norm.map(&value, Instant::now());
        if let Some(session) = self.norm.session.clone() {
            self.session = Some(session.clone());
            if !self.started {
                self.started = true;
                let model = self
                    .request
                    .model
                    .clone()
                    .unwrap_or_else(|| "default".into());
                if !self
                    .emit(AgentEvent::SessionStarted {
                        harness: HarnessId::CommandCode,
                        model,
                        tools: Vec::new(),
                        cwd: self.request.cwd.clone(),
                        session_id: session,
                        assistant_message_id: self.assistant.clone(),
                    })
                    .await
                {
                    return false;
                }
            }
        }
        for event in events {
            if !self.emit(event).await {
                return false;
            }
        }
        true
    }

    /// The process closed its output: retry a rejected config, chain the next
    /// queued message, or end the turn.
    async fn exited(&mut self) -> bool {
        let Some(mut process) = self.turn.take() else {
            return true;
        };
        let status = tokio::time::timeout(Duration::from_secs(5), process.child.wait())
            .await
            .ok()
            .and_then(Result::ok);
        process
            .child
            .shutdown(self.spawner.harness.kill_grace)
            .await;
        // Rejected before it began (no `run_start`): a model-specific
        // effort, or a session to resume that no longer exists.
        if self.norm.session.is_none() {
            if let Some(supported) = normalize::unsupported_effort(&process.plain).or_else(|| {
                process
                    .tail
                    .snapshot()
                    .as_deref()
                    .and_then(normalize::unsupported_effort)
            }) {
                self.spawner.learn_efforts(&self.request, supported.clone());
                let wanted = self
                    .request
                    .reasoning
                    .map(normalize::effort)
                    .unwrap_or("high");
                let effort = normalize::nearest(wanted, &supported);
                if effort != process.effort {
                    self.norm.error = None;
                    self.norm.settled = false;
                    return self.launch(&process.prompt, effort).await;
                }
            }
            // A session Command Code no longer has starts over, visibly.
            if let (Some(session), Some(error)) = (self.session.clone(), self.norm.error.clone()) {
                if normalize::missing_session(&error) {
                    let message = format!(
                        "{NAME} could not restore session {session}; starting a new session \
                         without the previous context: {error}"
                    );
                    if !self.emit(AgentEvent::Error { message }).await {
                        return false;
                    }
                    self.session = None;
                    self.norm.error = None;
                    self.norm.settled = false;
                    let effort = process.effort.clone();
                    return self.launch(&process.prompt, effort).await;
                }
            }
        }
        if !self.norm.settled && !self.norm.interrupted && self.norm.error.is_none() {
            let mut message = crate::crash_message(NAME, status, &process.tail);
            let plain = process.plain.trim();
            if !plain.is_empty() {
                message = format!("{message}\n{}", crate::redact_secrets(plain));
            }
            self.norm.error = Some(message);
        }
        self.norm.session = None;
        self.norm.settled = false;
        // Messages sent meanwhile continue this turn once it ran cleanly.
        if self.norm.status() == DoneStatus::Completed {
            if let Some(steer) = self.queued.pop_front() {
                return self.next(steer).await;
            }
        }
        self.queued.clear();
        self.finish().await
    }

    /// End the turn: its usage, then `Done`.
    async fn finish(&mut self) -> bool {
        let usage = self.norm.usage;
        if usage != Default::default() && !self.emit(AgentEvent::TurnUsage { usage }).await {
            return false;
        }
        let done = AgentEvent::Done {
            status: self.norm.status(),
            result: None,
            error: self.norm.error.clone(),
            session_id: self.session.clone(),
        };
        self.norm.reset();
        self.emit(done).await
    }

    async fn kill(&mut self) {
        if let Some(mut process) = self.turn.take() {
            process
                .child
                .shutdown(self.spawner.harness.kill_grace)
                .await;
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use zeron_proto::ToolCall;

    fn controls() -> (RunControls, mpsc::Sender<SteerMessage>) {
        let (steer_tx, steering) = mpsc::channel(8);
        let controls = RunControls {
            realtime: None,
            execution_lease: None,
            request_input: Box::new(|_| tokio::sync::oneshot::channel().1),
            steering,
            interrupt: Default::default(),
            turn: Default::default(),
        };
        (controls, steer_tx)
    }

    fn request(prompt: &str, cwd: &Path, reasoning: Option<ReasoningLevel>) -> RunRequest {
        let mut request: RunRequest = serde_json::from_value(serde_json::json!({
            "prompt": prompt,
            "cwd": cwd.to_string_lossy(),
            "sandbox": "workspace-write",
            "model": "test/model",
        }))
        .unwrap();
        request.reasoning = reasoning;
        request
    }

    /// Events up to and including the next `Done`.
    async fn turn(
        stream: &mut BoxStream<'static, Result<AgentEvent, HarnessError>>,
    ) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(20), stream.next())
                .await
                .expect("event in time")
                .expect("stream open")
                .unwrap();
            let done = matches!(event, AgentEvent::Done { .. });
            events.push(event);
            if done {
                return events;
            }
        }
    }

    /// A fake `commandcode`: rejects `--effort low` like a model taking only
    /// off/high/max, records its arguments, and answers with a tool call.
    fn fake(dir: &Path) -> PathBuf {
        let script = dir.join("commandcode");
        std::fs::write(
            &script,
            r#"#!/bin/sh
echo "$@" >> "$(dirname "$0")/args"
case " $* " in *" --effort low "*) echo 'Unknown effort "low". Supported: off, high, max.'; exit 1;; esac
prompt=$(cat)
session=s1
case " $* " in *" --resume missing "*) echo '{"type":"result","subtype":"error","error":"Error: No session \"missing\" found to resume."}'; exit 0;; esac
echo '{"type":"event","event":{"type":"run_start","sessionId":"'$session'"}}'
echo '{"type":"event","event":{"type":"model_request_start","model":"test/model"}}'
echo '{"type":"event","event":{"type":"tool_queued","toolCallId":"c1","toolName":"shell_command","input":{"command":"true"}}}'
echo '{"type":"event","event":{"type":"tool_completed","toolCallId":"c1","result":[{"type":"text","text":"ok"}]}}'
echo '{"type":"event","event":{"type":"text_delta","delta":"you said '"$prompt"'"}}'
echo '{"type":"event","event":{"type":"model_request_end","usage":{"inputTokens":10,"outputTokens":2,"cacheReadTokens":4,"cacheWriteTokens":0}}}'
case "$prompt" in slow*) sleep 30;; esac
echo '{"type":"result","subtype":"success","stopReason":"end_turn","sessionId":"'$session'"}'
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[tokio::test]
    async fn turns_resume_narrow_effort_and_stop_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let harness = CommandCodeHarness::new().with_executable(fake(dir.path()));
        let (controls, steer) = controls();
        let stop = controls.turn.clone();
        let mut request = request("hello", dir.path(), Some(ReasoningLevel::Low));
        request.resume = Some("missing".into());
        let mut stream = harness.run(request, controls).await.unwrap();

        let first = turn(&mut stream).await;
        assert!(first.iter().any(|e| matches!(e, AgentEvent::Error { message } if message.contains("could not restore session missing"))));
        assert!(first.iter().any(
            |e| matches!(e, AgentEvent::SessionStarted { session_id, .. } if session_id == "s1")
        ));
        assert!(first.iter().any(|e| matches!(e, AgentEvent::ToolCall { call: ToolCall::Exec { command }, .. } if command == "true")));
        assert!(
            first
                .iter()
                .any(|e| matches!(e, AgentEvent::TextDelta { text } if text == "you said hello"))
        );
        assert!(first.iter().any(
            |e| matches!(e, AgentEvent::TurnUsage { usage } if usage.input_tokens == Some(10))
        ));
        assert!(
            matches!(first.last(), Some(AgentEvent::Done { status: DoneStatus::Completed, session_id: Some(s), .. }) if s == "s1")
        );

        steer.send(SteerMessage::text("again")).await.unwrap();
        let second = turn(&mut stream).await;
        assert!(matches!(second[0], AgentEvent::Steered { .. }));
        assert!(matches!(
            second.last(),
            Some(AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            })
        ));

        steer.send(SteerMessage::text("slow")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        stop.stop_turn();
        let third = turn(&mut stream).await;
        assert!(matches!(
            third.last(),
            Some(AgentEvent::Done {
                status: DoneStatus::Interrupted,
                ..
            })
        ));

        drop(steer);
        assert!(
            tokio::time::timeout(Duration::from_secs(10), stream.next())
                .await
                .unwrap()
                .is_none()
        );
        let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
        let runs: Vec<&str> = args.lines().collect();
        // Rejected `low`, then the missing session at the nearest effort, a
        // fresh session, and two resumed messages, all at `high`.
        assert!(runs[0].contains("--effort low") && runs[0].contains("--resume missing"));
        assert!(runs[1].contains("--effort high") && runs[1].contains("--resume missing"));
        assert!(runs[2].contains("--effort high") && !runs[2].contains("--resume"));
        assert!(
            runs[3..]
                .iter()
                .all(|r| r.contains("--resume s1") && r.contains("--effort high"))
        );
        assert!(
            runs.iter()
                .all(|r| r.contains("--yolo") && r.contains("--model test/model"))
        );
    }

    /// Against the installed CLI and a signed-in account:
    /// `cargo test -p zeron-harness commandcode::tests::live -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live() {
        let dir = tempfile::tempdir().unwrap();
        let harness = CommandCodeHarness::new();
        let (controls, steer) = controls();
        let mut request = request(
            "Remember the code word ZEBRA-7. Reply only OK.",
            dir.path(),
            Some(ReasoningLevel::Low),
        );
        request.model = Some("deepseek/deepseek-v4-flash".into());
        let mut stream = harness.run(request, controls).await.unwrap();
        let first = turn(&mut stream).await;
        println!("{first:#?}");
        assert!(matches!(
            first.last(),
            Some(AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            })
        ));
        steer
            .send(SteerMessage::text(
                "What was the code word? Reply with it only.",
            ))
            .await
            .unwrap();
        let second = turn(&mut stream).await;
        println!("{second:#?}");
        let text: String = second
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains("ZEBRA-7"), "{text}");
    }
}
