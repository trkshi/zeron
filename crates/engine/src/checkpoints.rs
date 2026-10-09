//! Bounded, host-local snapshots. Neither capture nor restore touches Git's index/HEAD.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeron_proto::{CheckpointBackup, CheckpointFileChange, RestoreCheckpointResult};

use crate::{Repos, WorkspaceFiles};

const MAX_FILES: usize = 20_000;
const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 128 * 1024 * 1024;
const MAX_STORE_BYTES: i64 = 512 * 1024 * 1024;
const MAX_DATABASE_BYTES: i64 = 768 * 1024 * 1024;
const MAX_GIT_OUTPUT: u64 = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct Checkpoints {
    database: PathBuf,
    repos: Repos,
    workspace_files: WorkspaceFiles,
    /// Order prompt admission with restores; no new agent starts during a restore.
    pub(crate) gate: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct File {
    hash: String,
    mode: u32,
}

#[derive(PartialEq, Eq)]
struct FileStamp {
    size: u64,
    modified: Option<SystemTime>,
    mode: u32,
    #[cfg(unix)]
    identity: (u64, u64),
}

fn stamp(metadata: &std::fs::Metadata) -> FileStamp {
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777
    };
    #[cfg(not(unix))]
    let mode = if metadata.permissions().readonly() {
        0o444
    } else {
        0o644
    };
    FileStamp {
        size: metadata.len(),
        modified: metadata.modified().ok(),
        mode,
        #[cfg(unix)]
        identity: {
            use std::os::unix::fs::MetadataExt;
            (metadata.dev(), metadata.ino())
        },
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    root: PathBuf,
    git_dir: PathBuf,
    head: Option<String>,
    reference: String,
    files: BTreeMap<String, File>,
}

#[derive(Debug)]
pub(crate) struct FilePlan {
    id: String,
    saved: Snapshot,
    current: Snapshot,
    pub files: Vec<CheckpointFileChange>,
    pub token: String,
}

impl FilePlan {
    pub fn root(&self) -> String {
        self.saved.root.to_string_lossy().into_owned()
    }
}

fn error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn digest(bytes: &[u8]) -> String {
    crate::repos::hex(&Sha256::digest(bytes))
}

fn key(chat: &str, message: &str) -> String {
    digest(format!("{chat}\0{message}").as_bytes())
}

fn redirects_path(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Junctions and other reparse points are not all reported as symlinks.
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn validate_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.contains(['\\', '\0'])
        || !Path::new(path).components().all(|part| {
            matches!(part, Component::Normal(name) if !name.to_string_lossy().eq_ignore_ascii_case(".git"))
        })
    {
        return Err("Checkpoint contains an unsafe file path".into());
    }
    #[cfg(windows)]
    for component in path.split('/') {
        let stem = component
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if component.contains([':', '*', '?', '"', '<', '>', '|'])
            || component.ends_with(['.', ' '])
            || component.chars().any(char::is_control)
            || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || matches!(
                stem.as_bytes(),
                [b'C', b'O', b'M', b'1'..=b'9'] | [b'L', b'P', b'T', b'1'..=b'9']
            )
        {
            return Err("Checkpoint contains an unsafe Windows file path".into());
        }
    }
    Ok(())
}

fn safe_path(root: &Path, path: &str) -> Result<PathBuf, String> {
    validate_path(path)?;
    let metadata = std::fs::symlink_metadata(root).map_err(error)?;
    if !metadata.is_dir()
        || redirects_path(&metadata)
        || root.canonicalize().map_err(error)? != root
    {
        return Err("Project folder changed during restore".into());
    }
    let mut absolute = root.to_path_buf();
    for component in Path::new(path).components() {
        absolute.push(component);
        match std::fs::symlink_metadata(&absolute) {
            Ok(metadata) if redirects_path(&metadata) => {
                return Err(format!(
                    "Symlinks and reparse points cannot be restored: {path}"
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(error(e)),
        }
    }
    Ok(absolute)
}

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut command = Command::new("git");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(error)?;
    let stdout = child.stdout.take().ok_or("Git output unavailable")?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let result = stdout.take(MAX_GIT_OUTPUT + 1).read_to_end(&mut output);
        let _ = sender.send(result.map(|_| output));
    });
    let output = receiver.recv_timeout(Duration::from_secs(20));
    let output = match output {
        Ok(Ok(output)) if output.len() as u64 <= MAX_GIT_OUTPUT => output,
        other => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(match other {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    "Git checkpoint scan timed out; no files were changed"
                }
                _ => "Project file list exceeds the checkpoint limit or could not be read",
            }
            .into());
        }
    };
    let _ = reader.join();
    if !child.wait().map_err(error)?.success() {
        return Err("Could not read Git project state".into());
    }
    Ok(output)
}

