//! Home panel readings a device reports about itself: its health, recent git
//! activity in its projects, and the latest CI runs of their GitHub repos.
use serde::{Deserialize, Serialize};

/// One device's load and capacity. Byte counts are absolute; the UI derives
/// percentages so differently sized devices compare honestly.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DeviceHealth {
    /// Average CPU use across all cores since the previous reading, 0–100.
    pub cpu_percent: f32,
    pub cpu_count: u32,
    pub memory_total: u64,
    pub memory_used: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    /// The filesystem holding the engine's data directory.
    pub disk_total: Option<u64>,
    pub disk_used: Option<u64>,
    /// Linux PSI `some avg10` for memory, 0–100: the share of the last ten
    /// seconds some task stalled on memory. What systemd-oomd acts on.
    pub memory_pressure: Option<f32>,
    pub engine_version: String,
    /// Unix seconds the engine process started, i.e. the last deploy or restart.
    pub engine_started_at: Option<i64>,
    pub read_at: i64,
}

/// `GetGitActivity` params: project folders on the target device, and how
/// many days back to read.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GitActivityQuery {
    pub paths: Vec<String>,
    pub days: u32,
}

/// The device user's own non-merge commits on a project's local branches.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GitActivityRepo {
    pub path: String,
    pub commits: Vec<GitActivityCommit>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GitActivityCommit {
    pub hash: String,
    /// Author time, Unix seconds.
    pub at: i64,
    pub additions: u64,
    pub deletions: u64,
}

/// `ListCiRuns` params: project folders on the target device.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CiRunsQuery {
    pub paths: Vec<String>,
}

/// Latest GitHub Actions runs of one repository, read with `gh`. Folders
/// sharing a repository (worktrees, clones) share one entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CiRepo {
    /// `owner/name` on GitHub.
    pub repo: String,
    pub paths: Vec<String>,
    pub runs: Vec<CiRun>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CiRun {
    pub id: u64,
    pub workflow: String,
    pub title: String,
    pub branch: String,
    /// `queued`, `in_progress`, `completed`, …
    pub status: String,
    /// Set once completed: `success`, `failure`, `cancelled`, …
    pub conclusion: Option<String>,
    pub event: String,
    /// RFC 3339.
    pub created_at: String,
    pub updated_at: String,
    pub url: String,
}

/// `ListListeningServices` params: project folders on the target device.
/// Without `all_ports`, only listeners started inside one of them are listed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ListeningServicesQuery {
    pub paths: Vec<String>,
    pub all_ports: bool,
}

/// A TCP port this user's process listens on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ListeningService {
    pub port: u16,
    pub pid: u32,
    /// Executable name only; arguments may carry credentials.
    pub command: String,
    /// A recognized dev server (`Next.js`, `Vite`, …).
    pub framework: Option<String>,
    /// The project folder the process runs in, when it runs in one.
    pub path: Option<String>,
}
