use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use zeron_proto::ShellTaskStatus;

use super::wire::Frame;
use crate::shells::{ShellMonitor, command_label};

#[derive(Default)]
pub(super) struct Observer {
    tools: VecDeque<(String, String, String)>,
}

impl Observer {
    fn remember(&mut self, id: &str, command: &str) {
        if id.is_empty() {
            return;
        }
        if self.tools.iter().any(|(tool, _, _)| tool == id) {
            return;
        }
        if self.tools.len() == 128 {
            self.tools.pop_front();
        }
        self.tools
            .push_back((id.into(), command_label(command), id.into()));
    }

    pub(super) fn observe(&mut self, frame: &Frame, monitor: &ShellMonitor) {
        match frame {
            Frame::Assistant(message) => {
                for block in message.message.blocks() {
                    if block.kind == "tool_use" && block.name == "Bash" {
                        let command = block
                            .input
                            .get("command")
                            .and_then(Value::as_str)
                            .unwrap_or("Shell command");
                        self.remember(&block.id, command);
                        monitor.start(&block.id, command, false, false);
                    }
                }
            }
            Frame::User(message) => {
                for block in message
                    .message
                    .blocks()
                    .filter(|block| block.kind == "tool_result")
                {
                    let Some((_, command, id)) = self
                        .tools
                        .iter_mut()
                        .find(|(tool, _, _)| tool == &block.tool_use_id)
                    else {
                        continue;
                    };
                    if let Some(result) = message.tool_use_result.as_ref() {
                        if let Some(background) =
                            result.get("backgroundTaskId").and_then(Value::as_str)
                        {
                            monitor.rekey(id, background);
                            *id = background.into();
                            monitor.start(id, command, false, true);
                            monitor.append(
                                id,
                                result.get("stdout").and_then(Value::as_str).unwrap_or(""),
                            );
                            monitor.append(
                                id,
                                result.get("stderr").and_then(Value::as_str).unwrap_or(""),
                            );
                            if let Some(path) = result.get("outputFile").and_then(Value::as_str) {
                                monitor.output_file(id, path);
                            }
                            continue;
                        }
                        let stdout = result.get("stdout").and_then(Value::as_str).unwrap_or("");
                        let stderr = result.get("stderr").and_then(Value::as_str).unwrap_or("");
                        monitor.append(id, stdout);
                        monitor.append(id, stderr);
                    }
                    let text = message
                        .message
                        .content
                        .as_array()
                        .and_then(|blocks| {
                            blocks.iter().find(|value| {
                                value.get("tool_use_id").and_then(Value::as_str)
                                    == Some(block.tool_use_id.as_str())
                            })
                        })
                        .and_then(|value| value.get("content"))
                        .and_then(Value::as_str);
                    if message.tool_use_result.as_ref().is_none_or(|result| {
                        result.get("stdout").is_none() && result.get("stderr").is_none()
                    }) {
                        if let Some(text) = text {
                            monitor.append(id, text);
                        }
                    }
                    let result = message.tool_use_result.as_ref();
                    let exit = result
                        .and_then(|result| {
                            result.get("exitCode").or_else(|| result.get("exit_code"))
                        })
                        .and_then(Value::as_i64);
                    let interrupted = result
                        .and_then(|result| result.get("interrupted"))
                        .and_then(Value::as_bool)
                        == Some(true);
                    let status = if interrupted {
                        ShellTaskStatus::Stopped
                    } else if block.is_error == Some(true) || exit.is_some_and(|code| code != 0) {
                        ShellTaskStatus::Failed
                    } else {
                        ShellTaskStatus::Completed
                    };
                    monitor.finish(id, status, exit);
                }
            }
            Frame::System(system) => {
                if system.subtype == "background_tasks_changed" {
                    let Some(tasks) = &system.tasks else { return };
                    let mut ids = HashSet::new();
                    for task in tasks.iter().filter(|task| {
                        task.get("task_type").and_then(Value::as_str) == Some("local_bash")
                    }) {
                        let Some(id) = task.get("task_id").and_then(Value::as_str) else {
                            continue;
                        };
                        let command = task
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("Background shell");
                        monitor.start(id, command, true, true);
                        ids.insert(id.to_owned());
                        if let Some(path) = task.get("output_file").and_then(Value::as_str) {
                            monitor.output_file(id, path);
                        }
                    }
                    monitor.reconcile(&ids, false);
                } else if system.subtype == "task_started"
                    && system.task_type.as_deref() == Some("local_bash")
                {
                    if let Some(id) = &system.task_id {
                        let cached = system
                            .tool_use_id
                            .as_ref()
                            .and_then(|tool| self.tools.iter_mut().find(|(key, _, _)| key == tool));
                        let command = if let Some((_, command, key)) = cached {
                            monitor.rekey(key, id);
                            *key = id.clone();
                            command.as_str()
                        } else {
                            system.description.as_deref().unwrap_or("Background shell")
                        };
                        monitor.start(id, command, false, true);
                    }
                } else if system.subtype == "task_notification" {
                    if let Some(id) = &system.task_id
                        && monitor.contains(id)
                    {
                        if let Some(path) = &system.output_file {
                            monitor.output_file(id, path);
                        }
                        let status = match system.status.as_deref() {
                            Some("completed") => ShellTaskStatus::Completed,
                            Some("failed") => ShellTaskStatus::Failed,
                            Some("killed" | "stopped" | "cancelled") => ShellTaskStatus::Stopped,
                            _ => ShellTaskStatus::Unknown,
                        };
                        monitor.finish(id, status, None);
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::wire::parse_frame;
    use super::*;

    #[test]
    fn separates_shells_from_agents_and_keeps_terminal_outcome() {
        let monitor = ShellMonitor::default();
        let mut observer = Observer::default();
        for line in
            include_str!("../../tests/fixtures/claude/live-2.1.228-background-subagent.jsonl")
                .lines()
        {
            if let Ok(frame) = parse_frame(line) {
                observer.observe(&frame, &monitor);
            }
        }
        let tasks = monitor.snapshot().0;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "bfylmx121");
        assert_eq!(tasks[0].status, ShellTaskStatus::Completed);
    }

    #[test]
    fn foreground_result_captures_output_failure_and_interruption() {
        for (result, error, expected) in [
            (
                serde_json::json!({"stdout":"hello", "stderr":"", "exitCode":0}),
                false,
                ShellTaskStatus::Completed,
            ),
            (
                serde_json::json!({"stdout":"", "stderr":"failed", "exitCode":2}),
                true,
                ShellTaskStatus::Failed,
            ),
            (
                serde_json::json!({"stdout":"", "stderr":"", "interrupted":true}),
                false,
                ShellTaskStatus::Stopped,
            ),
        ] {
            let monitor = ShellMonitor::default();
            let mut observer = Observer::default();
            observer.observe(&parse_frame(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool-1","name":"Bash","input":{"command":"test"}}]}}"#).unwrap(), &monitor);
            let frame = serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","is_error":error}]},"tool_use_result":result});
            observer.observe(&parse_frame(&frame.to_string()).unwrap(), &monitor);
            assert_eq!(monitor.snapshot().0[0].status, expected);
            if expected == ShellTaskStatus::Completed {
                assert_eq!(monitor.output("tool-1").unwrap().text, "hello");
            }
        }
    }

    #[test]
    fn bash_yield_is_one_task_and_does_not_finish_at_turn_end() {
        let monitor = ShellMonitor::default();
        let mut observer = Observer::default();
        for line in [
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool-1","name":"Bash","input":{"command":"sleep 5"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","content":"Background"}]},"tool_use_result":{"backgroundTaskId":"bg1"}}"#,
            r#"{"type":"result","subtype":"success"}"#,
        ] {
            observer.observe(&parse_frame(line).unwrap(), &monitor);
        }
        let tasks = monitor.snapshot().0;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "bg1");
        assert_eq!(tasks[0].status, ShellTaskStatus::Running);
        observer.observe(&parse_frame(r#"{"type":"system","subtype":"task_notification","task_id":"bg1","status":"failed"}"#).unwrap(), &monitor);
        assert_eq!(monitor.snapshot().0[0].status, ShellTaskStatus::Failed);
    }
}