fn head(root: &Path) -> Result<Option<String>, String> {
    // An unborn repository has no HEAD, but is still a valid checkpoint target.
    let output = git(root, &["rev-parse", "--verify", "--quiet", "HEAD"]);
    match output {
        Ok(bytes) => Ok(Some(String::from_utf8(bytes).map_err(error)?.trim().into())),
        Err(_) => {
            git(root, &["rev-parse", "--git-dir"])?;
            Ok(None)
        }
    }
}

fn reference(git_dir: &Path) -> Result<String, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(git_dir.join("HEAD"))
        .map_err(error)?
        .take(4097)
        .read_to_end(&mut bytes)
        .map_err(error)?;
    if bytes.len() > 4096 {
        return Err("Git HEAD file exceeds the checkpoint limit".into());
    }
    String::from_utf8(bytes)
        .map(|value| value.trim().to_owned())
        .map_err(error)
}

fn paths(root: &Path) -> Result<BTreeSet<String>, String> {
    let bytes = git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let mut paths = BTreeSet::new();
    for bytes in bytes
        .split(|byte| *byte == 0)
        .filter(|bytes| !bytes.is_empty())
    {
        let path =
            std::str::from_utf8(bytes).map_err(|_| "Non-UTF-8 paths cannot be checkpointed")?;
        validate_path(path)?;
        paths.insert(path.to_owned());
        if paths.len() > MAX_FILES {
            return Err("Project exceeds the 20,000-file checkpoint limit".into());
        }
    }
    Ok(paths)
}

fn read_file(root: &Path, path: &str) -> Result<Option<(File, Vec<u8>, FileStamp)>, String> {
    let absolute = safe_path(root, path)?;
    let metadata = match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(error(e)),
    };
    if !metadata.is_file() {
        return Err(format!(
            "Submodules and non-regular files cannot be checkpointed: {path}"
        ));
    }
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(format!("File exceeds the 8 MiB checkpoint limit: {path}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o7000 != 0 {
            return Err(format!(
                "Files with special permission bits cannot be checkpointed: {path}"
            ));
        }
    }
    let mut bytes = Vec::new();
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let opened = options.open(&absolute).map_err(error)?;
    let opened_metadata = opened.metadata().map_err(error)?;
    if !opened_metadata.is_file()
        || redirects_path(&opened_metadata)
        || stamp(&opened_metadata) != stamp(&metadata)
    {
        return Err(format!("File changed during checkpoint capture: {path}"));
    }
    opened
        .take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(error)?;
    let after = std::fs::symlink_metadata(&absolute).map_err(error)?;
    if !after.is_file()
        || redirects_path(&after)
        || bytes.len() > MAX_FILE_BYTES
        || bytes.len() as u64 != metadata.len()
        || stamp(&metadata) != stamp(&after)
    {
        return Err(format!("File changed during checkpoint capture: {path}"));
    }
    Ok(Some((
        File {
            hash: digest(&bytes),
            mode: stamp(&metadata).mode,
        },
        bytes,
        stamp(&metadata),
    )))
}

