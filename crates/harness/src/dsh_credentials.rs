//! The DeepSeek API key DeepSeek Harness uses, edited from Settings.
//!
//! DSH keeps credentials in `$DSH_HOME/.credentials.yaml` (default
//! `~/.dsh`), shared by every profile, and its saved key outranks the
//! environment. So Zeron edits that file rather than keeping a copy of its
//! own: one key serves the ACP profile Zeron launches and `dsh web` alike.
//! Writes take DSH's own cross-process writer lock (a `wx`-created
//! `.credentials.yaml.lock` holding the writer's pid), keep every other entry,
//! and stay 0600 because DSH refuses to start on a wider mode. A running DSH
//! hot-reloads the file. Settings only ever receives the key's last four
//! characters.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};

const FILE: &str = ".credentials.yaml";
/// The credential reference DSH's `deepseek-official` provider reads.
const REF: &str = "DEEPSEEK_API_KEY";
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// What Settings sees about the saved key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DshKeyStatus {
    /// The key's last four characters; `None` when no key is saved.
    pub key_hint: Option<String>,
    /// The credentials file, for display.
    pub path: String,
}

/// `SaveDshApiKey` params: a new key, or `None`/empty to remove the saved one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DshKeyInput {
    pub api_key: Option<String>,
}

/// `$DSH_HOME`, else `~/.dsh`.
fn home() -> Option<PathBuf> {
    std::env::var_os("DSH_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::executable::home_dir().map(|home| home.join(".dsh")))
}

fn path() -> Result<PathBuf, String> {
    home()
        .map(|home| home.join(FILE))
        .ok_or_else(|| "couldn't find the DeepSeek Harness home directory".to_owned())
}

pub fn status() -> DshKeyStatus {
    let Ok(path) = path() else {
        return DshKeyStatus::default();
    };
    status_at(&path)
}

/// Save or remove the key, returning the resulting status.
pub fn save(input: DshKeyInput) -> Result<DshKeyStatus, String> {
    let key = input
        .api_key
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty());
    if let Some(key) = &key
        && (key.len() < 8 || key.chars().any(char::is_whitespace))
    {
        return Err("enter a valid DeepSeek API key".into());
    }
    let path = path()?;
    save_at(&path, key.as_deref())?;
    Ok(status_at(&path))
}

fn status_at(path: &Path) -> DshKeyStatus {
    let key = read(path)
        .ok()
        .and_then(|doc| stored_key(&doc).map(str::to_owned));
    DshKeyStatus {
        key_hint: key.map(|key| hint(&key)),
        path: path.display().to_string(),
    }
}

fn stored_key(doc: &Value) -> Option<&str> {
    doc.get("refs")?
        .get(REF)?
        .as_str()
        .filter(|key| !key.is_empty())
}

fn hint(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    chars[chars.len().saturating_sub(4)..].iter().collect()
}

/// The parsed document; a missing file reads as empty.
fn read(path: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) if raw.trim().is_empty() => Ok(Value::Mapping(Mapping::new())),
        Ok(raw) => serde_yaml_ng::from_str(&raw)
            .map_err(|error| format!("couldn't read {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(Value::Mapping(Mapping::new()))
        }
        Err(error) => Err(format!("couldn't read {}: {error}", path.display())),
    }
}

fn save_at(path: &Path, key: Option<&str>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("couldn't create {}: {error}", parent.display()))?;
    }
    let _lock = Lock::acquire(path)?;
    let mut doc = read(path)?;
    let Value::Mapping(root) = &mut doc else {
        return Err(format!("{} is not a YAML mapping", path.display()));
    };
    root.entry("version".into()).or_insert(Value::from(1));
    let refs = root
        .entry("refs".into())
        .or_insert_with(|| Value::Mapping(Mapping::new()));
    if refs.is_null() {
        *refs = Value::Mapping(Mapping::new());
    }
    let Value::Mapping(refs) = refs else {
        return Err(format!("{}: `refs` is not a mapping", path.display()));
    };
    match key {
        Some(key) => {
            refs.insert(REF.into(), key.into());
        }
        None => {
            refs.remove(REF);
        }
    }
    let yaml = serde_yaml_ng::to_string(&doc).map_err(|error| error.to_string())?;
    write(path, yaml.as_bytes())
        .map_err(|error| format!("couldn't save {}: {error}", path.display()))
}

/// Write through a 0600 staging file so a crash never leaves half a file.
fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let staging = path.with_extension(format!("yaml.tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = private_file(&staging, false)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&staging, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    result
}

fn private_file(path: &Path, create_new: bool) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// DSH's writer lock on the credentials file, released on drop.
struct Lock(PathBuf);

impl Lock {
    fn acquire(file: &Path) -> Result<Self, String> {
        let mut name = file.as_os_str().to_owned();
        name.push(".lock");
        let lock = PathBuf::from(name);
        let deadline = Instant::now() + LOCK_WAIT;
        let mut delay = Duration::from_millis(10);
        loop {
            match private_file(&lock, true) {
                Ok(mut handle) => {
                    let _ = writeln!(handle, "{}", std::process::id());
                    return Ok(Self(lock));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if holder_exited(&lock) {
                        let _ = std::fs::remove_file(&lock);
                        continue;
                    }
                }
                Err(error) => return Err(format!("couldn't lock {}: {error}", file.display())),
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "DeepSeek Harness is writing {}; try again in a moment",
                    file.display()
                ));
            }
            std::thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_millis(250));
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Whether a lock's recorded writer is gone (it crashed holding the lock).
/// Only checkable on Linux; elsewhere a lock is always presumed live.
fn holder_exited(lock: &Path) -> bool {
    let Some(pid) = std::fs::read_to_string(lock)
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
    else {
        return false;
    };
    cfg!(target_os = "linux") && !Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_removes_the_key_keeping_other_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(
            &path,
            "version: 1\nrecords:\n  client-connection/browser-session:\n    kind: grant\nrefs:\n  OTHER_KEY: keep\n",
        )
        .unwrap();
        save_at(&path, Some("sk-test-12345678")).unwrap();
        let doc = read(&path).unwrap();
        assert_eq!(stored_key(&doc), Some("sk-test-12345678"));
        assert_eq!(doc["refs"]["OTHER_KEY"].as_str(), Some("keep"));
        assert_eq!(
            doc["records"]["client-connection/browser-session"]["kind"].as_str(),
            Some("grant")
        );
        assert_eq!(status_at(&path).key_hint.as_deref(), Some("5678"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        save_at(&path, None).unwrap();
        let doc = read(&path).unwrap();
        assert_eq!(stored_key(&doc), None);
        assert_eq!(doc["refs"]["OTHER_KEY"].as_str(), Some("keep"));
        assert!(!path.with_extension("yaml.lock").exists());
    }

    #[test]
    fn creates_the_file_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("home").join(FILE);
        assert_eq!(status_at(&path).key_hint, None);
        save_at(&path, Some("sk-abcdefgh")).unwrap();
        let doc = read(&path).unwrap();
        assert_eq!(doc["version"].as_u64(), Some(1));
        assert_eq!(stored_key(&doc), Some("sk-abcdefgh"));
    }

    #[test]
    fn a_crashed_writers_lock_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let lock = dir.path().join(format!("{FILE}.lock"));
        // A pid far past any live process on the test box.
        std::fs::write(&lock, "4194300\n").unwrap();
        if cfg!(target_os = "linux") {
            save_at(&path, Some("sk-abcdefgh")).unwrap();
            assert!(!lock.exists());
        }
    }
}
