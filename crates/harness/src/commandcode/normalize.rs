//! Command Code's print-mode JSON events mapped onto Zeron's. One `Normalizer`
//! spans a Zeron turn, which may chain several `commandcode` processes when
//! messages queue up while it works.
use std::collections::HashMap;
use std::time::Instant;

use serde_json::Value;
use zeron_proto::{
    AgentEvent, DoneStatus, Model, ReasoningLevel, TodoItem, TodoStatus, TokenUsage, ToolCall,
    ToolDiff,
};

use crate::acp::normalize::{DIFF_TEXT_CAP, OUTPUT_CAP, cap_text};
use crate::generation::{self, StreamWindow};

fn string<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}

fn opt_string(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().map(str::to_owned)
}

#[derive(Default)]
pub(super) struct Normalizer {
    /// Edit/write inputs by call id, for the result's inline diff.
    tools: HashMap<String, (String, Value)>,
    /// The streaming window of the model request in flight.
    window: Option<StreamWindow>,
    requested: bool,
    pub usage: TokenUsage,
    pub error: Option<String>,
    pub interrupted: bool,
    /// The session the current process reported (`run_start`).
    pub session: Option<String>,
    /// The process's final `result` frame arrived.
    pub settled: bool,
}

impl Normalizer {
    /// Ready for the next turn.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn status(&self) -> DoneStatus {
        if self.interrupted {
            DoneStatus::Interrupted
        } else if self.error.is_some() {
            DoneStatus::Errored
        } else {
            DoneStatus::Completed
        }
    }

    pub fn map(&mut self, line: &Value, now: Instant) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        if line["type"] == "result" {
            self.settled = true;
            if line["subtype"] != "success" {
                self.error = Some(
                    line["error"]
                        .as_str()
                        .map(|e| e.trim_start_matches("Error: ").to_owned())
                        .unwrap_or_else(|| "Command Code failed".into()),
                );
            }
            match string(line, "stopReason") {
                "interrupted" => self.interrupted = true,
                "max_turns" => events.push(AgentEvent::Error {
                    message: "Command Code stopped at its turn limit for one message.".into(),
                }),
                _ => {}
            }
            return events;
        }
        let event = &line["event"];
        match string(event, "type") {
            "run_start" => self.session = opt_string(event, "sessionId"),
            "model_request_start" => {
                self.requested = true;
                self.window = None;
            }
            "text_delta" | "thinking_delta" => {
                let text = string(event, "delta");
                if !text.is_empty() {
                    if self.requested {
                        self.window
                            .get_or_insert_with(|| StreamWindow::open(now))
                            .chunk();
                    }
                    events.push(if event["type"] == "text_delta" {
                        AgentEvent::TextDelta { text: text.into() }
                    } else {
                        AgentEvent::ReasoningDelta { text: text.into() }
                    });
                }
            }
            "model_request_end" => {
                self.requested = false;
                let usage = &event["usage"];
                let n = |key: &str| usage[key].as_u64();
                // inputTokens already counts cached reads and writes.
                let (input, output) = (n("inputTokens"), n("outputTokens"));
                let add = |total: &mut Option<u64>, value: Option<u64>| {
                    if let Some(value) = value {
                        *total = Some(total.unwrap_or(0).saturating_add(value));
                    }
                };
                add(&mut self.usage.input_tokens, input);
                add(&mut self.usage.output_tokens, output);
                add(&mut self.usage.cached_input_tokens, n("cacheReadTokens"));
                add(
                    &mut self.usage.cache_write_input_tokens,
                    n("cacheWriteTokens"),
                );
                if let (Some(window), Some(output)) = (self.window.take(), output) {
                    if let Some(elapsed) = window.measured(now) {
                        self.usage.generation =
                            generation::accumulate(self.usage.generation, output, elapsed);
                    }
                }
                if let Some(input) = input {
                    events.push(AgentEvent::ContextUsage {
                        tokens: Some(input + output.unwrap_or(0)),
                        window: None,
                    });
                }
            }
            "tool_queued" => {
                let id = string(event, "toolCallId").to_owned();
                let name = string(event, "toolName");
                let input = event["input"].clone();
                events.push(AgentEvent::ToolCall {
                    id: id.clone(),
                    call: tool(name, &input),
                });
                self.tools.insert(id, (name.into(), input));
            }
            "tool_completed" | "tool_errored" | "tool_denied" | "tool_hook_blocked" => {
                let id = string(event, "toolCallId").to_owned();
                let info = self.tools.remove(&id);
                let is_error = event["type"] != "tool_completed";
                let output = match string(event, "type") {
                    "tool_completed" => content_text(&event["result"]),
                    "tool_errored" => content_text(&event["error"])
                        .or_else(|| event["error"]["message"].as_str().map(str::to_owned)),
                    "tool_denied" => Some("Permission denied.".into()),
                    _ => opt_string(event, "hookOutput"),
                };
                let diff = info
                    .filter(|_| !is_error)
                    .and_then(|(name, input)| diff(&name, &input));
                events.push(AgentEvent::ToolResult {
                    id,
                    is_error,
                    output: output.map(|text| cap_text(&text, OUTPUT_CAP)),
                    diff,
                });
            }
            "run_error" => {
                let error = &event["error"];
                let message = error["message"]
                    .as_str()
                    .or_else(|| error.as_str())
                    .unwrap_or("Command Code failed")
                    .to_owned();
                self.error = Some(message);
            }
            "interrupted" => self.interrupted = true,
            _ => {}
        }
        events
    }
}

