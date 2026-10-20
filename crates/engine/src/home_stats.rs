//! Home panel readings this device reports about itself: health, recent git
//! activity in its projects, and the latest CI runs of their GitHub repos.
//! Everything here is read-only; the UI polls it while Home is visible.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use zeron_proto::{
    CiRepo, CiRun, CiRunsQuery, DeviceHealth, GitActivityCommit, GitActivityQuery, GitActivityRepo,
    ListeningService, ListeningServicesQuery,
};

/// Paths per request and history depth, so a stale or hostile caller can't
/// make the engine walk unbounded history.
const MAX_PATHS: usize = 64;
const MAX_DAYS: u32 = 90;
const MAX_COMMITS: &str = "--max-count=5000";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
/// `gh` calls hit the network and GitHub's rate limit; every viewer shares one.
const CI_CACHE_TTL: Duration = Duration::from_secs(60);
const CI_RUNS_PER_REPO: &str = "3";

static STARTED_AT: OnceLock<i64> = OnceLock::new();

/// Stamp the engine's start; called once at boot.
pub fn mark_started() {
    STARTED_AT.get_or_init(|| chrono::Utc::now().timestamp());
}

/// Kept between readings: CPU use is measured as the change since the last one.
static SYSTEM: Mutex<Option<(sysinfo::System, Instant)>> = Mutex::new(None);

pub async fn device_health(data_dir: &Path) -> DeviceHealth {
    let data_dir = data_dir.to_path_buf();
    tokio::task::spawn_blocking(move || read_health(&data_dir))
        .await
        .unwrap_or_default()
}

fn read_health(data_dir: &Path) -> DeviceHealth {
    let mut guard = SYSTEM
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (system, sampled) = guard.get_or_insert_with(|| {
        let mut system = sysinfo::System::new();
        system.refresh_cpu_usage();
        (system, Instant::now())
    });
    // The first reading needs a second CPU sample to compare against.
    let wait = sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.saturating_sub(sampled.elapsed());
    if !wait.is_zero() {
        std::thread::sleep(wait);
    }
    system.refresh_cpu_usage();
    *sampled = Instant::now();
    system.refresh_memory();
    let (disk_total, disk_used) = disk_usage(data_dir).unzip();
    DeviceHealth {
        cpu_percent: system.global_cpu_usage(),
        cpu_count: system.cpus().len() as u32,
        memory_total: system.total_memory(),
        memory_used: system.used_memory(),
        swap_total: system.total_swap(),
        swap_used: system.used_swap(),
        disk_total,
        disk_used,
        memory_pressure: memory_pressure(),
        engine_version: env!("CARGO_PKG_VERSION").to_owned(),
        engine_started_at: STARTED_AT.get().copied(),
        read_at: chrono::Utc::now().timestamp(),
    }
}

/// `(total, used)` bytes of the filesystem holding `path`, where used is
/// what an unprivileged user can no longer write.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
fn disk_usage(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `stat` is a valid out pointer.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let block = stat.f_frsize as u64;
    let total = stat.f_blocks as u64 * block;
    let available = stat.f_bavail as u64 * block;
    Some((total, total.saturating_sub(available)))
}

#[cfg(windows)]
fn disk_usage(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let (mut available, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: `wide` is NUL-terminated and the out pointers are valid.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            &mut total,
            &mut free,
        )
    };
    (ok != 0).then(|| (total, total.saturating_sub(available)))
}

#[cfg(not(any(unix, windows)))]
fn disk_usage(_: &Path) -> Option<(u64, u64)> {
    None
}

#[cfg(target_os = "linux")]
fn memory_pressure() -> Option<f32> {
    parse_pressure(&std::fs::read_to_string("/proc/pressure/memory").ok()?)
}

#[cfg(not(target_os = "linux"))]
fn memory_pressure() -> Option<f32> {
    None
}

