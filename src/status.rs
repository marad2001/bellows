use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::runner::BlockReason;

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
    #[serde(default)]
    pub current: Option<CurrentRun>,
    /// Pre-claim block (issues #42 and #76): the orchestrator refused
    /// to claim a new issue this tick. The carried `BlockReason`
    /// distinguishes the slice-b case (open `agent/*` PRs gating
    /// master) from the slice-#76 case (stale `agent/<N>-*` ref
    /// deletion failed). `current` and `blocked` are mutually
    /// exclusive — `write_busy` and `write_blocked` both clear the
    /// other field.
    #[serde(default)]
    pub blocked: Option<BlockedState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentRun {
    pub issue_number: u64,
    pub issue_title: String,
    pub repo: String,
    pub claimed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedState {
    pub reason: BlockReason,
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
                blocked: None,
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
                blocked: None,
            },
        )
        .await
    }

    /// Persist the Blocked state — bellows refused to claim a new issue
    /// because either an open `agent/*` PR may still gate master (slice
    /// b / #42) or a stale `agent/<N>-*` ref deletion failed before
    /// claim (#76 / ADR-0003). The carried `BlockReason` distinguishes
    /// the two cases for `bellows status`. The empty-`pr_numbers`
    /// fail-closed case is encoded inside the `OpenAgentPrs` variant;
    /// the summariser renders that distinctly from a known list.
    pub async fn write_blocked(&self, reason: &BlockReason) -> Result<(), StatusError> {
        write(
            &self.path,
            &Status {
                pid: self.pid,
                started_at: self.started_at,
                current: None,
                blocked: Some(BlockedState {
                    reason: reason.clone(),
                }),
            },
        )
        .await
    }
}

/// The single idle line the polling loop emits on transition into idle.
/// Exposed so callers (and tests) can match against the canonical
/// wording without duplicating the literal.
pub const IDLE_LINE: &str = "bellows: idle (no ready-for-agent issues)";

const RESUME_LINE: &str =
    "bellows: no longer blocked by a running agent container; resuming normal claim behaviour";

/// The "ongoing" outcome states whose log lines are deduplicated. A
/// continuation tick in any of these states stays silent; only a
/// transition between distinct shapes emits a fresh line.
///
/// Per-event outcomes (Finalised, Contended, Cancelled) are NOT
/// modelled here: they reset the tracker via `observe_event` so the
/// next ongoing-state tick is treated as a fresh transition.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OngoingState {
    Idle,
    /// Carrying the whole `BlockReason` so a transition between block
    /// kinds (PR set merges but a stale-branch sweep starts failing,
    /// or vice versa) emits a fresh line — the dedup contract is
    /// "same kind + same payload, silent; anything else, fresh line".
    Blocked(BlockReason),
    /// A `RunError` whose shape (variant + payload, see
    /// `RunError::shape`) matches this key. Two consecutive `Err`s
    /// with the same shape key are treated as a continuation.
    Err(String),
}

/// Transition-only logger for the polling loop's "ongoing" outcomes.
///
/// Without dedup, a 30s polling interval floods the operator's terminal
/// and `bellows.log` with one identical line per tick — `Idle` between
/// events (the common case), or repeated `MissingAgentBrief(N)` while
/// the operator drafts a brief. `OutcomeTransition` collapses runs of
/// identical "ongoing" outcomes into a single line emitted on the
/// transition into that state, restoring signal in the log.
///
/// Per-event outcomes (Finalised, Contended, Cancelled) bypass dedup
/// via `observe_event`: each one represents a discrete happening, not
/// an ongoing state, so the caller always logs them. `observe_event`
/// still returns the resume-from-blocked line when leaving the
/// `Blocked` state so the operator sees the transition.
///
/// Generalises the original `BlockTransition` introduced for issue
/// #42 — same transition-only contract for `Blocked`, now extended to
/// `Idle` and identical-`Err` runs per issue #50.
#[derive(Debug, Default)]
pub struct OutcomeTransition {
    last: Option<OngoingState>,
}