fn snapshot(
    root: &Path,
    mut save: impl FnMut(&File, &[u8]) -> Result<(), String>,
) -> Result<Snapshot, String> {
    let root = root.canonicalize().map_err(error)?;
    let git_dir = PathBuf::from(
        String::from_utf8(git(&root, &["rev-parse", "--absolute-git-dir"])?)
            .map_err(error)?
            .trim(),
    )
    .canonicalize()
    .map_err(error)?;
    let reference = reference(&git_dir)?;
    let head = head(&root)?;
    let paths = paths(&root)?;
    let mut files = BTreeMap::new();
    let mut stamps = BTreeMap::new();
    let mut size = 0usize;
    for path in &paths {
        if let Some((file, bytes, stamp)) = read_file(&root, path)? {
            size = size.saturating_add(bytes.len());
            if size > MAX_SNAPSHOT_BYTES {
                return Err("Project exceeds the 128 MiB checkpoint limit".into());
            }
            save(&file, &bytes)?;
            files.insert(path.clone(), file);
            stamps.insert(path.clone(), Some(stamp));
        } else {
            stamps.insert(path.clone(), None);
        }
    }
    // Catch early files changing while the rest of the tree was being read.
    for (path, before) in stamps {
        let metadata = match std::fs::symlink_metadata(safe_path(&root, &path)?) {
            Ok(metadata) if metadata.is_file() => Some(stamp(&metadata)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            _ => return Err(format!("File changed during checkpoint capture: {path}")),
        };
        if metadata != before {
            return Err(format!("File changed during checkpoint capture: {path}"));
        }
    }
    if paths != self::paths(&root)?
        || head != self::head(&root)?
        || reference != self::reference(&git_dir)?
    {
        return Err("Project changed during checkpoint capture".into());
    }
    Ok(Snapshot {
        root,
        git_dir,
        head,
        reference,
        files,
    })
}

fn changes(current: &Snapshot, saved: &Snapshot) -> Vec<CheckpointFileChange> {
    current
        .files
        .keys()
        .chain(saved.files.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| current.files.get(*path) != saved.files.get(*path))
        .map(|path| CheckpointFileChange {
            path: path.clone(),
            action: match (current.files.get(path), saved.files.get(path)) {
                (None, Some(_)) => "Create",
                (Some(_), None) => "Remove",
                _ => "Restore",
            }
            .into(),
        })
        .collect()
}

impl Checkpoints {
    pub fn new(data_dir: &Path, repos: Repos, workspace_files: WorkspaceFiles) -> Self {
        Self {
            database: data_dir.join("checkpoints").join("checkpoints.sqlite"),
            repos,
            workspace_files,
            gate: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    fn open(&self) -> Result<Connection, String> {
        let directory = self
            .database
            .parent()
            .ok_or("Checkpoint directory unavailable")?;
        std::fs::create_dir_all(directory).map_err(error)?;
        if redirects_path(&std::fs::symlink_metadata(directory).map_err(error)?)
            || std::fs::symlink_metadata(&self.database).is_ok_and(|m| redirects_path(&m))
        {
            return Err("Checkpoint storage must not be a symlink or reparse point".into());
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                .map_err(error)?;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&self.database).map_err(error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(error)?;
        }
        let connection = Connection::open(&self.database).map_err(error)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(10))
            .map_err(error)?;
        connection
            .pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .map_err(error)?;
        let page_size: i64 = connection
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .map_err(error)?;
        connection
            .pragma_update(None, "max_page_count", MAX_DATABASE_BYTES / page_size)
            .map_err(error)?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS blobs(hash TEXT PRIMARY KEY, data BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS checkpoints(
               id TEXT PRIMARY KEY, chat TEXT NOT NULL, message TEXT NOT NULL,
               created INTEGER NOT NULL, backup INTEGER NOT NULL, snapshot TEXT, error TEXT);
             CREATE INDEX IF NOT EXISTS checkpoint_chat ON checkpoints(chat, created);
             CREATE TABLE IF NOT EXISTS checkpoint_blobs(
               checkpoint TEXT REFERENCES checkpoints(id) ON DELETE CASCADE,
               hash TEXT REFERENCES blobs(hash), PRIMARY KEY(checkpoint, hash));
             CREATE INDEX IF NOT EXISTS checkpoint_blob_hash ON checkpoint_blobs(hash);
             CREATE TABLE IF NOT EXISTS restores(id TEXT PRIMARY KEY, request TEXT NOT NULL,
               created INTEGER NOT NULL, backup TEXT, result TEXT);
             CREATE INDEX IF NOT EXISTS pending_restore_backup ON restores(backup) WHERE result IS NULL;",
            )
            .map_err(error)?;
        Ok(connection)
    }

    fn prune(
        connection: &Connection,
        chat: &str,
        incoming_backup: bool,
        preserve: Option<&str>,
    ) -> Result<(), String> {
        for (backup, limit) in [(0, 50), (1, 10)] {
            let protected: i64 = connection.query_row(
                "SELECT COUNT(*) FROM checkpoints WHERE chat=?1 AND backup=?2 AND
                 (id=COALESCE(?3,'') OR EXISTS (SELECT 1 FROM restores WHERE backup=checkpoints.id AND result IS NULL))",
                params![chat, backup, preserve], |row| row.get(0),
            ).map_err(error)?;
            let incoming = if (backup != 0) == incoming_backup {
                1
            } else {
                0
            };
            let keep = (limit - incoming - protected).max(0);
            connection.execute(
                "DELETE FROM checkpoints WHERE id IN (
                 SELECT id FROM checkpoints WHERE chat=?1 AND backup=?2
                 AND id != COALESCE(?4,'')
                 AND NOT EXISTS (SELECT 1 FROM restores WHERE backup=checkpoints.id AND result IS NULL)
                 ORDER BY created DESC, id DESC LIMIT -1 OFFSET ?3)",
                params![chat, backup, keep, preserve],
            ).map_err(error)?;
        }
        let protected: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM checkpoints WHERE id=COALESCE(?1,'') OR EXISTS
             (SELECT 1 FROM restores WHERE backup=checkpoints.id AND result IS NULL)",
                [preserve],
                |row| row.get(0),
            )
            .map_err(error)?;
        connection
            .execute(
                "DELETE FROM checkpoints WHERE id IN (
             SELECT id FROM checkpoints WHERE id != COALESCE(?1,'')
             AND NOT EXISTS (SELECT 1 FROM restores WHERE backup=checkpoints.id AND result IS NULL)
             ORDER BY created DESC, id DESC LIMIT -1 OFFSET ?2)",
                params![preserve, (999 - protected).max(0)],
            )
            .map_err(error)?;
        connection.execute(
            "DELETE FROM blobs WHERE NOT EXISTS (SELECT 1 FROM checkpoint_blobs WHERE hash=blobs.hash)", [],
        ).map_err(error)?;
        Ok(())
    }

    fn save_snapshot(
        &self,
        id: &str,
        chat: &str,
        message: &str,
        root: &Path,
        backup: bool,
        preserve: Option<&str>,
    ) -> Result<Snapshot, String> {
        let mut connection = self.open()?;
        let transaction = connection.transaction().map_err(error)?;
        Self::prune(&transaction, chat, backup, preserve)?;
        let mut stored: i64 = transaction
            .query_row(
                "SELECT COALESCE(SUM(length(data)),0) FROM blobs",
                [],
                |row| row.get(0),
            )
            .map_err(error)?;
        let saved = snapshot(root, |file, bytes| {
            let exists: bool = transaction
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM blobs WHERE hash=?1)",
                    [&file.hash],
                    |row| row.get(0),
                )
                .map_err(error)?;
            if !exists {
                stored += bytes.len() as i64;
                if stored > MAX_STORE_BYTES {
                    return Err(
                        "Checkpoint storage reached its 512 MiB limit; no files were changed"
                            .into(),
                    );
                }
                transaction
                    .execute(
                        "INSERT INTO blobs(hash,data) VALUES(?1,?2)",
                        params![file.hash, bytes],
                    )
                    .map_err(error)?;
            }
            Ok(())
        })?;
        transaction.execute(
            "INSERT INTO checkpoints(id,chat,message,created,backup,snapshot) VALUES(?1,?2,?3,?4,?5,?6)",
            params![id, chat, message, crate::now_ms(), backup, serde_json::to_string(&saved).map_err(error)?],
        ).map_err(error)?;
        for file in saved.files.values() {
            transaction
                .execute(
                    "INSERT OR IGNORE INTO checkpoint_blobs(checkpoint,hash) VALUES(?1,?2)",
                    params![id, file.hash],
                )
                .map_err(error)?;
        }
        transaction.commit().map_err(error)?;
        connection
            .execute_batch("PRAGMA incremental_vacuum(512);")
            .map_err(error)?;
        Ok(saved)
    }

    /// Captured before prompt delivery. Busy/unsupported projects never claim a usable snapshot.
    pub(crate) async fn capture(
        &self,
        chat: &str,
        message: &str,
        cwd: &str,
        unavailable: Option<&str>,
        admission: tokio::sync::OwnedMutexGuard<()>,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let identity = if let Some(reason) = unavailable {
            Err(reason.to_owned())
        } else {
            self.repos
                .checkout_identity(Path::new(cwd))
                .await
                .map_err(error)
        };
        let workspace_guard = match &identity {
            Ok(identity) => Some(
                self.workspace_files
                    .mutation_gate(&identity.id)
                    .write_owned()
                    .await,
            ),
            Err(_) => None,
        };
        let this = self.clone();
        let (chat, message) = (chat.to_owned(), message.to_owned());
        let result = tokio::task::spawn_blocking(move || {
            let _workspace_guard = workspace_guard;
            let result = (|| -> Result<(), String> {
            let mut connection = this.open()?;
            let id = key(&chat, &message);
            let exists: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM checkpoints WHERE id=?1)", [&id], |row| row.get(0)).map_err(error)?;
            if exists { return Ok(()); }
            let saved = identity.and_then(|identity| this.save_snapshot(&id, &chat, &message, &identity.root, false, None));
            if let Err(reason) = saved {
                let transaction = connection.transaction().map_err(error)?;
                Self::prune(&transaction, &chat, false, None)?;
                transaction.execute(
                    "INSERT OR IGNORE INTO checkpoints(id,chat,message,created,backup,error) VALUES(?1,?2,?3,?4,0,?5)",
                    params![id, chat, message, crate::now_ms(), reason],
                ).map_err(error)?;
                transaction.commit().map_err(error)?;
            }
            Ok(())
            })();
            (admission, result)
        }).await;
        match result {
            Ok((admission, result)) => {
                if let Err(error) = result {
                    tracing::warn!(%error, "Could not save the turn's local file checkpoint");
                }
                admission
            }
            Err(error) => {
                tracing::warn!(%error, "Checkpoint worker failed");
                self.gate.clone().lock_owned().await
            }
        }
    }

    fn load(&self, id: &str, chat: &str, backup: bool) -> Result<Snapshot, String> {
        let connection = self.open()?;
        let row: Option<(Option<String>, Option<String>)> = connection
            .query_row(
                "SELECT snapshot,error FROM checkpoints WHERE id=?1 AND chat=?2 AND backup=?3",
                params![id, chat, backup],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(error)?;
        match row {
            Some((Some(snapshot), _)) => {
                let snapshot: Snapshot = serde_json::from_str(&snapshot).map_err(error)?;
                if snapshot.files.len() > MAX_FILES { return Err("Checkpoint manifest exceeds the file limit".into()); }
                for (path, file) in &snapshot.files {
                    validate_path(path)?;
                    if file.mode > 0o777 || file.hash.len() != 64 || !file.hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                        return Err("Checkpoint manifest is corrupt".into());
                    }
                }
                Ok(snapshot)
            }
            Some((_, Some(error))) => Err(error),
            _ => Err("No saved file checkpoint for this message. Older or expired turns support conversation restore only.".into()),
        }
    }

    pub(crate) fn backups(&self, chat: &str) -> Result<Vec<CheckpointBackup>, String> {
        let connection = self.open()?;
        let mut statement = connection.prepare("SELECT id,created FROM checkpoints WHERE chat=?1 AND backup=1 ORDER BY created DESC LIMIT 10").map_err(error)?;
        let rows = statement
            .query_map([chat], |row| {
                Ok(CheckpointBackup {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                })
            })
            .map_err(error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(error)
    }

    pub(crate) fn plan(
        &self,
        chat: &str,
        message: &str,
        backup: Option<&str>,
        root: &Path,
    ) -> Result<FilePlan, String> {
        let id = backup.map_or_else(|| key(chat, message), str::to_owned);
        let saved = self.load(&id, chat, backup.is_some())?;
        let root = root.canonicalize().map_err(error)?;
        if root != saved.root {
            return Err("This conversation now uses a different project folder".into());
        }
        let current = snapshot(&root, |_, _| Ok(()))?;
        if current.git_dir != saved.git_dir
            || current.head != saved.head
            || current.reference != saved.reference
        {
            return Err("Git HEAD changed since this checkpoint. File restore will not rewrite commits or switch branches.".into());
        }
        let files = changes(&current, &saved);
        for change in &files {
            let path = safe_path(&root, &change.path)?;
            if path.is_dir() {
                return Err(format!(
                    "A directory occupies the restore path: {}",
                    change.path
                ));
            }
            // A previously tracked file may now be ignored. Never silently replace it.
            if !current.files.contains_key(&change.path) && path.exists() {
                return Err(format!(
                    "A file outside the current Git file list occupies: {}",
                    change.path
                ));
            }
        }
        let token = digest(&serde_json::to_vec(&(&saved, &current)).map_err(error)?);
        Ok(FilePlan {
            id,
            saved,
            current,
            files,
            token,
        })
    }

    pub(crate) fn save_backup(
        &self,
        chat: &str,
        message: &str,
        plan: &FilePlan,
    ) -> Result<String, String> {
        let id = format!("backup-{}", crate::new_id());
        let saved =
            self.save_snapshot(&id, chat, message, &plan.current.root, true, Some(&plan.id))?;
        if serde_json::to_vec(&saved).map_err(error)?
            != serde_json::to_vec(&plan.current).map_err(error)?
        {
            return Err(
                "Files changed after confirmation. Refresh the preview; no files were restored."
                    .into(),
            );
        }
        Ok(id)
    }

    fn write_file(
        connection: &Connection,
        root: &Path,
        path: &str,
        expected: Option<&File>,
        desired: Option<&File>,
    ) -> Result<(), String> {
        let absolute = safe_path(root, path)?;
        let current = read_file(root, path)?.map(|(file, _, _)| file);
        if current.as_ref() != expected {
            return Err(format!("File changed during restore: {path}"));
        }
        let Some(desired) = desired else {
            if current.is_some() {
                std::fs::remove_file(absolute).map_err(error)?;
            }
            return Ok(());
        };
        let bytes: Vec<u8> = connection
            .query_row(
                "SELECT data FROM blobs WHERE hash=?1",
                [&desired.hash],
                |row| row.get(0),
            )
            .map_err(error)?;
        if bytes.len() > MAX_FILE_BYTES || digest(&bytes) != desired.hash {
            return Err("Checkpoint content is missing or corrupt".into());
        }
        let parent = absolute.parent().ok_or("Invalid restore path")?;
        std::fs::create_dir_all(parent).map_err(error)?;
        safe_path(root, path)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(error)?;
        temporary.write_all(&bytes).map_err(error)?;
        temporary.as_file().sync_all().map_err(error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(std::fs::Permissions::from_mode(desired.mode))
                .map_err(error)?;
        }
        #[cfg(not(unix))]
        {
            let mut permissions = temporary.as_file().metadata().map_err(error)?.permissions();
            permissions.set_readonly(desired.mode & 0o222 == 0);
            temporary
                .as_file()
                .set_permissions(permissions)
                .map_err(error)?;
        }
        safe_path(root, path)?;
        if read_file(root, path)?.map(|(file, _, _)| file).as_ref() != expected {
            return Err(format!("File changed during restore: {path}"));
        }
        temporary.persist(&absolute).map_err(error)?;
        Ok(())
    }

    pub(crate) fn apply(&self, plan: &FilePlan) -> Result<(), String> {
        if head(&plan.saved.root)? != plan.current.head
            || reference(&plan.saved.git_dir)? != plan.current.reference
        {
            return Err(
                "Git HEAD changed during restore preparation; no files were changed".into(),
            );
        }
        let connection = self.open()?;
        let mut applied: Vec<&CheckpointFileChange> = Vec::new();
        for change in &plan.files {
            if let Err(reason) = Self::write_file(
                &connection,
                &plan.saved.root,
                &change.path,
                plan.current.files.get(&change.path),
                plan.saved.files.get(&change.path),
            ) {
                let mut failures = Vec::new();
                for change in applied.into_iter().rev() {
                    if let Err(failure) = Self::write_file(
                        &connection,
                        &plan.saved.root,
                        &change.path,
                        plan.saved.files.get(&change.path),
                        plan.current.files.get(&change.path),
                    ) {
                        failures.push(failure);
                    }
                }
                return Err(if failures.is_empty() {
                    format!(
                        "{reason}. Applied files were rolled back; the backup remains available."
                    )
                } else {
                    format!(
                        "{reason}. Some files could not be rolled back: {}. Restore the saved backup after stopping other writers.",
                        failures.join("; ")
                    )
                });
            }
            applied.push(change);
        }
        Ok(())
    }

    pub(crate) fn previous_restore(
        &self,
        id: &str,
        request: &str,
    ) -> Result<Option<RestoreCheckpointResult>, String> {
        let connection = self.open()?;
        let row: Option<(String, Option<String>, Option<String>)> = connection
            .query_row(
                "SELECT request,backup,result FROM restores WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(error)?;
        match row {
            Some((previous, _, _)) if previous != request => {
                Err("Restore operation id was already used for a different request".into())
            }
            Some((_, _, Some(result))) => serde_json::from_str(&result).map(Some).map_err(error),
            Some((_, backup_id, None)) => {
                let result = RestoreCheckpointResult {
                    chat_id: None, chat: None, backup_id,
                    error: Some("A previous restore was interrupted. Inspect files and use its backup before starting another restore.".into()),
                };
                self.finish_restore(id, &result)?;
                Ok(Some(result))
            }
            None => Ok(None),
        }
    }

    pub(crate) fn begin_restore(
        &self,
        id: &str,
        request: &str,
        backup: Option<&str>,
    ) -> Result<(), String> {
        let connection = self.open()?;
        let unfinished = {
            let mut statement = connection
                .prepare("SELECT id,backup FROM restores WHERE result IS NULL")
                .map_err(error)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })
                .map_err(error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(error)?
        };
        for (old_id, backup_id) in unfinished {
            self.finish_restore(&old_id, &RestoreCheckpointResult {
                chat_id: None, chat: None, backup_id,
                error: Some("Restore was interrupted. Use its recovery backup to inspect or restore files.".into()),
            })?;
        }
        connection.execute("DELETE FROM restores WHERE id IN (SELECT id FROM restores WHERE result IS NOT NULL ORDER BY created DESC LIMIT -1 OFFSET 999)", []).map_err(error)?;
        connection
            .execute(
                "INSERT INTO restores(id,request,created,backup) VALUES(?1,?2,?3,?4)",
                params![id, request, crate::now_ms(), backup],
            )
            .map_err(error)?;
        Ok(())
    }

    pub(crate) fn finish_restore(
        &self,
        id: &str,
        result: &RestoreCheckpointResult,
    ) -> Result<(), String> {
        self.open()?
            .execute(
                "UPDATE restores SET result=?2 WHERE id=?1",
                params![id, serde_json::to_string(result).map_err(error)?],
            )
            .map_err(error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EngineCore, HarnessRegistry};

    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, EngineCore) {
        let data = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        git(project.path(), &["init", "-q"]).unwrap();
        std::fs::write(project.path().join("app.txt"), "base").unwrap();
        std::fs::write(project.path().join(".gitignore"), "ignored/\n").unwrap();
        git(project.path(), &["add", "app.txt", ".gitignore"]).unwrap();
        git(
            project.path(),
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
        )
        .unwrap();
        let core = EngineCore::assemble(
            data.path(),
            Arc::new(HarnessRegistry::new()),
            zeron_proto::HarnessId::Mock,
            None,
        )
        .unwrap();
        (data, project, core)
    }

    #[tokio::test]
    async fn restores_regular_files_keeps_ignored_files_and_never_changes_git_index() {
        let (_data, project, core) = fixture();
        let store = &core.checkpoints;
        std::fs::write(project.path().join("untracked.txt"), "before").unwrap();
        store
            .save_snapshot(
                &key("chat", "user"),
                "chat",
                "user",
                project.path(),
                false,
                None,
            )
            .unwrap();
        std::fs::write(project.path().join("app.txt"), "agent edit").unwrap();
        std::fs::remove_file(project.path().join("untracked.txt")).unwrap();
        std::fs::write(project.path().join("new.txt"), "agent created").unwrap();
        std::fs::create_dir(project.path().join("ignored")).unwrap();
        std::fs::write(project.path().join("ignored/keep.txt"), "do not change").unwrap();
        git(project.path(), &["add", "app.txt"]).unwrap();
        let index = std::fs::read(project.path().join(".git/index")).unwrap();
        let before_head = head(project.path()).unwrap();
        let plan = store.plan("chat", "user", None, project.path()).unwrap();
        assert_eq!(plan.files.len(), 3);
        let backup = store.save_backup("chat", "user", &plan).unwrap();
        store.apply(&plan).unwrap();
        assert_eq!(
            std::fs::read_to_string(project.path().join("app.txt")).unwrap(),
            "base"
        );
        assert_eq!(
            std::fs::read_to_string(project.path().join("untracked.txt")).unwrap(),
            "before"
        );
        assert!(!project.path().join("new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(project.path().join("ignored/keep.txt")).unwrap(),
            "do not change"
        );
        assert_eq!(
            std::fs::read(project.path().join(".git/index")).unwrap(),
            index
        );
        assert_eq!(head(project.path()).unwrap(), before_head);
        let recovery = store
            .plan("chat", "user", Some(&backup), project.path())
            .unwrap();
        store.save_backup("chat", "user", &recovery).unwrap();
        store.apply(&recovery).unwrap();
        assert_eq!(
            std::fs::read_to_string(project.path().join("app.txt")).unwrap(),
            "agent edit"
        );
        assert!(project.path().join("new.txt").exists());
        assert!(!project.path().join("untracked.txt").exists());
        assert!(
            store
                .plan("another-chat", "user", Some(&backup), project.path())
                .is_err()
        );
        core.shutdown().await;
    }

    #[tokio::test]
    async fn a_changed_file_cannot_be_overwritten_and_partial_work_is_rolled_back() {
        let (_data, project, core) = fixture();
        let store = &core.checkpoints;
        std::fs::write(project.path().join("z.txt"), "before z").unwrap();
        store
            .save_snapshot(
                &key("chat", "user"),
                "chat",
                "user",
                project.path(),
                false,
                None,
            )
            .unwrap();
        std::fs::write(project.path().join("app.txt"), "after app").unwrap();
        std::fs::write(project.path().join("z.txt"), "after z").unwrap();
        let plan = store.plan("chat", "user", None, project.path()).unwrap();
        store.save_backup("chat", "user", &plan).unwrap();
        std::fs::write(project.path().join("z.txt"), "external edit").unwrap();
        assert!(store.apply(&plan).unwrap_err().contains("rolled back"));
        assert_eq!(
            std::fs::read_to_string(project.path().join("app.txt")).unwrap(),
            "after app"
        );
        assert_eq!(
            std::fs::read_to_string(project.path().join("z.txt")).unwrap(),
            "external edit"
        );
        core.shutdown().await;
    }

    #[tokio::test]
    async fn new_commits_or_a_different_checkout_refuse_file_restore() {
        let (_data, project, core) = fixture();
        let store = &core.checkpoints;
        store
            .save_snapshot(
                &key("chat", "user"),
                "chat",
                "user",
                project.path(),
                false,
                None,
            )
            .unwrap();
        let other = tempfile::tempdir().unwrap();
        assert!(
            store
                .plan("chat", "user", None, other.path())
                .unwrap_err()
                .contains("different project")
        );
        std::fs::write(project.path().join("app.txt"), "committed change").unwrap();
        git(project.path(), &["add", "app.txt"]).unwrap();
        git(
            project.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--no-gpg-sign",
                "-qm",
                "new head",
            ],
        )
        .unwrap();
        assert!(
            store
                .plan("chat", "user", None, project.path())
                .unwrap_err()
                .contains("HEAD changed")
        );
        assert_eq!(
            std::fs::read_to_string(project.path().join("app.txt")).unwrap(),
            "committed change"
        );
        core.shutdown().await;
    }

    #[tokio::test]
    async fn snapshots_are_deduplicated_and_retained_with_explicit_limits() {
        let (_data, project, core) = fixture();
        let store = &core.checkpoints;
        for index in 0..55 {
            let message = format!("user-{index}");
            store
                .save_snapshot(
                    &key("chat", &message),
                    "chat",
                    &message,
                    project.path(),
                    false,
                    None,
                )
                .unwrap();
        }
        let connection = store.open().unwrap();
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM checkpoints", [], |row| row.get(0))
            .unwrap();
        let blobs: i64 = connection
            .query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 50);
        assert_eq!(blobs, 2);
        assert!(store.plan("chat", "user-0", None, project.path()).is_err());
        assert!(store.plan("chat", "user-54", None, project.path()).is_ok());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&store.database)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        core.shutdown().await;
    }

    #[tokio::test]
    async fn recovery_keeps_the_original_checkpoint_and_never_captures_partly_edited_files() {
        let (_data, project, core) = fixture();
        let store = &core.checkpoints;
        let cwd = project.path().to_str().unwrap();
        let guard = store.gate.clone().lock_owned().await;
        drop(store.capture("chat", "saved", cwd, None, guard).await);
        std::fs::write(project.path().join("app.txt"), "partial edit").unwrap();
        for message in ["saved", "missing"] {
            let guard = store.gate.clone().lock_owned().await;
            drop(
                store
                    .capture("chat", message, cwd, Some("resumed turn"), guard)
                    .await,
            );
        }
        let plan = store.plan("chat", "saved", None, project.path()).unwrap();
        assert_eq!(plan.files.len(), 1);
        let guard = store.gate.clone().lock_owned().await;
        drop(store.capture("chat", "missing", cwd, None, guard).await);
        assert!(
            store
                .plan("chat", "missing", None, project.path())
                .unwrap_err()
                .contains("resumed turn")
        );
        core.shutdown().await;
    }

    #[tokio::test]
    async fn interrupted_operations_remain_recoverable_and_cannot_be_replayed_as_new_requests() {
        let (_data, _project, core) = fixture();
        let store = &core.checkpoints;
        store
            .begin_restore("operation", "original request", Some("backup-id"))
            .unwrap();
        let result = store
            .previous_restore("operation", "original request")
            .unwrap()
            .unwrap();
        assert!(result.error.unwrap().contains("interrupted"));
        assert_eq!(result.backup_id.as_deref(), Some("backup-id"));
        assert!(
            store
                .previous_restore("operation", "changed request")
                .is_err()
        );
        core.shutdown().await;
    }

    #[test]
    fn unsafe_paths_never_reach_the_filesystem() {
        for path in [
            "",
            "../outside",
            "/absolute",
            ".git/config",
            "a/.GiT/config",
            "a\\outside",
        ] {
            assert!(validate_path(path).is_err(), "{path}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_aliases_and_alternate_streams_are_not_checkpoint_paths() {
        for path in [
            "file:stream",
            "a/NUL.txt",
            "COM1",
            "a/trailing.",
            "a/trailing ",
        ] {
            assert!(validate_path(path).is_err(), "{path}");
        }
        assert!(validate_path("src/component.rs").is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_and_oversized_files_do_not_produce_a_usable_snapshot() {
        use std::os::unix::fs::symlink;
        let (_data, project, core) = fixture();
        let outside = tempfile::NamedTempFile::new().unwrap();
        symlink(outside.path(), project.path().join("link.txt")).unwrap();
        assert!(
            core.checkpoints
                .save_snapshot("symlink", "chat", "user", project.path(), false, None)
                .unwrap_err()
                .contains("Symlinks")
        );
        std::fs::remove_file(project.path().join("link.txt")).unwrap();
        std::fs::File::create(project.path().join("large.bin"))
            .unwrap()
            .set_len(MAX_FILE_BYTES as u64 + 1)
            .unwrap();
        assert!(
            core.checkpoints
                .save_snapshot("large", "chat", "user", project.path(), false, None)
                .unwrap_err()
                .contains("8 MiB")
        );
        let count: i64 = core
            .checkpoints
            .open()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM checkpoints", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        core.shutdown().await;
    }
}
