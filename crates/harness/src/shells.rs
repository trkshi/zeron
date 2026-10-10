//! Bounded, runtime-local shell telemetry, separate from the turn's lifecycle.

use std::collections::{HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use zeron_proto::{ShellTaskOutput, ShellTaskStatus};

pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_TASKS: usize = 64;
const RETENTION_MS: i64 = 120_000;

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn tail(text: &str, limit: usize) -> (&str, bool) {
    let mut start = text.len().saturating_sub(limit);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    (&text[start..], start > 0)
}

pub(crate) fn command_label(command: &str) -> String {
    let end = command
        .char_indices()
        .nth(2048)
        .map_or(command.len(), |(at, _)| at);
    command[..end].to_owned()
}

#[derive(Clone, Debug)]
pub struct ShellEntry {
    pub id: String,
    pub command: String,
    pub status: ShellTaskStatus,
    pub started_at: i64,
    pub start_estimated: bool,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i64>,
    pub output_available: bool,
}

struct Task {
    entry: ShellEntry,
    output: String,
    truncated: bool,
    output_file: Option<PathBuf>,
    background: bool,
    yielded: bool,
}

#[derive(Default)]
struct Store {
    tasks: VecDeque<Task>,
    truncated: bool,
    ended: bool,
}

#[derive(Clone, Default)]
pub struct ShellMonitor {
    store: Arc<Mutex<Store>>,
    refresh: Arc<tokio::sync::Notify>,
}

impl ShellMonitor {
    pub fn snapshot(&self) -> (Vec<ShellEntry>, bool) {
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let cutoff = now_ms() - RETENTION_MS;
        store
            .tasks
            .retain(|task| task.entry.finished_at.is_none_or(|at| at >= cutoff));
        if store.tasks.is_empty() {
            store.truncated = false;
        }
        (
            store.tasks.iter().map(|task| task.entry.clone()).collect(),
            store.truncated,
        )
    }

    pub fn request_refresh(&self) {
        self.refresh.notify_one();
    }

    pub(crate) async fn refresh_requested(&self) {
        self.refresh.notified().await;
    }

    pub(crate) fn start(&self, id: &str, command: &str, estimated: bool, background: bool) {
        if id.is_empty() || id.len() > 256 {
            return;
        }
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        if store.ended {
            return;
        }
        if let Some(task) = store.tasks.iter_mut().find(|task| task.entry.id == id) {
            if !command.is_empty() && (!estimated || task.entry.command.is_empty()) {
                task.entry.command = command_label(command);
            }
            task.background |= background;
            if task.entry.status == ShellTaskStatus::Unknown {
                task.entry.finished_at = None;
            }
            if task.entry.finished_at.is_none() {
                task.entry.status = ShellTaskStatus::Running;
            }
            return;
        }
        if store.tasks.len() == MAX_TASKS {
            let at = store
                .tasks
                .iter()
                .position(|task| task.entry.finished_at.is_some())
                .unwrap_or(0);
            store.tasks.remove(at);
            store.truncated = true;
        }
        store.tasks.push_back(Task {
            entry: ShellEntry {
                id: id.into(),
                command: command_label(command),
                status: ShellTaskStatus::Running,
                started_at: now_ms(),
                start_estimated: estimated,
                finished_at: None,
                exit_code: None,
                output_available: false,
            },
            output: String::new(),
            truncated: false,
            output_file: None,
            background,
            yielded: false,
        });
    }

    pub(crate) fn contains(&self, id: &str) -> bool {
        self.store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tasks
            .iter()
            .any(|task| task.entry.id == id)
    }

    pub(crate) fn rekey(&self, from: &str, to: &str) {
        if to.is_empty() || to.len() > 256 {
            return;
        }
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        if store.tasks.iter().any(|task| task.entry.id == to) {
            store.tasks.retain(|task| task.entry.id != from);
        } else if let Some(task) = store.tasks.iter_mut().find(|task| task.entry.id == from) {
            task.entry.id = to.into();
            task.background = true;
        }
    }

    pub(crate) fn finish(&self, id: &str, status: ShellTaskStatus, exit_code: Option<i64>) {
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(task) = store.tasks.iter_mut().find(|task| task.entry.id == id) {
            task.entry.status = status;
            task.entry.finished_at = Some(now_ms());
            task.entry.exit_code = exit_code;
        }
    }

    pub(crate) fn yielded(&self, id: &str) {
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(task) = store.tasks.iter_mut().find(|task| task.entry.id == id) {
            task.yielded = true;
            task.entry.status = ShellTaskStatus::Unknown;
        }
    }

    /// An inventory's omissions prove absence, not successful exit. A later
    /// terminal event can still replace Unknown with its actual outcome.
    pub(crate) fn reconcile(&self, ids: &HashSet<String>, codex: bool) {
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        for task in &mut store.tasks {
            if task.entry.finished_at.is_none()
                && !ids.contains(&task.entry.id)
                && (task.background || (codex && task.yielded))
            {
                task.entry.status = ShellTaskStatus::Unknown;
                task.entry.finished_at = Some(now_ms());
            }
        }
    }

    pub(crate) fn append(&self, id: &str, output: &str) {
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(task) = store.tasks.iter_mut().find(|task| task.entry.id == id) {
            let (output, truncated) = tail(output, MAX_OUTPUT_BYTES);
            task.output.push_str(output);
            let (bounded, trimmed) = tail(&task.output, MAX_OUTPUT_BYTES);
            task.output = bounded.to_owned();
            task.truncated |= truncated || trimmed;
            task.entry.output_available |= !task.output.is_empty();
        }
    }

    pub(crate) fn replace_output(&self, id: &str, output: &str) {
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(task) = store.tasks.iter_mut().find(|task| task.entry.id == id) {
            let (output, truncated) = tail(output, MAX_OUTPUT_BYTES);
            task.output = output.to_owned();
            task.truncated |= truncated;
            task.entry.output_available |= !task.output.is_empty();
        }
    }

    pub(crate) fn output_file(&self, id: &str, path: &str) {
        if path.is_empty() || !valid_output_path(id, Path::new(path)) {
            return;
        }
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(task) = store.tasks.iter_mut().find(|task| task.entry.id == id) {
            task.output_file = Some(path.into());
            task.entry.output_available = true;
        }
    }

    pub fn output(&self, id: &str) -> Option<ShellTaskOutput> {
        let (text, truncated, path) = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let task = store.tasks.iter().find(|task| task.entry.id == id)?;
            (
                task.output.clone(),
                task.truncated,
                task.output_file.clone(),
            )
        };
        if let Some(path) = path {
            match read_output(id, &path) {
                Ok(output) => return Some(output),
                Err(_) if text.is_empty() => {
                    return Some(ShellTaskOutput {
                        error: Some("Shell output is unavailable".into()),
                        ..Default::default()
                    });
                }
                Err(_) => {}
            }
        }
        Some(ShellTaskOutput {
            text,
            truncated,
            error: None,
        })
    }

    pub fn runtime_ended(&self) {
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        store.ended = true;
        for task in &mut store.tasks {
            if task.entry.finished_at.is_none() {
                task.entry.status = ShellTaskStatus::Unknown;
                task.entry.finished_at = Some(now_ms());
            }
        }
    }
}