impl OutcomeTransition {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe a tick that returned `RunOutcome::Blocked` with this
    /// `reason`. Returns `Some(line)` if a log line should be emitted
    /// (the reason differs from the previous tick) or `None` if the
    /// tick is a same-reason continuation that should stay silent.
    /// Slice-b's same-PR-set silence contract is preserved; slice-#76
    /// adds the same contract for the stale-branch-deletion case.
    pub fn observe_blocked(&mut self, reason: &BlockReason) -> Option<String> {
        let new = OngoingState::Blocked(reason.clone());
        if self.last.as_ref() == Some(&new) {
            return None;
        }
        self.last = Some(new);
        Some(format_blocked_line(reason))
    }

    /// Observe a tick that returned `RunOutcome::Idle`. Returns the
    /// lines to log on this tick:
    /// - empty when the prior tick was also idle (the dedup case);
    /// - one element on a fresh transition into idle from an error or
    ///   from startup;
    /// - two elements when leaving the blocked state into idle: the
    ///   resume-from-blocked line first, then the idle line, so the
    ///   operator sees both that the block cleared and that the loop
    ///   has returned to its steady-state idle pattern.
    pub fn observe_idle(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        if matches!(self.last, Some(OngoingState::Idle)) {
            return lines;
        }
        if matches!(self.last, Some(OngoingState::Blocked(_))) {
            lines.push(RESUME_LINE.to_string());
        }
        self.last = Some(OngoingState::Idle);
        lines.push(IDLE_LINE.to_string());
        lines
    }

    /// Observe a tick that returned `Err(_)` from `run_once`. The
    /// caller passes a `shape_key` (typically `RunError::shape()`) plus
    /// the formatted log line. Returns the lines to log:
    /// - empty when the prior tick had the same shape key (the dedup
    ///   case — a `MissingAgentBrief(42)` recurring until the operator
    ///   fixes the brief);
    /// - one element on a fresh error or a transition between
    ///   different error shapes;
    /// - two elements when leaving the blocked state into an error:
    ///   the resume-from-blocked line first, then the error line, so
    ///   the operator sees the block cleared and the new failure
    ///   context.
    pub fn observe_error(&mut self, shape_key: &str, line: String) -> Vec<String> {
        let mut lines = Vec::new();
        let new = OngoingState::Err(shape_key.to_string());
        if self.last.as_ref() == Some(&new) {
            return lines;
        }
        if matches!(self.last, Some(OngoingState::Blocked(_))) {
            lines.push(RESUME_LINE.to_string());
        }
        self.last = Some(new);
        lines.push(line);
        lines
    }

    /// Observe a tick that returned a per-event outcome (`Finalised`,
    /// `Contended`, or `Cancelled`). These outcomes are inherently
    /// one-shot and the caller always logs their own line; the tracker
    /// is responsible only for:
    /// - returning the resume-from-blocked line when transitioning out
    ///   of the blocked state, so the operator sees that transition
    ///   even on a tick whose primary log line is the event itself;
    /// - resetting the ongoing-state tracker so that the next
    ///   idle/error/blocked tick is treated as a fresh transition
    ///   (per brief AC #2: "re-enters idle after some other outcome"
    ///   must emit a fresh idle line).
    pub fn observe_event(&mut self) -> Option<String> {
        let was_blocked = matches!(self.last, Some(OngoingState::Blocked(_)));
        self.last = None;
        if was_blocked {
            Some(RESUME_LINE.to_string())
        } else {
            None
        }
    }
}

fn format_blocked_line(reason: &BlockReason) -> String {
    match reason {
        BlockReason::AgentContainerRunning { container_id, .. } if container_id.is_empty() => {
            "bellows: blocked (could not probe local Docker daemon for running agent containers; failing closed and retrying next tick)"
                .to_string()
        }
        BlockReason::AgentContainerRunning { container_id, .. } => {
            format!(
                "bellows: blocked by running agent container {container_id} (waiting for it to exit)",
            )
        }
        BlockReason::StaleAgentBranchDeletionFailed { branch, error } => {
            format!(
                "bellows: blocked — pre-claim deletion of stale agent branch `{branch}` failed: {error} (retrying next tick)",
            )
        }
    }
}

/// Outcome of `check_status_for_kill` — whether `bellows kill <N>`
/// should proceed to find + remove the container, or refuse with a
/// clear message because the orchestrator isn't busy on that issue.
#[derive(Debug, PartialEq, Eq)]
pub enum KillPrecheck {
    /// Status file shows bellows is busy on the requested issue —
    /// safe to look up the container and force-remove it.
    Proceed,
    /// Status file shows bellows is idle, working on a different
    /// issue, or not running at all. Holds the message the CLI
    /// should print to stderr before exiting non-zero.
    Refuse(String),
}