/// Text of a tool result: a string, or `[{type: "text", text}]` blocks.
fn content_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_owned());
    }
    let parts: Vec<&str> = value
        .as_array()?
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn diff(name: &str, input: &Value) -> Option<ToolDiff> {
    let path = string(input, "file_path").to_owned();
    match name {
        "edit_file" => Some(ToolDiff {
            path,
            old_text: Some(cap_text(string(input, "old_string"), DIFF_TEXT_CAP)),
            new_text: cap_text(string(input, "new_string"), DIFF_TEXT_CAP),
        }),
        "write_file" => Some(ToolDiff {
            path,
            old_text: None,
            new_text: cap_text(string(input, "content"), DIFF_TEXT_CAP),
        }),
        _ => None,
    }
}

fn tool(name: &str, input: &Value) -> ToolCall {
    match name {
        "shell_command" | "run_command" | "powershell" => ToolCall::Exec {
            command: string(input, "command").into(),
        },
        "read_file" => ToolCall::ReadFile {
            path: string(input, "file_path").into(),
        },
        "write_file" => ToolCall::WriteFile {
            path: string(input, "file_path").into(),
            content: opt_string(input, "content"),
        },
        "edit_file" => ToolCall::EditFile {
            path: string(input, "file_path").into(),
            old_string: opt_string(input, "old_string"),
            new_string: opt_string(input, "new_string"),
        },
        "grep" => ToolCall::Search {
            pattern: string(input, "pattern").into(),
            path: opt_string(input, "path"),
        },
        "glob" => ToolCall::Glob {
            pattern: string(input, "pattern").into(),
        },
        "web_fetch" => ToolCall::WebFetch {
            url: string(input, "url").into(),
            prompt: opt_string(input, "prompt"),
        },
        "web_search" => ToolCall::WebSearch {
            query: string(input, "query").into(),
        },
        "todo_write" if input["todos"].is_array() => ToolCall::Todo {
            items: input["todos"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|todo| {
                    let status = match string(todo, "status") {
                        "completed" => TodoStatus::Completed,
                        "in_progress" => TodoStatus::InProgress,
                        _ => TodoStatus::Pending,
                    };
                    TodoItem::new(string(todo, "content"), status)
                })
                .collect(),
        },
        // Zeron's own tools, offered by the per-run mod.
        name if name.starts_with("zeron_") => ToolCall::Mcp {
            server: "zeron".into(),
            tool: name["zeron_".len()..].into(),
            input: Some(input.clone()),
        },
        _ => ToolCall::Unknown {
            name: name.into(),
            input: Some(input.clone()),
        },
    }
}

