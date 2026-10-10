//! Volatile, read-only agent shell inventory. Output is requested separately,
//! never journaled into messages or replicated into workspace documents.

use serde::{Deserialize, Serialize};

use crate::HarnessId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ShellTaskStatus {
    Running,
    Completed,
    Failed,
    Stopped,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellTask {
    pub id: String,
    pub chat_id: String,
    pub device_id: String,
    pub harness: HarnessId,
    pub command: String,
    pub status: ShellTaskStatus,
    pub started_at: i64,
    pub start_estimated: bool,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i64>,
    pub output_available: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellTasksSnapshot {
    pub tasks: Vec<ShellTask>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellTaskOutputParams {
    pub chat_id: String,
    pub task_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellTaskOutput {
    pub text: String,
    pub truncated: bool,
    pub error: Option<String>,
}