/// Decide whether `bellows kill <N>` should proceed based on what the
/// status file (and PID-liveness) report. Pure function — no IO — so
/// every branch (no file, idle, busy on different issue, busy on the
/// requested issue, stale PID) is testable without orchestrating a
/// real bellows process.
///
/// `pid_alive` is the result of `is_pid_alive(status.pid)` and is
/// passed in rather than recomputed so the tests can pin both branches
/// of the staleness check.
pub fn check_status_for_kill(
    status: Option<&Status>,
    pid_alive: bool,
    target_issue: u64,
) -> KillPrecheck {
    let busy_on = match status {
        None => None,
        Some(_) if !pid_alive => None,
        Some(s) => s.current.as_ref().map(|c| c.issue_number),
    };
    match busy_on {
        Some(n) if n == target_issue => KillPrecheck::Proceed,
        Some(n) => KillPrecheck::Refuse(format!(
            "bellows is not currently working on issue #{target_issue} (currently busy on issue #{n})",
        )),
        None => KillPrecheck::Refuse(format!(
            "bellows is not currently working on issue #{target_issue} (currently idle)",
        )),
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
        Some(s) => {
            // Blocked takes precedence over current. The two are
            // mutually exclusive on the writer side, but a defensive
            // priority here matches the brief: a Blocked status is
            // the operator's headline when bellows is refusing to
            // claim because a running agent container holds the
            // concurrency=1 slot.
            if let Some(b) = &s.blocked {
                return format_blocked_summary(s, b);
            }
            match &s.current {
                None => format!(
                    "bellows is running (PID {}, started at {}), currently idle.",
                    s.pid,
                    s.started_at.to_rfc3339(),
                ),
                Some(c) => format!(
                    "bellows is running (PID {}, started at {}), currently busy on {}#{} (\"{}\"), claimed at {}.",
                    s.pid,
                    s.started_at.to_rfc3339(),
                    c.repo,
                    c.issue_number,
                    c.issue_title,
                    c.claimed_at.to_rfc3339(),
                ),
            }
        }
    }
}

