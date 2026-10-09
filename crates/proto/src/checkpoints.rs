//! Host-local file checkpoints and non-destructive conversation restores.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RestoreMode {
    #[default]
    Conversation,
    Files,
    Both,
}

impl RestoreMode {
    pub fn includes_files(self) -> bool {
        matches!(self, Self::Files | Self::Both)
    }

    pub fn includes_conversation(self) -> bool {
        matches!(self, Self::Conversation | Self::Both)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointParams {
    pub chat_id: String,
    pub message_id: String,
    #[serde(default)]
    pub backup_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointFileChange {
    pub path: String,
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointBackup {
    pub id: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointPreview {
    pub conversation_available: bool,
    pub files_available: bool,
    pub files_error: Option<String>,
    pub token: Option<String>,
    pub root: Option<String>,
    pub files: Vec<CheckpointFileChange>,
    pub backups: Vec<CheckpointBackup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreCheckpointParams {
    #[serde(flatten)]
    pub checkpoint: CheckpointParams,
    pub mode: RestoreMode,
    pub operation_id: String,
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreCheckpointResult {
    pub chat_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<crate::Chat>,
    pub backup_id: Option<String>,
    /// A failed file restore keeps its backup and reports any partial failure.
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_request_is_flat_and_uses_camel_case() {
        let request: RestoreCheckpointParams = serde_json::from_value(serde_json::json!({
            "chatId": "chat", "messageId": "message", "mode": "both",
            "operationId": "operation", "token": "checksum",
        }))
        .unwrap();
        assert!(request.mode.includes_files());
        assert!(request.mode.includes_conversation());
        assert_eq!(request.checkpoint.backup_id, None);
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["chatId"], "chat");
        assert_eq!(value["operationId"], "operation");
        assert!(value.get("checkpoint").is_none());
    }
}