/// `some avg10=…` from a PSI file.
fn parse_pressure(text: &str) -> Option<f32> {
    text.lines()
        .find(|line| line.starts_with("some "))?
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse()
        .ok()
}

/// Run a git or gh command with a deadline, returning stdout on success.
async fn output(program: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
    let mut command = tokio::process::Command::new(program);
    if program == "gh" {
        zeron_harness::compose_login_shell_path(&mut command);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.as_std_mut().creation_flags(0x08000000);
    }
    command
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(COMMAND_TIMEOUT, command.output())
        .await
        .map_err(|_| format!("{program} timed out"))?
        .map_err(|error| format!("{program} failed to start: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        return Err(if message.is_empty() {
            format!("{program} failed ({})", output.status)
        } else {
            message.lines().next().unwrap_or(message).to_owned()
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Absolute existing folders only, deduplicated and capped.
fn usable_paths(paths: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        .filter(|path| Path::new(path).is_absolute() && Path::new(path).is_dir())
        .filter(|path| seen.insert(path.clone()))
        .take(MAX_PATHS)
        .collect()
}

pub async fn git_activity(query: GitActivityQuery) -> Vec<GitActivityRepo> {
    let days = query.days.clamp(1, MAX_DAYS);
    let reads = usable_paths(query.paths)
        .into_iter()
        .map(|path| async move {
            match repo_activity(Path::new(&path), days).await {
                Ok(commits) => GitActivityRepo {
                    path,
                    commits,
                    error: None,
                },
                Err(error) => GitActivityRepo {
                    path,
                    commits: Vec::new(),
                    error: Some(error),
                },
            }
        });
    futures::future::join_all(reads).await
}

async fn repo_activity(path: &Path, days: u32) -> Result<Vec<GitActivityCommit>, String> {
    // Count only the device user's commits, so merging someone else's
    // history doesn't flood the chart. No configured email counts everyone.
    let email = output("git", &["config", "user.email"], path)
        .await
        .unwrap_or_default();
    let email = email.trim();
    let since = format!("--since={days}.days.ago");
    let author = format!("--author={email}");
    let mut args = vec![
        "log",
        "--branches",
        "--no-merges",
        "--no-color",
        "--numstat",
        "--format=%x1e%H%x1f%at",
        MAX_COMMITS,
        since.as_str(),
    ];
    if !email.is_empty() {
        args.extend(["--fixed-strings", author.as_str()]);
    }
    Ok(parse_log(&output("git", &args, path).await?))
}

/// Parse `--format=%x1e%H%x1f%at --numstat`. Binary files (`-`) add nothing.
fn parse_log(text: &str) -> Vec<GitActivityCommit> {
    text.split('\u{1e}')
        .filter_map(|record| {
            let mut lines = record.lines();
            let (hash, at) = lines.next()?.split_once('\u{1f}')?;
            let mut commit = GitActivityCommit {
                hash: hash.trim().to_owned(),
                at: at.trim().parse().ok()?,
                additions: 0,
                deletions: 0,
            };
            for line in lines {
                let mut fields = line.split('\t');
                if let (Some(added), Some(deleted)) = (fields.next(), fields.next()) {
                    commit.additions += added.parse::<u64>().unwrap_or(0);
                    commit.deletions += deleted.parse::<u64>().unwrap_or(0);
                }
            }
            (!commit.hash.is_empty()).then_some(commit)
        })
        .collect()
}

/// `owner/name` of a GitHub remote URL (https, ssh, or scp-like).
fn github_slug(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.split_once("github.com/").map(|(_, rest)| rest))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = rest.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    (!owner.is_empty() && !name.is_empty() && parts.next().is_none())
        .then(|| format!("{owner}/{name}"))
}

type CiCache = HashMap<String, (Instant, Result<Vec<CiRun>, String>)>;
static CI_CACHE: Mutex<Option<CiCache>> = Mutex::new(None);

pub async fn ci_runs(query: CiRunsQuery) -> Vec<CiRepo> {
    // Read each folder's `origin`; folders without a GitHub origin are skipped.
    let slugs = futures::future::join_all(usable_paths(query.paths).into_iter().map(
        |path| async move {
            let url = output("git", &["remote", "get-url", "origin"], Path::new(&path))
                .await
                .ok()?;
            Some((github_slug(&url)?, path))
        },
    ))
    .await;
    let mut repos: Vec<CiRepo> = Vec::new();
    for (repo, path) in slugs.into_iter().flatten() {
        match repos.iter_mut().find(|entry| entry.repo == repo) {
            Some(entry) => entry.paths.push(path),
            None => repos.push(CiRepo {
                repo,
                paths: vec![path],
                ..CiRepo::default()
            }),
        }
    }
    let reads = repos.into_iter().map(|mut entry| async move {
        match repo_runs(&entry.repo, Path::new(&entry.paths[0])).await {
            Ok(runs) => entry.runs = runs,
            Err(error) => entry.error = Some(error),
        }
        entry
    });
    futures::future::join_all(reads).await
}

async fn repo_runs(repo: &str, cwd: &Path) -> Result<Vec<CiRun>, String> {
    let cached = CI_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .and_then(|cache| cache.get(repo))
        .filter(|(at, _)| at.elapsed() < CI_CACHE_TTL)
        .map(|(_, result)| result.clone());
    if let Some(result) = cached {
        return result;
    }
    let result = output(
        "gh",
        &[
            "run",
            "list",
            "--repo",
            repo,
            "--limit",
            CI_RUNS_PER_REPO,
            "--json",
            "databaseId,workflowName,displayTitle,headBranch,status,conclusion,event,createdAt,updatedAt,url",
        ],
        cwd,
    )
    .await
    .and_then(|text| parse_runs(&text));
    CI_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(repo.to_owned(), (Instant::now(), result.clone()));
    result
}

fn parse_runs(text: &str) -> Result<Vec<CiRun>, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct GhRun {
        #[serde(default)]
        database_id: u64,
        #[serde(default)]
        workflow_name: String,
        #[serde(default)]
        display_title: String,
        #[serde(default)]
        head_branch: String,
        #[serde(default)]
        status: String,
        #[serde(default)]
        conclusion: String,
        #[serde(default)]
        event: String,
        #[serde(default)]
        created_at: String,
        #[serde(default)]
        updated_at: String,
        #[serde(default)]
        url: String,
    }
    let runs: Vec<GhRun> =
        serde_json::from_str(text).map_err(|error| format!("gh output: {error}"))?;
    Ok(runs
        .into_iter()
        .map(|run| CiRun {
            id: run.database_id,
            workflow: run.workflow_name,
            title: run.display_title,
            branch: run.head_branch,
            status: run.status,
            conclusion: (!run.conclusion.is_empty()).then_some(run.conclusion),
            event: run.event,
            created_at: run.created_at,
            updated_at: run.updated_at,
            url: run.url,
        })
        .collect())
}

/// The device user's listening TCP ports, labelled with the project folder
/// each process runs in. Listing only reads process tables; nothing is probed.
pub async fn listening_services(query: ListeningServicesQuery) -> Vec<ListeningService> {
    let roots = usable_paths(query.paths);
    let listeners = tokio::task::spawn_blocking(zeron_preview::discovery::listeners)
        .await
        .unwrap_or_default();
    services(listeners, &roots, query.all_ports)
}

fn services(
    listeners: Vec<zeron_preview::discovery::Listener>,
    roots: &[String],
    all_ports: bool,
) -> Vec<ListeningService> {
    let mut seen = std::collections::HashSet::new();
    let mut services: Vec<ListeningService> = listeners
        .into_iter()
        // One row per process and port, whatever addresses it binds.
        .filter(|listener| seen.insert((listener.pid, listener.address.port())))
        .filter_map(|listener| {
            // The deepest root wins, so a worktree inside its repo is named.
            let path = roots
                .iter()
                .filter(|root| listener.belongs_to(Path::new(root)))
                .max_by_key(|root| root.len())
                .cloned();
            if path.is_none() && !all_ports {
                return None;
            }
            let (framework, rank) = zeron_preview::discovery::framework(&listener.args);
            Some(ListeningService {
                port: listener.address.port(),
                pid: listener.pid,
                command: listener
                    .args
                    .first()
                    .and_then(|arg| Path::new(arg).file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                framework: (rank >= 80).then(|| framework.to_owned()),
                path,
            })
        })
        .collect();
    services.sort_by_key(|service| (service.path.is_none(), service.port));
    services
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numstat_log_records() {
        let text = "\u{1e}aaa\u{1f}100\n\n3\t1\tsrc/a.rs\n-\t-\timg.png\n\
                    \u{1e}bbb\u{1f}200\n\u{1e}ccc\u{1f}300\n\n10\t0\tb.rs\n2\t5\tc.rs\n";
        let commits = parse_log(text);
        assert_eq!(commits.len(), 3);
        assert_eq!((commits[0].additions, commits[0].deletions), (3, 1));
        assert_eq!((commits[1].hash.as_str(), commits[1].at), ("bbb", 200));
        assert_eq!((commits[2].additions, commits[2].deletions), (12, 5));
    }

    #[test]
    fn reads_github_slugs_from_remote_urls() {
        for url in [
            "https://github.com/trkshi/zeron.git\n",
            "git@github.com:trkshi/zeron.git",
            "ssh://git@github.com/trkshi/zeron",
            "https://github.com/trkshi/zeron/",
        ] {
            assert_eq!(github_slug(url).as_deref(), Some("trkshi/zeron"), "{url}");
        }
        assert_eq!(github_slug("https://gitlab.com/a/b.git"), None);
        assert_eq!(github_slug("https://github.com/only-owner"), None);
    }

    #[test]
    fn labels_listeners_by_their_deepest_project() {
        let listener = |pid, port, cwd: &str| zeron_preview::discovery::Listener {
            pid,
            parent: 1,
            cwd: cwd.into(),
            args: vec!["/usr/bin/node".into()],
            started_at: 0,
            address: std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            zeron_owned: false,
        };
        let roots = vec!["/work/app".to_owned(), "/work/app/.wt/feature".to_owned()];
        let listeners = vec![
            listener(10, 3000, "/work/app/web"),
            listener(10, 3000, "/work/app/web"),
            listener(11, 5173, "/work/app/.wt/feature"),
            listener(12, 8080, "/srv/other"),
        ];
        let projects = services(listeners.clone(), &roots, false);
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].path.as_deref(), Some("/work/app"));
        assert_eq!(projects[0].command, "node");
        assert_eq!(projects[1].path.as_deref(), Some("/work/app/.wt/feature"));
        let all = services(listeners, &roots, true);
        assert_eq!(all.len(), 3);
        assert_eq!((all[2].port, all[2].path.as_ref()), (8080, None));
    }

    #[test]
    fn parses_psi_and_gh_runs() {
        let psi = "some avg10=1.25 avg60=0.50 avg300=0.10 total=1\n\
                   full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        assert_eq!(parse_pressure(psi), Some(1.25));
        let runs = parse_runs(
            r#"[{"databaseId":7,"workflowName":"Build","displayTitle":"t","headBranch":"main",
                "status":"in_progress","conclusion":"","event":"workflow_dispatch",
                "createdAt":"2026-10-21T01:00:00Z","updatedAt":"2026-10-21T01:05:00Z",
                "url":"https://github.com/o/r/actions/runs/7"}]"#,
        )
        .unwrap();
        assert_eq!(runs[0].id, 7);
        assert_eq!(runs[0].conclusion, None);
    }
}