fn format_blocked_summary(s: &Status, b: &BlockedState) -> String {
    match &b.reason {
        BlockReason::AgentContainerRunning { container_id, .. } if container_id.is_empty() => {
            format!(
                "bellows is running (PID {}, started at {}), blocked (could not probe local Docker daemon for running agent containers; failing closed and retrying next tick).",
                s.pid,
                s.started_at.to_rfc3339(),
            )
        }
        BlockReason::AgentContainerRunning {
            container_id,
            started_at,
        } => format!(
            "bellows is running (PID {}, started at {}), blocked by running agent container {} (started at {}, waiting for it to exit).",
            s.pid,
            s.started_at.to_rfc3339(),
            container_id,
            started_at.to_rfc3339(),
        ),
        BlockReason::StaleAgentBranchDeletionFailed { branch, error } => format!(
            "bellows is running (PID {}, started at {}), blocked — pre-claim deletion of stale agent branch `{}` failed: {} (retrying next tick).",
            s.pid,
            s.started_at.to_rfc3339(),
            branch,
            error,
        ),
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

    /// Tiny ergonomic helper so the dedup tests can still read a single
    /// `container("abc")` rather than the multi-line
    /// `BlockReason::AgentContainerRunning { container_id, started_at }`
    /// literal at every call site. `started_at` is fixed; the dedup
    /// contract is keyed on the BlockReason value so two distinct calls
    /// with the same id collapse to one log line.
    fn container(id: &str) -> BlockReason {
        BlockReason::AgentContainerRunning {
            container_id: id.to_string(),
            started_at: DateTime::parse_from_rfc3339("2026-05-10T15:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        }
    }

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
            blocked: None,
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
            blocked: None,
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
        // Issue #35 multi-repo polling: the busy line surfaces the
        // `<owner>/<name>#<issue>` form so an operator running
        // `bellows status` can tell at a glance which repo bellows is
        // currently working on. Plain `issue #9` would be ambiguous in
        // a multi-repo config.
        assert!(s.contains("Smoke test: ..."), "{}", s);
        assert!(s.contains("marad2001/bellows-test"), "{}", s);
        assert!(s.contains("2026-05-10T15:02:00"), "{}", s);
        assert!(!s.contains("currently idle"), "{}", s);
    }

    #[test]
    fn summarise_busy_line_uses_owner_repo_hash_issue_format() {
        // Issue #35 acceptance criterion: bellows status busy-line
        // includes `<owner>/<name>#<issue>` so the report is
        // unambiguous in multi-repo configs. The brief specifically
        // calls out this format; pin the substring so a reformat
        // can't silently regress to the slice-9 wording.
        let st = sample_status();
        let s = summarise(Some(&st), true);
        assert!(
            s.contains("marad2001/bellows-test#9"),
            "busy line must include <owner>/<name>#<issue>: {s}",
        );
    }

    #[test]
    fn summarise_blocked_status_names_the_running_agent_container_id() {
        // Issue #126: pre-claim check inspects the local Docker daemon
        // for a running `bellows-*` agent container; when one is
        // detected, the tick is `Blocked(AgentContainerRunning)` and
        // `bellows status` must name the container id (the brief: "the
        // status-file renderer and `bellows status` CLI summariser are
        // updated to name what is actually blocking").
        let st = Status {
            pid: 12345,
            started_at: DateTime::parse_from_rfc3339("2026-05-10T15:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            current: None,
            blocked: Some(BlockedState {
                reason: BlockReason::AgentContainerRunning {
                    container_id: "abc123def456".to_string(),
                    started_at: DateTime::parse_from_rfc3339("2026-05-10T15:20:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                },
            }),
        };
        let s = summarise(Some(&st), true);
        assert!(s.contains("bellows is running"), "{}", s);
        assert!(s.contains("PID 12345"), "{}", s);
        assert!(
            s.contains("blocked"),
            "summary must surface the blocked state: {s}",
        );
        assert!(
            s.contains("abc123def456"),
            "summary must name the blocking container id: {s}",
        );
        assert!(
            s.to_lowercase().contains("agent container"),
            "summary must say it's blocked by an agent container: {s}",
        );
        assert!(
            !s.contains("currently idle"),
            "blocked is not idle — must not render the idle wording: {s}",
        );
        assert!(
            !s.to_lowercase().contains("waiting for merge"),
            "container-running case must NOT use the dropped PR-gated wording: {s}",
        );
    }

    #[tokio::test]
    async fn write_blocked_persists_agent_container_in_documented_schema() {
        // Pin the on-disk shape for the new variant so a future serde
        // rename can't silently break the status command or
        // `bellows status`'s consumers. The schema nests under
        // `reason.kind` (the BlockReason discriminant) so both the
        // stale-branch case from #76 and the container-running case
        // from #126 can be surfaced through the same field.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("status.json");
        let ctx = StatusContext {
            path: path.clone(),
            pid: 4242,
            started_at: DateTime::parse_from_rfc3339("2026-05-10T15:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };

        let container_started = DateTime::parse_from_rfc3339("2026-05-10T15:20:00Z")
            .unwrap()
            .with_timezone(&Utc);
        ctx.write_blocked(&BlockReason::AgentContainerRunning {
            container_id: "abc123def456".to_string(),
            started_at: container_started,
        })
        .await
        .unwrap();
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["pid"], 4242);
        assert_eq!(v["started_at"], "2026-05-10T15:00:00Z");
        assert!(v["current"].is_null());
        assert_eq!(v["blocked"]["reason"]["kind"], "agent_container_running");
        assert_eq!(v["blocked"]["reason"]["container_id"], "abc123def456");
        assert_eq!(
            v["blocked"]["reason"]["started_at"],
            "2026-05-10T15:20:00Z",
        );
    }

    #[tokio::test]
    async fn write_blocked_persists_stale_branch_failure_in_documented_schema() {
        // Issue #76 / ADR-0003: the new failure mode's on-disk shape.
        // Pin it the same way the slice-b case is pinned so a renamer
        // or accidental untagged-enum change is caught by CI.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("status.json");
        let ctx = StatusContext {
            path: path.clone(),
            pid: 4242,
            started_at: DateTime::parse_from_rfc3339("2026-05-10T15:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };

        ctx.write_blocked(&BlockReason::StaleAgentBranchDeletionFailed {
            branch: "agent/16-foo".to_string(),
            error: "403: Resource not accessible by integration".to_string(),
        })
        .await
        .unwrap();
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v["blocked"]["reason"]["kind"],
            "stale_agent_branch_deletion_failed",
        );
        assert_eq!(v["blocked"]["reason"]["branch"], "agent/16-foo");
        assert!(
            v["blocked"]["reason"]["error"]
                .as_str()
                .unwrap()
                .contains("403"),
            "error string must round-trip through the JSON: {}",
            v["blocked"]["reason"]["error"],
        );
    }

    #[test]
    fn summarise_blocked_status_for_stale_branch_failure_names_branch_and_remedy() {
        // `bellows status` must distinguish the slice-#76 failure from
        // the slice-b PR-gated case so the operator knows which lever
        // to pull (branch protection or PAT scope, vs. wait for CI).
        let st = Status {
            pid: 12345,
            started_at: DateTime::parse_from_rfc3339("2026-05-10T15:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            current: None,
            blocked: Some(BlockedState {
                reason: BlockReason::StaleAgentBranchDeletionFailed {
                    branch: "agent/16-foo".to_string(),
                    error: "403: Resource not accessible by integration".to_string(),
                },
            }),
        };
        let s = summarise(Some(&st), true);
        assert!(s.contains("blocked"), "{}", s);
        assert!(s.contains("agent/16-foo"), "must name the failing branch: {s}");
        assert!(s.contains("403"), "must include the underlying error: {s}");
        assert!(
            !s.contains("waiting for merge"),
            "stale-branch case must NOT use the PR-gated wording: {s}",
        );
    }

    #[tokio::test]
    async fn write_idle_after_write_blocked_clears_the_blocked_state() {
        // Brief: "Closing or merging the blocking PR causes the next
        // polling tick to transition out of blocked and resume normal
        // claim behaviour without operator intervention." The status
        // file's Blocked state must clear when bellows transitions back
        // to idle — otherwise `bellows status` would lie until the next
        // claim landed.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("status.json");
        let ctx = StatusContext {
            path: path.clone(),
            pid: 4242,
            started_at: DateTime::parse_from_rfc3339("2026-05-10T15:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };

        ctx.write_blocked(&BlockReason::AgentContainerRunning {
            container_id: "abc123def456".to_string(),
            started_at: DateTime::parse_from_rfc3339("2026-05-10T15:20:00Z")
                .unwrap()
                .with_timezone(&Utc),
        })
        .await
        .unwrap();
        ctx.write_idle().await.unwrap();
        let parsed = read(&path).await.unwrap().unwrap();
        assert!(parsed.blocked.is_none(), "expected blocked cleared, got {:?}", parsed.blocked);
        assert!(parsed.current.is_none());
    }

    #[tokio::test]
    async fn write_busy_clears_any_prior_blocked_state() {
        // Defensive: a claim should never coexist with a Blocked state
        // in the status file. write_busy must clear the blocked field
        // so consumers don't have to handle both-set-at-once.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("status.json");
        let ctx = StatusContext {
            path: path.clone(),
            pid: 4242,
            started_at: DateTime::parse_from_rfc3339("2026-05-10T15:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        ctx.write_blocked(&BlockReason::AgentContainerRunning {
            container_id: "abc123def456".to_string(),
            started_at: DateTime::parse_from_rfc3339("2026-05-10T15:20:00Z")
                .unwrap()
                .with_timezone(&Utc),
        })
        .await
        .unwrap();
        ctx.write_busy(CurrentRun {
            issue_number: 9,
            issue_title: "Test".to_string(),
            repo: "o/r".to_string(),
            claimed_at: DateTime::parse_from_rfc3339("2026-05-10T15:30:00Z")
                .unwrap()
                .with_timezone(&Utc),
        })
        .await
        .unwrap();
        let parsed = read(&path).await.unwrap().unwrap();
        assert!(parsed.blocked.is_none());
        assert!(parsed.current.is_some());
    }

    #[test]
    fn outcome_transition_idle_to_blocked_emits_log_line() {
        // Brief: "The polling loop's log emits a 'blocked' line only on
        // state transitions — not on every tick." On the first tick
        // that sees a running agent container, the line fires and names
        // the container id so an operator running `tail -f bellows.log`
        // knows what is gating the next claim.
        let mut tracker = OutcomeTransition::new();
        let line = tracker.observe_blocked(&container("abc123"));
        let line = line.expect("expected a transition log line");
        assert!(line.contains("blocked"), "{}", line);
        assert!(line.contains("abc123"), "{}", line);
    }

    #[test]
    fn outcome_transition_same_block_set_twice_in_a_row_is_silent() {
        // The whole point of transition-only logging: while a single
        // long-running agent container is in flight, the polling loop
        // would otherwise flood the log with ~N identical "blocked"
        // lines every poll interval, drowning out everything else.
        let mut tracker = OutcomeTransition::new();
        let _ = tracker.observe_blocked(&container("abc123"));
        let line = tracker.observe_blocked(&container("abc123"));
        assert!(
            line.is_none(),
            "second identical observation must not emit a log line, got {:?}",
            line,
        );
    }

    #[test]
    fn outcome_transition_block_set_changing_emits_a_fresh_log_line() {
        // If one agent container finishes and a new one starts before
        // the next idle tick (unusual but possible — fast finalise +
        // immediate next claim by another process holding the lock),
        // the block set changes and the operator wants a fresh line.
        let mut tracker = OutcomeTransition::new();
        let _ = tracker.observe_blocked(&container("abc123"));
        let line = tracker.observe_blocked(&container("def456"));
        let line = line.expect("set changed; expected a transition line");
        assert!(line.contains("def456"), "{}", line);
    }

    #[test]
    fn outcome_transition_blocked_to_idle_emits_resume_then_idle_line() {
        // Transitioning out of blocked into idle: the operator needs to
        // see both that the block has cleared (resume line) AND that the
        // loop is back to the steady-state idle pattern.
        let mut tracker = OutcomeTransition::new();
        let _ = tracker.observe_blocked(&container("abc123"));
        let lines = tracker.observe_idle();
        assert_eq!(
            lines.len(),
            2,
            "blocked->idle must emit resume + idle, got {:?}",
            lines,
        );
        assert!(
            lines[0].to_lowercase().contains("no longer blocked")
                || lines[0].to_lowercase().contains("resuming"),
            "first line must be the resume line: {:?}",
            lines,
        );
        assert!(
            lines[1].contains("idle"),
            "second line must be the idle line: {:?}",
            lines,
        );
    }

    #[test]
    fn outcome_transition_blocked_to_event_emits_only_resume_line() {
        // For per-event outcomes (Finalised/Contended/Cancelled) the
        // event itself is logged by the caller; the tracker is only
        // responsible for the resume-from-blocked line.
        let mut tracker = OutcomeTransition::new();
        let _ = tracker.observe_blocked(&container("abc123"));
        let line = tracker.observe_event();
        let line = line.expect("blocked->event must emit a resume line");
        assert!(
            line.to_lowercase().contains("no longer blocked")
                || line.to_lowercase().contains("resuming"),
            "expected resume wording: {line}",
        );
    }

    #[test]
    fn outcome_transition_first_idle_emits_the_idle_line() {
        // On startup, the very first idle tick must emit the idle line
        // so the operator knows the polling loop is running and seeing
        // an empty queue.
        let mut tracker = OutcomeTransition::new();
        let lines = tracker.observe_idle();
        assert_eq!(lines.len(), 1, "first idle must emit one line, got {:?}", lines);
        assert!(lines[0].contains("idle"), "{:?}", lines);
        assert!(
            lines[0].contains("no ready-for-agent"),
            "idle line must explain why it's idle: {:?}",
            lines,
        );
    }

    #[test]
    fn outcome_transition_idle_to_idle_is_silent() {
        // Brief AC #1: a polling loop that stays idle for N consecutive
        // ticks emits the idle log line at most ONCE — on transition
        // into idle. The whole point of this slice.
        let mut tracker = OutcomeTransition::new();
        let _ = tracker.observe_idle();
        let lines = tracker.observe_idle();
        assert!(
            lines.is_empty(),
            "idle->idle must be silent, got {:?}",
            lines,
        );
        let lines = tracker.observe_idle();
        assert!(
            lines.is_empty(),
            "idle->idle->idle must stay silent, got {:?}",
            lines,
        );
    }

    #[test]
    fn outcome_transition_event_then_idle_emits_fresh_idle_line() {
        // Brief AC #2: re-entering idle after some other outcome
        // (Finalised, Contended, Cancelled) emits a fresh idle line.
        // Events reset the tracker so the next idle counts as a
        // transition.
        let mut tracker = OutcomeTransition::new();
        let _ = tracker.observe_idle(); // first idle line emitted
        let _ = tracker.observe_event(); // simulate a finalised event
        let lines = tracker.observe_idle();
        assert_eq!(
            lines.len(),
            1,
            "post-event idle must emit a fresh idle line, got {:?}",
            lines,
        );
        assert!(lines[0].contains("idle"), "{:?}", lines);
    }

    #[test]
    fn outcome_transition_error_to_idle_emits_idle_line() {
        // After a persistent error clears (operator fixed the brief),
        // the next idle tick must announce the recovery.
        let mut tracker = OutcomeTransition::new();
        let _ = tracker.observe_error("missing_agent_brief:42", "err".to_string());
        let lines = tracker.observe_idle();
        assert_eq!(
            lines.len(),
            1,
            "error->idle must emit the idle line, got {:?}",
            lines,
        );
        assert!(lines[0].contains("idle"), "{:?}", lines);
    }

    #[test]
    fn outcome_transition_first_error_emits_the_error_line() {
        let mut tracker = OutcomeTransition::new();
        let lines = tracker.observe_error(
            "missing_agent_brief:42",
            "bellows: error: brief missing".to_string(),
        );
        assert_eq!(lines.len(), 1, "{:?}", lines);
        assert_eq!(lines[0], "bellows: error: brief missing");
    }

    #[test]
    fn outcome_transition_same_error_twice_is_silent() {
        // Brief AC #3: repeated `Err(MissingAgentBrief(N))` for the same
        // issue number emits the error line at most once. The 2026-05-11
        // exemplar in the brief was 5 identical lines back-to-back; the
        // dedup makes that one.
        let mut tracker = OutcomeTransition::new();
        let _ = tracker.observe_error("missing_agent_brief:42", "err line".to_string());
        let lines = tracker.observe_error("missing_agent_brief:42", "err line".to_string());
        assert!(
            lines.is_empty(),
            "same error shape must be silent, got {:?}",
            lines,
        );
        // And a third tick still silent.
        let lines = tracker.observe_error("missing_agent_brief:42", "err line".to_string());
        assert!(lines.is_empty(), "{:?}", lines);
    }

    #[test]
    fn outcome_transition_error_with_different_payload_emits_fresh_line() {
        // Brief AC #4: same variant + different issue number = different
        // shape = fresh line. MissingAgentBrief(42) → MissingAgentBrief(43).
        let mut tracker = OutcomeTransition::new();
        let _ = tracker.observe_error("missing_agent_brief:42", "err 42".to_string());
        let lines = tracker.observe_error("missing_agent_brief:43", "err 43".to_string());
        assert_eq!(lines.len(), 1, "{:?}", lines);
        assert_eq!(lines[0], "err 43");
    }

    #[test]
    fn outcome_transition_error_to_different_variant_emits_fresh_line() {
        // Brief AC #4: different variant = different shape, fresh line.
        let mut tracker = OutcomeTransition::new();
        let _ = tracker.observe_error("missing_agent_brief:42", "brief err".to_string());
        let lines = tracker.observe_error("octocrab:rate limited", "rate err".to_string());
        assert_eq!(lines.len(), 1, "{:?}", lines);
        assert_eq!(lines[0], "rate err");
    }

    #[test]
    fn outcome_transition_idle_to_error_emits_the_error_line() {
        let mut tracker = OutcomeTransition::new();
        let _ = tracker.observe_idle();
        let lines = tracker.observe_error("io:disk full", "io err".to_string());
        assert_eq!(lines.len(), 1, "{:?}", lines);
        assert_eq!(lines[0], "io err");
    }

    #[test]
    fn outcome_transition_blocked_to_error_emits_resume_then_error() {
        // If the polling loop transitions from blocked straight into an
        // error state, the operator still wants the resume line so they
        // know the block cleared — followed by the new error context.
        let mut tracker = OutcomeTransition::new();
        let _ = tracker.observe_blocked(&container("abc123"));
        let lines = tracker.observe_error("missing_agent_brief:42", "brief err".to_string());
        assert_eq!(
            lines.len(),
            2,
            "blocked->error must emit resume + error, got {:?}",
            lines,
        );
        assert!(
            lines[0].to_lowercase().contains("no longer blocked")
                || lines[0].to_lowercase().contains("resuming"),
            "first line must be the resume line: {:?}",
            lines,
        );
        assert_eq!(lines[1], "brief err");
    }

    #[test]
    fn outcome_transition_event_then_error_emits_fresh_error_line() {
        // Brief AC #2 sibling: events reset the tracker. If an error
        // recurs after a successful run, the operator sees it again.
        let mut tracker = OutcomeTransition::new();
        let _ = tracker.observe_error("missing_agent_brief:42", "err".to_string());
        let _ = tracker.observe_event();
        let lines = tracker.observe_error("missing_agent_brief:42", "err".to_string());
        assert_eq!(
            lines.len(),
            1,
            "post-event same error must re-emit, got {:?}",
            lines,
        );
    }

    #[test]
    fn outcome_transition_event_after_idle_returns_no_resume_line() {
        // The resume line is specifically about leaving the blocked
        // state. Going from idle into a per-event outcome must not
        // emit a misleading "no longer blocked" line.
        let mut tracker = OutcomeTransition::new();
        let _ = tracker.observe_idle();
        let line = tracker.observe_event();
        assert!(line.is_none(), "idle->event must not emit resume, got {:?}", line);
    }

    #[test]
    fn outcome_transition_event_after_error_returns_no_resume_line() {
        let mut tracker = OutcomeTransition::new();
        let _ = tracker.observe_error("missing_agent_brief:42", "err".to_string());
        let line = tracker.observe_event();
        assert!(line.is_none(), "error->event must not emit resume, got {:?}", line);
    }

    #[test]
    fn outcome_transition_back_to_back_events_both_return_no_resume_line() {
        // Two finalised runs in a row: no ongoing-state line in
        // between, the events themselves are logged by the caller.
        let mut tracker = OutcomeTransition::new();
        assert!(tracker.observe_event().is_none());
        assert!(tracker.observe_event().is_none());
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
    fn check_status_for_kill_proceeds_when_busy_on_requested_issue() {
        let st = sample_status(); // sample is busy on issue 9
        match check_status_for_kill(Some(&st), true, 9) {
            KillPrecheck::Proceed => {}
            other => panic!("expected Proceed, got {other:?}"),
        }
    }

    #[test]
    fn check_status_for_kill_refuses_when_busy_on_different_issue() {
        let st = sample_status(); // busy on issue 9
        match check_status_for_kill(Some(&st), true, 17) {
            KillPrecheck::Refuse(msg) => {
                assert!(
                    msg.contains("issue #17") && msg.contains("issue #9"),
                    "msg should name both target and current issue: {msg}",
                );
                assert!(msg.contains("not currently working"), "msg: {msg}");
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
    }

    #[test]
    fn check_status_for_kill_refuses_when_idle() {
        let mut st = sample_status();
        st.current = None;
        match check_status_for_kill(Some(&st), true, 9) {
            KillPrecheck::Refuse(msg) => {
                assert!(msg.contains("issue #9"), "msg should name target: {msg}");
                assert!(msg.contains("currently idle"), "msg: {msg}");
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
    }

    #[test]
    fn check_status_for_kill_refuses_when_no_status_file() {
        match check_status_for_kill(None, false, 9) {
            KillPrecheck::Refuse(msg) => {
                assert!(msg.contains("issue #9"), "msg should name target: {msg}");
                assert!(msg.contains("currently idle"), "msg: {msg}");
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
    }

    #[test]
    fn check_status_for_kill_treats_stale_pid_as_idle() {
        // Status file says busy on 9, but the PID isn't alive — the
        // orchestrator process died and left the file behind. There's
        // no process to signal, so refuse with the idle message.
        let st = sample_status();
        match check_status_for_kill(Some(&st), false, 9) {
            KillPrecheck::Refuse(msg) => {
                assert!(msg.contains("currently idle"), "msg: {msg}");
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
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