fn valid_output_path(id: &str, path: &Path) -> bool {
    if id.is_empty()
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return false;
    }
    if path.file_name().and_then(|name| name.to_str()) != Some(format!("{id}.output").as_str())
        || path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            != Some("tasks")
    {
        return false;
    }
    let temporary = std::env::temp_dir();
    let canonical_temporary = temporary
        .canonicalize()
        .unwrap_or_else(|_| temporary.clone());
    let Ok(relative) = path
        .strip_prefix(&temporary)
        .or_else(|_| path.strip_prefix(&canonical_temporary))
    else {
        return false;
    };
    let Some(std::path::Component::Normal(root)) = relative.components().next() else {
        return false;
    };
    let root = root.to_string_lossy();
    (root == "claude" || root.starts_with("claude-"))
        && relative
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
}

fn read_output(id: &str, path: &Path) -> std::io::Result<ShellTaskOutput> {
    let canonical = path.canonicalize()?;
    if !valid_output_path(id, &canonical)
        || std::fs::symlink_metadata(path)?.file_type().is_symlink()
    {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(&canonical)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    let start = metadata.len().saturating_sub(MAX_OUTPUT_BYTES as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(MAX_OUTPUT_BYTES);
    file.take(MAX_OUTPUT_BYTES as u64).read_to_end(&mut bytes)?;
    let decoded = String::from_utf8_lossy(&bytes);
    let (text, trimmed) = tail(&decoded, MAX_OUTPUT_BYTES);
    Ok(ShellTaskOutput {
        text: text.to_owned(),
        truncated: start > 0 || trimmed,
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_and_history_are_bounded() {
        let monitor = ShellMonitor::default();
        monitor.start("a", "echo test", false, false);
        monitor.append("a", &"x".repeat(MAX_OUTPUT_BYTES));
        monitor.append("a", "end");
        let output = monitor.output("a").unwrap();
        assert!(output.truncated);
        assert_eq!(output.text.len(), MAX_OUTPUT_BYTES);
        assert!(output.text.ends_with("end"));
        for id in 0..100 {
            monitor.start(&id.to_string(), "sleep 1", false, false);
        }
        let (entries, truncated) = monitor.snapshot();
        assert_eq!(entries.len(), MAX_TASKS);
        assert!(truncated);
    }

    #[test]
    fn missing_task_and_runtime_exit_do_not_invent_success() {
        let monitor = ShellMonitor::default();
        monitor.start("bg", "server", true, true);
        monitor.start("fg", "sleep 1", false, false);
        monitor.reconcile(&HashSet::new(), false);
        assert_eq!(monitor.snapshot().0[0].status, ShellTaskStatus::Unknown);
        assert_eq!(monitor.snapshot().0[1].status, ShellTaskStatus::Running);
        monitor.finish("bg", ShellTaskStatus::Failed, Some(1));
        monitor.runtime_ended();
        assert_eq!(monitor.snapshot().0[0].status, ShellTaskStatus::Failed);
        assert_eq!(monitor.snapshot().0[1].status, ShellTaskStatus::Unknown);
    }

    #[test]
    fn inventory_can_recover_unknown_but_never_revives_a_terminal_task() {
        let monitor = ShellMonitor::default();
        monitor.start("bg", "actual command", false, true);
        monitor.reconcile(&HashSet::new(), false);
        monitor.start("bg", "description", true, true);
        assert_eq!(monitor.snapshot().0[0].status, ShellTaskStatus::Running);
        assert_eq!(monitor.snapshot().0[0].command, "actual command");
        monitor.finish("bg", ShellTaskStatus::Completed, Some(0));
        monitor.start("bg", "description", true, true);
        assert_eq!(monitor.snapshot().0[0].status, ShellTaskStatus::Completed);
    }

    #[test]
    fn utf8_output_tail_does_not_split_characters() {
        let monitor = ShellMonitor::default();
        monitor.start("a", "test", false, false);
        monitor.append("a", &"\u{1f642}".repeat(MAX_OUTPUT_BYTES));
        monitor.append("a", "x");
        let output = monitor.output("a").unwrap();
        assert!(output.text.len() <= MAX_OUTPUT_BYTES);
        assert!(output.text.ends_with('x'));
        assert!(output.truncated);
    }

    #[test]
    fn only_registered_provider_task_files_are_read() {
        let root = tempfile::Builder::new()
            .prefix("claude-shell-test-")
            .tempdir()
            .unwrap();
        let tasks = root.path().join("session/tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        let path = tasks.join("bg.output");
        std::fs::write(&path, "test output").unwrap();
        assert_eq!(read_output("bg", &path).unwrap().text, "test output");
        std::fs::write(&path, vec![0xff; MAX_OUTPUT_BYTES * 2]).unwrap();
        let output = read_output("bg", &path).unwrap();
        assert!(output.truncated);
        assert!(output.text.len() <= MAX_OUTPUT_BYTES);
        assert!(!valid_output_path("../bg", &path));
        assert!(!valid_output_path("bg", Path::new("/etc/bg.output")));
        #[cfg(unix)]
        {
            let link = tasks.join("link.output");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(read_output("link", &link).is_err());
        }
    }
}
