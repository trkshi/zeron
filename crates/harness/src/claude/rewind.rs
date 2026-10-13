//! Claude's session transcript, read for in-place rewinds.
//!
//! A stdin user line carrying `uuid` is persisted under that uuid, its
//! `parentUuid` naming the previous chain entry; `--resume-session-at
//! <parentUuid> --resume-drops-turn <uuid>` then resumes the same session with
//! that prompt and everything after it dropped (verified against CLI 2.1.296).
//! A uuid the session already holds is taken as a replay and starts no turn,
//! so a reused transcript id must first be checked against the session.

use std::collections::HashSet;
use std::io::BufRead as _;
use std::path::{Path, PathBuf};

use serde_json::Value;

fn config_root() -> PathBuf {
    crate::model_context::root(
        "CLAUDE_CONFIG_DIR",
        crate::executable::home_or_current_dir().join(".claude"),
    )
}

/// Session ids are global; search project directories rather than duplicating
/// the CLI's cwd encoding (see `Normalizer::for_resume`).
fn transcript(config_root: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    std::fs::read_dir(config_root.join("projects"))
        .ok()?
        .flatten()
        .map(|project| project.path().join(format!("{session_id}.jsonl")))
        .find(|path| path.is_file())
}

/// A canonical UUID, the only shape reused as a provider message id.
pub(crate) fn is_message_uuid(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.hyphenated().to_string() == id)
}

/// The first record whose `uuid` is `id`. Lines are only parsed when they
/// mention the id: long sessions' histories run to hundreds of MB.
fn find_record(file: &Path, id: &str) -> std::io::Result<Option<Value>> {
    let mut reader = std::io::BufReader::with_capacity(1 << 16, std::fs::File::open(file)?);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(None);
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            continue;
        };
        if !text.contains(id) {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if record.get("uuid").and_then(Value::as_str) == Some(id) {
            return Ok(Some(record));
        }
    }
}

/// Transcript ids a run may not reuse: every record uuid the resumed session
/// holds. `None` when the history exists but cannot be read — then no id is
/// safe to reuse and the run falls back to random ones.
pub(crate) fn taken_ids(session_id: Option<&str>) -> Option<HashSet<String>> {
    match session_id {
        Some(session) => taken_in(&config_root(), session),
        None => Some(HashSet::new()),
    }
}

fn taken_in(config_root: &Path, session_id: &str) -> Option<HashSet<String>> {
    const KEY: &str = "\"uuid\":\"";
    let Some(file) = transcript(config_root, session_id) else {
        // No history yet: nothing to collide with.
        return Some(HashSet::new());
    };
    let mut reader = std::io::BufReader::with_capacity(1 << 16, std::fs::File::open(file).ok()?);
    let mut taken = HashSet::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).ok()? == 0 {
            return Some(taken);
        }
        // Quotes inside string values are escaped, so every match is a real
        // key; a nested one only costs a reusable id.
        let text = String::from_utf8_lossy(&line);
        for (at, _) in text.match_indices(KEY) {
            if let Some(id) = text.get(at + KEY.len()..at + KEY.len() + 36) {
                taken.insert(id.to_owned());
            }
        }
    }
}

/// `id` as this run's provider message id, when it is a UUID the session does
/// not hold yet (the CLI answers a held id as a replay, with no turn).
pub(crate) fn reusable(id: Option<&str>, taken: Option<&HashSet<String>>) -> Option<String> {
    let id = id.filter(|id| is_message_uuid(id))?;
    taken.is_some_and(|taken| !taken.contains(id)).then(|| id.to_owned())
}

/// The entry a rewind before prompt `prompt_id` keeps through: that prompt's
/// `parentUuid`. `None` when the session, the prompt, or its parent is missing
/// (the prompt was sent before ids were reused, or the history was removed).
/// Blocking: reads the session's whole history in the worst case.
pub fn rewind_anchor(session_id: &str, prompt_id: &str) -> Option<String> {
    anchor_in(&config_root(), session_id, prompt_id)
}

fn anchor_in(config_root: &Path, session_id: &str, prompt_id: &str) -> Option<String> {
    if !is_message_uuid(prompt_id) {
        return None;
    }
    let record = find_record(&transcript(config_root, session_id)?, prompt_id).ok()??;
    if record.get("type").and_then(Value::as_str) != Some("user")
        || record.get("isSidechain").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    record
        .get("parentUuid")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "0e45f591-7c64-45c3-a280-34f0124dcc9e";
    const FIRST: &str = "11111111-2222-4333-8444-555555555555";
    const SECOND: &str = "22222222-2222-4333-8444-555555555555";

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("projects").join("-tmp-project");
        std::fs::create_dir_all(&project).unwrap();
        let lines = [
            format!(r#"{{"type":"attachment","uuid":"a0","parentUuid":null}}"#),
            format!(r#"{{"type":"user","uuid":"{FIRST}","parentUuid":"a0"}}"#),
            format!(r#"{{"type":"assistant","uuid":"r1","parentUuid":"{FIRST}"}}"#),
            r#"{"type":"queue-operation"}"#.to_owned(),
            format!(r#"{{"type":"user","uuid":"{SECOND}","parentUuid":"r1"}}"#),
        ];
        std::fs::write(project.join(format!("{SESSION}.jsonl")), lines.join("\n")).unwrap();
        root
    }

    #[test]
    fn anchor_is_the_dropped_prompts_parent() {
        let root = fixture();
        assert_eq!(anchor_in(root.path(), SESSION, SECOND).as_deref(), Some("r1"));
        assert_eq!(anchor_in(root.path(), SESSION, FIRST).as_deref(), Some("a0"));
    }

    #[test]
    fn missing_sessions_prompts_and_odd_ids_have_no_anchor() {
        let root = fixture();
        let unknown = "33333333-2222-4333-8444-555555555555";
        assert_eq!(anchor_in(root.path(), SESSION, unknown), None);
        assert_eq!(anchor_in(root.path(), unknown, SECOND), None);
        assert_eq!(anchor_in(root.path(), SESSION, "r1"), None);
        assert_eq!(anchor_in(root.path(), "../escape", SECOND), None);
    }

    #[test]
    fn ids_the_session_holds_are_not_reused() {
        let root = fixture();
        let taken = taken_in(root.path(), SESSION).unwrap();
        let unknown = "33333333-2222-4333-8444-555555555555";
        assert_eq!(reusable(Some(SECOND), Some(&taken)), None);
        assert_eq!(reusable(Some(unknown), Some(&taken)).as_deref(), Some(unknown));
        assert_eq!(reusable(Some("not-a-uuid"), Some(&taken)), None);
        // Unreadable history: no id is safe.
        assert_eq!(reusable(Some(unknown), None), None);
        // No history yet: nothing to collide with.
        let empty = taken_in(root.path(), "44444444-2222-4333-8444-555555555555").unwrap();
        assert_eq!(reusable(Some(SECOND), Some(&empty)).as_deref(), Some(SECOND));
    }
}
