use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("could not resolve cache directory for the status file")]
    NoCacheDir,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub current: Option<CurrentRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentRun {
    pub issue_number: u64,
    pub issue_title: String,
    pub repo: String,
    pub claimed_at: DateTime<Utc>,
}

/// Resolve the platform-appropriate status-file path:
/// - Linux:   `~/.cache/bellows/status.json`
/// - macOS:   `~/Library/Caches/bellows/status.json`
/// - Windows: `%LOCALAPPDATA%\bellows\status.json`
///
/// Errors if the cache dir can't be resolved (rare — only on stripped-
/// down environments where neither `$XDG_CACHE_HOME` nor `$HOME` is set).
pub fn default_status_path() -> Result<PathBuf, StatusError> {
    let mut p = dirs::cache_dir().ok_or(StatusError::NoCacheDir)?;
    p.push("bellows");
    p.push("status.json");
    Ok(p)
}

/// Atomically write `status` to `path`. Writes to `<path>.tmp` first,
/// then renames over `path` so a concurrent `bellows status` reader
/// never sees a half-written file. Creates parent directories as
/// needed.
pub async fn write(path: &Path, status: &Status) -> Result<(), StatusError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = tmp_path_for(path);
    let json = serde_json::to_vec_pretty(status)?;
    tokio::fs::write(&tmp, &json).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// Read and parse a status file. Returns `Ok(None)` for a missing file,
/// `Ok(Some)` for present + parsed, `Err` for malformed JSON or other
/// IO errors.
pub async fn read(path: &Path) -> Result<Option<Status>, StatusError> {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            let status: Status = serde_json::from_slice(&bytes)?;
            Ok(Some(status))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Idempotent remove — a missing file is `Ok(())`, not an error.
pub async fn remove(path: &Path) -> Result<(), StatusError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Cross-platform PID liveness check.
///
/// On Unix (Linux/macOS), uses `libc::kill(pid, 0)` — sending signal 0
/// is the canonical "does this process exist and can I signal it"
/// probe; it doesn't actually deliver a signal. We treat both
/// success (the process exists and we have permission) and
/// `EPERM` (the process exists but is owned by another user) as
/// "alive". `ESRCH` and any other error map to "dead".
///
/// On Windows, shells out to `tasklist /FI "PID eq <pid>"` and
/// inspects whether a CSV row was returned. Shelling out avoids
/// pulling in `windows-sys` for a one-off liveness check called
/// once per `bellows status` invocation.
pub fn is_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // Finding #2 (review of PR #26): on Unix `libc::pid_t` is `i32`,
        // and `kill(-N, 0)` is POSIX-defined to probe process group N
        // rather than process N. A `u32` PID above `i32::MAX` would
        // silently get re-interpreted as a negative group probe. Real
        // PIDs from `std::process::id()` are far below this limit
        // (Linux's `pid_max` defaults to 32768; macOS caps at 99998),
        // so this only matters for a hand-edited / corrupted status
        // file — but reject it explicitly rather than silently doing
        // the wrong thing.
        if pid > i32::MAX as u32 {
            return false;
        }
        // SAFETY: libc::kill with sig=0 performs no action; it only
        // checks for the existence of the target process. The single
        // syscall has no preconditions and is safe to call from any
        // thread state.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == 0 {
            return true;
        }
        // SAFETY: errno is a thread-local lvalue; reading it after a
        // failed libc call is the standard pattern.
        let err = std::io::Error::last_os_error();
        // EPERM means "the process exists but you can't signal it"
        // — for our purposes that's still "alive".
        err.raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH", "/FO", "CSV"])
            .output();
        match output {
            Ok(o) if o.status.success() => {
                // When no process matches, tasklist prints either
                // "INFO: No tasks are running which match the specified criteria."
                // (older versions) or an empty result. When a process
                // matches, the CSV row begins with the image-name column,
                // which is wrapped in double quotes.
                let stdout = String::from_utf8_lossy(&o.stdout);
                stdout.contains('"')
            }
            _ => false,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        // Conservative default for unsupported platforms: treat the
        // process as alive so we don't spuriously claim a real
        // bellows-run is dead. The brief lists Windows + Unix as the
        // supported targets, so this branch is unreachable in practice.
        let _ = pid;
        true
    }
}

/// Persistent context for an orchestrator run — the immutable pid +
/// started_at carried across every status-file write. The polling
/// loop owns one of these and hands it to `run_once` so each
/// claim/finalise transition writes a status file with the same
/// orchestrator identity.
pub struct StatusContext {
    pub path: PathBuf,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
}