/// Every effort Command Code knows, lowest first. Which ones a model takes
/// is only known from the error naming them (see [`unsupported_effort`]).
const EFFORTS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

fn rank(effort: &str) -> Option<usize> {
    EFFORTS.iter().position(|e| *e == effort)
}

/// Command Code's effort for a Zeron level, before any model narrows it.
pub(super) fn effort(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh => "xhigh",
        ReasoningLevel::Max
        | ReasoningLevel::Ultra
        | ReasoningLevel::Ultracode
        | ReasoningLevel::Ultrathink => "max",
    }
}

/// The supported effort closest to `wanted`, the higher one on a tie.
pub(super) fn nearest(wanted: &str, supported: &[String]) -> Option<String> {
    let target = rank(wanted)? as i64;
    supported
        .iter()
        .filter_map(|e| Some((rank(e)? as i64, e)))
        .min_by_key(|(r, _)| ((r - target).abs(), -r))
        .map(|(_, e)| e.clone())
}

/// What a rejected `--effort` says the model takes instead: `Unknown effort
/// "low". Supported: off, high, max.`, or an empty list for a model without
/// adjustable effort.
pub(super) fn unsupported_effort(output: &str) -> Option<Vec<String>> {
    if output.contains("has no adjustable reasoning effort") {
        return Some(Vec::new());
    }
    let (_, rest) = output.split_once("Unknown effort")?;
    let (_, list) = rest.split_once("Supported:")?;
    let list = list
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_end_matches('.');
    Some(
        list.split(',')
            .map(|e| e.trim().to_owned())
            .filter(|e| !e.is_empty())
            .collect(),
    )
}

/// A `--resume` of a session Command Code no longer has (another project
/// directory, or deleted).
pub(super) fn missing_session(error: &str) -> bool {
    error.contains("found to resume")
}