impl StatusContext {
    /// Build a context that writes to `path`, capturing the calling
    /// process's PID and the current time as the orchestrator's
    /// started_at.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            pid: std::process::id(),
            started_at: Utc::now(),
        }
    }

    pub async fn write_idle(&self) -> Result<(), StatusError> {
        write(
            &self.path,
            &Status {
                pid: self.pid,
                started_at: self.started_at,
                current: None,
            },
        )
        .await
    }

    pub async fn write_busy(&self, current: CurrentRun) -> Result<(), StatusError> {
        write(
            &self.path,
            &Status {
                pid: self.pid,
                started_at: self.started_at,
                current: Some(current),
            },
        )
        .await
    }
}

/// Format a one-paragraph human-readable summary of the status state
/// for `bellows status`. Pure function — accepts the parsed `Status`
/// (or `None` for a missing file) and a precomputed pid-liveness
/// boolean so the wording can be tested without spinning up a real
/// process.
pub fn summarise(status: Option<&Status>, pid_alive: bool) -> String {
    match status {
        None => "bellows is not running.".to_string(),
        Some(s) if !pid_alive => format!(
            "bellows is not running (stale status file from PID {} — may be a leftover from a crashed prior process).",
            s.pid,
        ),
        Some(s) => match &s.current {
            None => format!(
                "bellows is running (PID {}, started at {}), currently idle.",
                s.pid,
                s.started_at.to_rfc3339(),
            ),
            Some(c) => format!(
                "bellows is running (PID {}, started at {}), currently busy on issue #{} (\"{}\") in {}, claimed at {}.",
                s.pid,
                s.started_at.to_rfc3339(),
                c.issue_number,
                c.issue_title,
                c.repo,
                c.claimed_at.to_rfc3339(),
            ),
        },
    }
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_status() -> Status {
        Status {
            pid: 12345,
            started_at: DateTime::parse_from_rfc3339("2026-05-10T15:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            current: Some(CurrentRun {
                issue_number: 9,
                issue_title: "Smoke test: ...".to_string(),
                repo: "marad2001/bellows-test".to_string(),
                claimed_at: DateTime::parse_from_rfc3339("2026-05-10T15:02:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            }),
        }
    }

    #[tokio::test]
    async fn write_then_read_round_trips_all_fields() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("status.json");
        let status = sample_status();

        write(&path, &status).await.unwrap();
        let loaded = read(&path).await.unwrap().expect("file should exist");

        assert_eq!(loaded, status);
    }

    #[tokio::test]
    async fn write_idle_status_serialises_current_as_null() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("status.json");
        let status = Status {
            pid: 12345,
            started_at: DateTime::parse_from_rfc3339("2026-05-10T15:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            current: None,
        };

        write(&path, &status).await.unwrap();
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["pid"], 12345);
        assert_eq!(v["started_at"], "2026-05-10T15:00:00Z");
        assert!(v["current"].is_null(), "expected null, got: {}", v["current"]);
    }

    #[tokio::test]
    async fn write_busy_status_serialises_documented_schema() {
        // Pin the on-disk shape against the brief's schema so a future
        // serde rename/restructure can't silently break consumers.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("status.json");
        let status = sample_status();

        write(&path, &status).await.unwrap();
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["pid"], 12345);
        assert_eq!(v["started_at"], "2026-05-10T15:00:00Z");
        assert_eq!(v["current"]["issue_number"], 9);
        assert_eq!(v["current"]["issue_title"], "Smoke test: ...");
        assert_eq!(v["current"]["repo"], "marad2001/bellows-test");
        assert_eq!(v["current"]["claimed_at"], "2026-05-10T15:02:00Z");
    }

    #[tokio::test]
    async fn read_returns_none_for_missing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let loaded = read(&path).await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn read_returns_err_for_malformed_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("status.json");
        tokio::fs::write(&path, b"not json at all{{{").await.unwrap();
        let result = read(&path).await;
        assert!(result.is_err(), "expected Err, got {:?}", result);
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("deeper").join("status.json");
        write(&path, &sample_status()).await.unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn write_leaves_no_tmp_file_behind() {
        // After a successful write, the `.tmp` file used for the
        // atomic write+rename should no longer exist.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("status.json");
        write(&path, &sample_status()).await.unwrap();
        let tmp = tmp_path_for(&path);
        assert!(path.exists());
        assert!(!tmp.exists(), "leftover tmp file: {}", tmp.display());
    }

    #[tokio::test]
    async fn write_overwrites_existing_status_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("status.json");
        let mut s = sample_status();
        write(&path, &s).await.unwrap();
        s.pid = 99999;
        s.current = None;
        write(&path, &s).await.unwrap();
        let loaded = read(&path).await.unwrap().unwrap();
        assert_eq!(loaded.pid, 99999);
        assert!(loaded.current.is_none());
    }

    #[tokio::test]
    async fn remove_is_idempotent_for_missing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist.json");
        remove(&path).await.unwrap();
        // Calling twice still ok.
        remove(&path).await.unwrap();
    }

    #[tokio::test]
    async fn status_context_write_idle_then_busy_carries_same_pid_and_started_at() {
        // The polling-loop context is constructed once and reused
        // across many run_once calls. Both transitions must write
        // the same pid + started_at so the resulting status file
        // identifies the same orchestrator process throughout.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("status.json");
        let ctx = StatusContext {
            path: path.clone(),
            pid: 4242,
            started_at: DateTime::parse_from_rfc3339("2026-05-10T15:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };

        ctx.write_idle().await.unwrap();
        let idle = read(&path).await.unwrap().unwrap();
        assert_eq!(idle.pid, 4242);
        assert!(idle.current.is_none());

        ctx.write_busy(CurrentRun {
            issue_number: 7,
            issue_title: "T".to_string(),
            repo: "o/r".to_string(),
            claimed_at: DateTime::parse_from_rfc3339("2026-05-10T15:30:00Z")
                .unwrap()
                .with_timezone(&Utc),
        })
        .await
        .unwrap();
        let busy = read(&path).await.unwrap().unwrap();
        assert_eq!(busy.pid, 4242);
        assert_eq!(busy.started_at, idle.started_at);
        let current = busy.current.unwrap();
        assert_eq!(current.issue_number, 7);
        assert_eq!(current.repo, "o/r");
    }

    #[tokio::test]
    async fn remove_deletes_existing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("status.json");
        write(&path, &sample_status()).await.unwrap();
        assert!(path.exists());
        remove(&path).await.unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn default_status_path_ends_in_bellows_status_json() {
        // The exact prefix is platform-dependent, but the trailing
        // two segments are platform-invariant.
        let p = default_status_path().expect("cache dir resolves in test env");
        let mut iter = p.components().rev();
        assert_eq!(iter.next().unwrap().as_os_str(), "status.json");
        assert_eq!(iter.next().unwrap().as_os_str(), "bellows");
    }

    #[test]
    fn summarise_missing_file_says_not_running() {
        let s = summarise(None, false);
        assert_eq!(s, "bellows is not running.");
    }

    #[test]
    fn summarise_idle_running_mentions_pid_started_and_idle() {
        let mut st = sample_status();
        st.current = None;
        let s = summarise(Some(&st), true);
        assert!(s.contains("bellows is running"), "{}", s);
        assert!(s.contains("PID 12345"), "{}", s);
        assert!(s.contains("2026-05-10T15:00:00"), "{}", s);
        assert!(s.contains("currently idle"), "{}", s);
    }

    #[test]
    fn summarise_busy_running_mentions_issue_title_repo_and_claim_time() {
        let st = sample_status();
        let s = summarise(Some(&st), true);
        assert!(s.contains("bellows is running"), "{}", s);
        assert!(s.contains("PID 12345"), "{}", s);
        assert!(s.contains("issue #9"), "{}", s);
        assert!(s.contains("Smoke test: ..."), "{}", s);
        assert!(s.contains("marad2001/bellows-test"), "{}", s);
        assert!(s.contains("2026-05-10T15:02:00"), "{}", s);
        assert!(!s.contains("currently idle"), "{}", s);
    }

    #[test]
    fn summarise_stale_file_mentions_pid_and_crash_hint() {
        // PID-not-alive path: the user should see "not running" plus
        // the stale-PID note so they understand why a file is on disk.
        let st = sample_status();
        let s = summarise(Some(&st), false);
        assert!(s.contains("bellows is not running"), "{}", s);
        assert!(s.contains("stale"), "{}", s);
        assert!(s.contains("12345"), "{}", s);
        assert!(s.contains("crashed"), "{}", s);
    }

    #[test]
    fn is_pid_alive_returns_true_for_current_process() {
        assert!(is_pid_alive(std::process::id()));
    }

    #[test]
    fn is_pid_alive_rejects_pid_above_i32_max() {
        // Finding #2 fix: PIDs above i32::MAX can't be expressed as a
        // valid Unix `pid_t` (signed i32) — the previous cast would
        // reinterpret them as negative process-group probes. The
        // function should refuse such PIDs before the syscall.
        assert!(!is_pid_alive(u32::MAX));
        assert!(!is_pid_alive((i32::MAX as u32) + 1));
    }

    #[test]
    fn is_pid_alive_returns_false_for_almost_certainly_dead_pid() {
        // Pick a PID that's extremely unlikely to be in use. The Linux
        // default `pid_max` is 32768; macOS caps at 99998. A PID up at
        // u32::MAX - 1 is not assignable on either platform.
        // Finding #2 fix: switched sentinel from u32::MAX - 1 (which on
        // Unix would have been silently reinterpreted as a negative
        // process-group probe and only "passed" because that group
        // happened to be empty on the test host) to a value safely
        // inside i32::MAX so the function actually exercises the
        // ESRCH path on Unix and the empty-tasklist path on Windows.
        assert!(!is_pid_alive(i32::MAX as u32 - 1));
    }
}