/// Models from `commandcode --list-models`: group headings, then one
/// `<id>  <tagline>` row per model.
pub(super) fn parse_models(text: &str) -> Vec<Model> {
    let levels = vec![
        ReasoningLevel::Low,
        ReasoningLevel::Medium,
        ReasoningLevel::High,
        ReasoningLevel::Max,
    ];
    let mut models = Vec::new();
    let mut group = String::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() || line.starts_with("Available models") {
            continue;
        }
        let Some((id, tagline)) = line
            .split_once("  ")
            .filter(|(id, _)| !id.is_empty() && !id.contains(' '))
        else {
            group = line.trim().to_owned();
            continue;
        };
        let tagline = tagline.trim().trim_end_matches("(default)").trim();
        let label = id.rsplit('/').next().unwrap_or(id).to_owned();
        let description = match (group.is_empty(), tagline.is_empty()) {
            (false, false) => format!("{group} · {tagline}"),
            (false, true) => group.clone(),
            _ => tagline.to_owned(),
        };
        models.push(Model {
            id: id.to_owned(),
            label,
            description: (!description.is_empty()).then_some(description),
            reasoning_levels: levels.clone(),
            options: Vec::new(),
        });
    }
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn event(value: Value) -> Value {
        json!({ "type": "event", "event": value })
    }

    #[test]
    fn a_turn_maps_text_tools_and_usage() {
        let mut n = Normalizer::default();
        let t = Instant::now();
        n.map(&event(json!({"type":"run_start","sessionId":"s1"})), t);
        assert_eq!(n.session.as_deref(), Some("s1"));
        n.map(&event(json!({"type":"model_request_start","model":"m"})), t);
        for i in 0..3 {
            let out = n.map(
                &event(json!({"type":"text_delta","delta":"hi"})),
                t + Duration::from_millis(i * 200),
            );
            assert!(matches!(&out[0], AgentEvent::TextDelta { text } if text == "hi"));
        }
        let out = n.map(
            &event(json!({"type":"model_request_end","usage":{"inputTokens":100,"outputTokens":20,"cacheReadTokens":80,"cacheWriteTokens":0}})),
            t + Duration::from_millis(1000),
        );
        assert!(matches!(
            out[0],
            AgentEvent::ContextUsage {
                tokens: Some(120),
                ..
            }
        ));
        let call = n.map(
            &event(json!({"type":"tool_queued","toolCallId":"c","toolName":"edit_file","input":{"file_path":"/a","old_string":"one","new_string":"two"}})),
            t,
        );
        assert!(
            matches!(&call[0], AgentEvent::ToolCall { call: ToolCall::EditFile { path, .. }, .. } if path == "/a")
        );
        let done = n.map(
            &event(json!({"type":"tool_completed","toolCallId":"c","result":[{"type":"text","text":"Edited"}]})),
            t,
        );
        assert!(
            matches!(&done[0], AgentEvent::ToolResult { is_error: false, output: Some(o), diff: Some(d), .. } if o == "Edited" && d.new_text == "two")
        );
        n.map(
            &json!({"type":"result","subtype":"success","stopReason":"end_turn"}),
            t,
        );
        assert!(n.settled);
        assert_eq!(n.status(), DoneStatus::Completed);
        assert_eq!(n.usage.input_tokens, Some(100));
        assert_eq!(n.usage.cached_input_tokens, Some(80));
        let generation = n.usage.generation.expect("measured");
        assert_eq!(
            (generation.output_tokens, generation.elapsed_ms),
            (20, 1000)
        );
        n.reset();
        assert_eq!(n.usage, TokenUsage::default());
    }

    #[test]
    fn failed_tools_and_errored_results() {
        let mut n = Normalizer::default();
        let t = Instant::now();
        n.map(&event(json!({"type":"tool_queued","toolCallId":"c","toolName":"shell_command","input":{"command":"false"}})), t);
        let out = n.map(
            &event(json!({"type":"tool_errored","toolCallId":"c","error":"boom"})),
            t,
        );
        assert!(
            matches!(&out[0], AgentEvent::ToolResult { is_error: true, output: Some(o), diff: None, .. } if o == "boom")
        );
        n.map(&json!({"type":"result","subtype":"error","error":"Error: No session \"x\" found to resume."}), t);
        assert_eq!(n.status(), DoneStatus::Errored);
        assert!(missing_session(n.error.as_deref().unwrap()));
    }

    #[test]
    fn efforts_fall_back_to_the_nearest_supported_one() {
        let supported =
            unsupported_effort("Unknown effort \"low\". Supported: off, high, max.\n").unwrap();
        assert_eq!(supported, ["off", "high", "max"]);
        assert_eq!(nearest("low", &supported).as_deref(), Some("high"));
        assert_eq!(nearest("minimal", &supported).as_deref(), Some("off"));
        assert_eq!(nearest("xhigh", &supported).as_deref(), Some("max"));
        assert_eq!(nearest("medium", &[]), None);
        assert_eq!(
            unsupported_effort("x has no adjustable reasoning effort."),
            Some(vec![])
        );
        assert_eq!(unsupported_effort("all good"), None);
    }

    #[test]
    fn model_list_rows_keep_their_group() {
        let models = parse_models(
            "Available models  ·  3 models\n\nOpen Source\n\ndeepseek/deepseek-v4-flash             fast reasoning (default)\nmoonshotai/kimi-k3                     long-horizon coding\nAnthropic\nclaude-sonnet-5-5                      best combo\n",
        );
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "deepseek/deepseek-v4-flash");
        assert_eq!(models[0].label, "deepseek-v4-flash");
        assert_eq!(
            models[0].description.as_deref(),
            Some("Open Source · fast reasoning")
        );
        assert_eq!(
            models[2].description.as_deref(),
            Some("Anthropic · best combo")
        );
    }
}
