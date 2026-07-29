use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::process::Command;

use crate::config::GatesConfig;
use crate::large_files::{scan_large_files, LargeFile};
use crate::workflow_parse::{parse_ci_workflow, ExtractedCommands, Provenance};

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("git clone failed (status {0})")]
    CloneFailed(std::process::ExitStatus),
    #[error("git {args:?} failed (status {status}){}", format_stderr_suffix(stderr))]
    GitFailed {
        args: Vec<String>,
        status: std::process::ExitStatus,
        /// Captured stderr from the failing git invocation, if the
        /// callsite used `.output()` rather than `.status()`. Issue
        /// #113: the workspace push helper captures this so the
        /// runner's halt-log path can surface git's lease-rejection
        /// message verbatim to operators. Empty string when not
        /// captured (most internal callsites use `.status()` and
        /// inherit the child's stderr to the parent's tty).
        stderr: String,
    },
    #[error("agent produced no changes to commit; the brief was probably unmet")]
    NoChangesToCommit,
    #[error("workspace sample exceeded the {limit}-byte read limit for git {args:?}")]
    SampleTooLarge { args: Vec<String>, limit: u64 },
    #[error("workspace sample could not be isolated from the agent's repository: {0}")]
    SampleIsolation(String),
}

/// Render the trailing `: <stderr>` for [`WorkspaceError::GitFailed`]'s
/// `Display`. Empty stderr (the default for `.status()`-based
/// callsites) produces an empty suffix so the legacy single-line
/// rendering is preserved; a non-empty stderr is appended after a
/// colon so the operator-visible halt-log line attributes the failure
/// to its git-side cause.
fn format_stderr_suffix(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(": {trimmed}")
    }
}

pub struct Workspace {
    temp_dir: TempDir,
    branch_name: String,
    default_branch: String,
    gate_commands: GateCommands,
    /// Issue #161 large-file pre-scan, captured at `prepare` time for
    /// the same reason as `gate_commands`: snapshotted once at clone
    /// time so the list handed to the implement kickoff cannot drift
    /// mid-run even if the agent edits or adds files.
    large_files: Vec<LargeFile>,
}

/// ADR-0004 cargo-checks gate command snapshot, captured at
/// `prepare` time and read by both the post-implement and
/// end-pipeline gate phases within the same run. Each command carries
/// its own [`Provenance`] so the operator-visible run-log line can
/// state unambiguously whether the command was mirrored from the
/// target repo's `.github/workflows/ci.yml` or substituted from the
/// operator-declared `[gates].*_flags` fallback in `orchestrator.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateCommands {
    /// Complete cargo clippy invocation, including the `cargo`
    /// prefix. Bellows hands this to the sandbox container verbatim.
    pub clippy: String,
    pub clippy_source: Provenance,
    /// Build-relevant env the target's CI ran clippy under, mirrored
    /// into the gate (issue #180). Empty when the command came from the
    /// `[gates]` fallback — an operator-declared flag set carries no CI
    /// environment to mirror.
    pub clippy_env: Vec<(String, String)>,
    /// The CI job whose steps this command was mirrored from, which is
    /// the name GitHub gives the corresponding check-run. `None` when the
    /// command came from the `[gates]` fallback: bellows invented that
    /// posture, so no check-run on the repo corresponds to it.
    pub clippy_check: Option<String>,
    pub test: String,
    pub test_source: Provenance,
    /// Build-relevant env the target's CI ran tests under. See
    /// `clippy_env`; kept per-command because a repo can split clippy
    /// and test into sibling jobs with differing env blocks.
    pub test_env: Vec<(String, String)>,
    /// See `clippy_check`.
    pub test_check: Option<String>,
}

impl GateCommands {
    /// Format the two operator-visible run-log lines that announce
    /// which cargo command is about to run and where it came from.
    /// Emitted by the runner at the start of each cargo-checks gate
    /// phase, so an operator tailing the log can tell whether bellows
    /// is mirroring the target repo's CI or has fallen back to the
    /// operator-declared `[gates].*_flags`.
    ///
    /// Each line is shaped:
    ///   `  <check>: <command>  [<provenance>]`
    /// where `<provenance>` is either `parsed from <path>` or
    /// `fallback from [gates].<knob>` so the source is unambiguous.
    /// Any mirrored env is announced on its own trailing line so the
    /// first two lines keep their historical shape (issue #180).
    pub fn announcement_lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!(
                "  clippy: {}  [{}]",
                self.clippy,
                format_provenance(&self.clippy_source, "[gates].clippy_flags"),
            ),
            format!(
                "  test:   {}  [{}]",
                self.test,
                format_provenance(&self.test_source, "[gates].test_flags"),
            ),
        ];
        for (label, env) in [
            ("clippy", &self.clippy_env),
            ("test", &self.test_env),
        ] {
            if env.is_empty() {
                continue;
            }
            let rendered = env
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ");
            lines.push(format!("  {label} env: {rendered}  [mirrored from CI]"));
        }
        lines
    }
}

fn format_provenance(provenance: &Provenance, config_knob: &str) -> String {
    match provenance {
        Provenance::ParsedFromWorkflow(path) => format!("parsed from {}", path.display()),
        Provenance::FallbackFromConfig => format!("fallback from {}", config_knob),
    }
}

impl Workspace {
    pub fn path(&self) -> &Path {
        self.temp_dir.path()
    }

    pub fn branch_name(&self) -> &str {
        &self.branch_name
    }

    /// The remote's default branch as it was at clone time
    /// (e.g. "master" or "main"). Used as the base for opening PRs.
    pub fn default_branch(&self) -> &str {
        &self.default_branch
    }

    /// The snapshotted cargo-checks gate commands (ADR-0004). Captured
    /// at `prepare` time so subsequent gate phases within the same run
    /// see a stable verdict even if the agent edits
    /// `.github/workflows/ci.yml` mid-pipeline.
    pub fn gate_commands(&self) -> &GateCommands {
        &self.gate_commands
    }

    /// The snapshotted large-file pre-scan (issue #161), sorted
    /// descending by size with a lexicographic path tiebreak. Captured
    /// at `prepare` time so the implement kickoff names exactly the
    /// files that were over the `Read` cap in the clone, and the list
    /// cannot shift mid-run even if the agent edits files.
    pub fn large_files(&self) -> &[LargeFile] {
        &self.large_files
    }
}

/// The single operator-visible run-log line announcing what the
/// large-file pre-scan (issue #161) found. In the spirit of
/// [`GateCommands::announcement_lines`]: emitted by the runner at the
/// start of the implement phase so an operator tailing the log can see,
/// per-run, how many over-cap files were flagged into the kickoff (or
/// that none were).
pub fn large_files_announcement(files: &[LargeFile]) -> String {
    if files.is_empty() {
        "  large-file pre-scan: no files estimated over ~20k tokens".to_string()
    } else {
        format!(
            "  large-file pre-scan: {} file(s) estimated over ~20k tokens, listed in the implement kickoff",
            files.len(),
        )
    }
}

pub async fn prepare(repo_url: &str, branch_name: &str) -> Result<Workspace, WorkspaceError> {
    prepare_with_gates(repo_url, branch_name, &GatesConfig::default()).await
}

/// `prepare` variant that accepts the operator-declared
/// `[gates].*_flags` fallback. The runner uses this so the
/// snapshotted gate commands on the returned `Workspace` reflect the
/// runtime configuration; callers without access to the config (e.g.
/// unit tests not exercising the cargo-checks gate) can keep using
/// the legacy `prepare(url, branch)` shape, which delegates here with
/// `GatesConfig::default()`.
pub async fn prepare_with_gates(
    repo_url: &str,
    branch_name: &str,
    gates: &GatesConfig,
) -> Result<Workspace, WorkspaceError> {
    let temp_dir = TempDir::new()?;
    let path = temp_dir.path();

    let status = Command::new("git")
        .arg("clone")
        .arg(repo_url)
        .arg(path)
        .status()
        .await?;
    if !status.success() {
        return Err(WorkspaceError::CloneFailed(status));
    }

    git(path, &["config", "user.email", "bellows@local"]).await?;
    git(path, &["config", "user.name", "Bellows"]).await?;

    // Bellows-managed local exclusions. .git/info/exclude is per-clone
    // and never committed — distinct from .gitignore which the agent
    // owns. Defends against agents that don't write a .gitignore from
    // committing canonical build-output directories on `git add -A`,
    // which slice X1's smoke test caught when the agent committed an
    // entire `target/` tree.
    let exclude_path = path.join(".git").join("info").join("exclude");
    let exclude_content =
        "# Bellows-managed local exclusions; never committed to the repo.\n\
         target/\n\
         node_modules/\n\
         __pycache__/\n\
         .bellows-*\n";
    tokio::fs::write(&exclude_path, exclude_content).await?;

    let default_branch = detect_default_branch(path).await?;

    git(path, &["checkout", "-b", branch_name]).await?;

    // ADR-0004 snapshot: parse the target repo's CI workflow ONCE here
    // and store the resolved (parsed-or-fallback) gate commands on the
    // Workspace. Both the post-implement and end-pipeline gates read
    // from this snapshot, so a mid-pipeline edit to
    // `.github/workflows/ci.yml` cannot shift the in-flight verdict.
    let extracted = parse_ci_workflow(path);
    let gate_commands = materialise_gate_commands(extracted, gates);

    // Issue #161 snapshot: walk the clone ONCE here and store the
    // over-large-file list on the Workspace. The implement kickoff reads
    // from this snapshot, so a mid-pipeline edit that adds or shrinks a
    // file cannot shift what the agent was told at kickoff time.
    let large_files = scan_large_files(path);

    Ok(Workspace {
        temp_dir,
        branch_name: branch_name.to_string(),
        default_branch,
        gate_commands,
        large_files,
    })
}

/// Merge the parser output with the operator-declared fallback flags.
/// Per-command: a `Some(cmd)` from the parser wins; a `None` falls
/// back to `cargo <subcommand> <flags>` from `gates`. The provenance
/// is reported per command so the run-log line attributes each gate
/// invocation to its actual source.
/// Issue #180: mirrored env travels with a *parsed* command only. A
/// command that fell back to `[gates].*_flags` is bellows's own posture,
/// not CI's, so pairing it with CI's environment would mirror half of
/// each and could produce a build posture neither side ever ran.
fn materialise_gate_commands(extracted: ExtractedCommands, gates: &GatesConfig) -> GateCommands {
    let (clippy, clippy_source, clippy_env, clippy_check) = match extracted.clippy {
        Some(cmd) => (
            cmd,
            extracted.source.clone(),
            extracted.clippy_env,
            extracted.clippy_check,
        ),
        None => (
            format!("cargo clippy {}", gates.clippy_flags),
            Provenance::FallbackFromConfig,
            Vec::new(),
            None,
        ),
    };
    let (test, test_source, test_env, test_check) = match extracted.test {
        Some(cmd) => (
            cmd,
            extracted.source,
            extracted.test_env,
            extracted.test_check,
        ),
        None => (
            format!("cargo test {}", gates.test_flags),
            Provenance::FallbackFromConfig,
            Vec::new(),
            None,
        ),
    };
    GateCommands {
        clippy,
        clippy_source,
        clippy_env,
        clippy_check,
        test,
        test_source,
        test_env,
        test_check,
    }
}

async fn detect_default_branch(repo: &Path) -> Result<String, WorkspaceError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--abbrev-ref", "origin/HEAD"])
        .output()
        .await?;
    if !output.status.success() {
        return Err(WorkspaceError::GitFailed {
            args: vec![
                "rev-parse".into(),
                "--abbrev-ref".into(),
                "origin/HEAD".into(),
            ],
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    Ok(raw
        .trim()
        .strip_prefix("origin/")
        .unwrap_or(raw.trim())
        .to_string())
}

/// Stage everything in the workspace and create a single commit. Used after
/// the sandbox has run; the caller does not know in advance which files were
/// produced, so we `git add -A` rather than naming files explicitly.
///
/// Returns [`WorkspaceError::NoChangesToCommit`] if the workspace is clean
/// after staging — this typically means the agent produced nothing, not a
/// genuine git failure.
pub async fn commit_all(workspace: &Workspace) -> Result<(), WorkspaceError> {
    git(workspace.path(), &["add", "-A"]).await?;

    // Detect "nothing to commit" via porcelain status before attempting a
    // commit, so we surface a clear error instead of git's terse exit 1.
    let status_output = Command::new("git")
        .arg("-C")
        .arg(workspace.path())
        .args(["status", "--porcelain"])
        .output()
        .await?;
    if !status_output.status.success() {
        return Err(WorkspaceError::GitFailed {
            args: vec!["status".into(), "--porcelain".into()],
            status: status_output.status,
            stderr: String::from_utf8_lossy(&status_output.stderr).into_owned(),
        });
    }
    if status_output.stdout.is_empty() {
        return Err(WorkspaceError::NoChangesToCommit);
    }

    git(workspace.path(), &["commit", "-m", "Bellows agent run"]).await?;
    Ok(())
}

/// The slice-9.6 four-corner commit/push pattern, packaged. Run this
/// after any agent invocation that may have left the workspace in
/// either of two shapes:
///
///   * Agent self-commit: `HEAD` advanced under the agent's own commit
///     message inside the sandbox. `commit_all` finds nothing to stage
///     and returns [`WorkspaceError::NoChangesToCommit`], but the
///     branch genuinely moved and we must push the agent's commit.
///   * Bellows-on-behalf: the agent left uncommitted edits in the
///     workspace. `commit_all` produces the boilerplate "Bellows agent
///     run" commit; `HEAD` advances here and we push that.
///
/// Both shapes (and a mixed shape where the agent commits *and* leaves
/// further edits) are collapsed by tracking `HEAD` movement
/// independently of `commit_all`'s return value. The push is gated on
/// `head_after != head_before`, so a genuinely-no-op invocation (no
/// commit, no edits) does NOT trigger a wasted no-op push.
///
/// Returns the post-commit `HEAD`. Callers that need to classify what
/// the agent did (e.g. the per-finding loop's `commit_landed` signal)
/// pair this with [`diff_between_touches_only_agent_notes`].
///
/// Issue #52 motivation: the nit-batch invocation used the legacy
/// `match commit_all { Ok(()) => push, NoChangesToCommit => {} }`
/// shape, which silently dropped agent-self-committed nit fixes — the
/// commit lived on local HEAD but never reached origin, and the
/// end-pipeline cargo-checks gate then ran against a workspace that
/// had diverged from the pushed branch. False-positive `FinalTestsRed`
/// classifications followed. Both the per-finding loop and the
/// nit-batch invocation now share this helper so the gap cannot
/// reappear at one site if the other is updated.
pub async fn commit_all_and_push_if_advanced(
    workspace: &Workspace,
    head_before: &str,
) -> Result<String, WorkspaceError> {
    match commit_all(workspace).await {
        Ok(()) | Err(WorkspaceError::NoChangesToCommit) => {}
        Err(e) => return Err(e),
    }
    let head_after = head_sha(workspace).await?;
    if head_after != head_before {
        push_branch(workspace).await?;
    }
    Ok(head_after)
}

/// Commit a set of files directly to `branch` on the workspace's
/// remote, bypassing the agent/* PR flow. Used by the `bellows
/// triage <N>` wontfix-enhancement path, which must land an
/// `.out-of-scope/<filename>.md` precedent on master so subsequent
/// triage runs see the new precedent in the workspace at clone time.
///
/// The helper fetches `branch` from origin, force-checks-out a local
/// branch tracking it (so a stale local copy from a prior op cannot
/// produce a wrong-base commit), writes each `(relative_path,
/// content)` pair (mkdir-ing parent directories as needed), stages
/// the paths, commits with `message`, and pushes. Multiple files
/// land in a single commit so the post-condition is a single new
/// commit on the branch.
///
/// The caller's workspace is left checked out on `branch` afterwards;
/// the workspace is discarded by `bellows triage` after this call so
/// the post-state of the local working copy is immaterial.
pub async fn commit_to_branch(
    workspace: &Workspace,
    branch: &str,
    message: &str,
    files: &[(String, String)],
) -> Result<(), WorkspaceError> {
    let path = workspace.path();

    // Bring `branch` up to date from origin and force-recreate the
    // local copy off it, so the commit's parent is the remote's
    // current tip rather than whatever the workspace had locally.
    git(path, &["fetch", "origin", branch]).await?;
    let origin_ref = format!("origin/{branch}");
    git(path, &["checkout", "-B", branch, &origin_ref]).await?;

    for (rel, content) in files {
        let abs = path.join(rel);
        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&abs, content).await?;
        git(path, &["add", rel]).await?;
    }

    git(path, &["commit", "-m", message]).await?;
    git(path, &["push", "origin", branch]).await?;
    Ok(())
}

/// Push the workspace's agent branch to origin.
///
/// Issue #113: the invocation is
/// `git push --force-with-lease=<branch> -u origin <branch>`. The
/// lease's expected value is the local remote-tracking ref, which git
/// maintains automatically across this pipeline's earlier pushes
/// within the same workspace. Four cases:
///
/// * **First push** (no remote-tracking ref yet): git's documented
///   fallback applies — `--force-with-lease=<branch>` with no recorded
///   expected value degrades to a plain push, so the implement-phase
///   first push behaves identically to the pre-#113 plain
///   `git push -u origin <branch>`.
/// * **Subsequent push, no rewrite**: the lease's expected value
///   matches; the push fast-forwards. All bellows-internal push paths
///   (security-fix, parser-as-backstop synth, end-of-pipeline) are
///   unchanged in shape.
/// * **Subsequent push, agent rewrote history** (the bug case
///   motivating #113): the lease matches because bellows is the sole
///   writer of `agent/*` per ADR-0003, so the force-update lands and
///   the run can finalise normally.
/// * **Anomaly — external writer changed origin between bellows
///   pushes**: the lease fails. The function returns
///   [`WorkspaceError::GitFailed`] with git's captured stderr in the
///   error's `stderr` field, so the runner's halt-log path can render
///   the lease-rejection message verbatim and an operator can
///   distinguish this from any other git failure.
///
/// ADR-0003's "Consequences" section names this `push_branch` lease
/// policy as the realisation of the forward-referenced "force-push as
/// a recovery primitive at push time."
///
/// Public function signature unchanged from the pre-#113 plain-push
/// shape, so all existing callsites compile without modification.
pub async fn push_branch(workspace: &Workspace) -> Result<(), WorkspaceError> {
    let lease_flag = format!("--force-with-lease={}", workspace.branch_name);
    let args = vec![
        "push".to_string(),
        lease_flag,
        "-u".to_string(),
        "origin".to_string(),
        workspace.branch_name.clone(),
    ];
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace.path())
        .args(&args)
        .output()
        .await?;
    if !output.status.success() {
        return Err(WorkspaceError::GitFailed {
            args,
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}

/// Capture the current `HEAD` SHA via `git rev-parse HEAD`. Used by
/// the slice-9.6 per-finding loop to detect whether the agent's
/// invocation advanced `HEAD` (an agent self-commit advances `HEAD`
/// without bellows's subsequent `commit_all` seeing anything to stage).
/// PR #38 review finding #1 fix: paired with
/// [`diff_between_touches_only_agent_notes`] so the per-finding
/// `commit_landed` signal handles all three commit-shape outcomes
/// (agent self-commit, bellows commit on behalf, no advancement).
pub async fn head_sha(workspace: &Workspace) -> Result<String, WorkspaceError> {
    head_sha_at(workspace.path()).await
}

/// [`head_sha`] against a bare path. Issue #164's workspace sampler
/// holds the bind-mounted path rather than the `Workspace` itself, and
/// needs the same reading to apply the at-base-SHA advance guard while
/// the container is still running.
pub async fn head_sha_at(repo: &Path) -> Result<String, WorkspaceError> {
    let stdout = git_stdout(repo, &["rev-parse", "HEAD"]).await?;
    Ok(String::from_utf8_lossy(&stdout).trim().to_string())
}

/// Sample the state of the agent's work in `repo` and reduce it to one
/// comparable hash (issue #164). The workspace is a host directory
/// bind-mounted into the agent's container (`src/sandbox.rs` mounts it
/// at `/workspace`), so bellows can run git against it from the host
/// while the container writes.
///
/// The sample is of the *work product*, not the directory:
///
///   1. `git diff HEAD` — every tracked modification, staged or not.
///   2. The sorted list of untracked-and-not-ignored paths from
///      `git status --porcelain`.
///
/// Going through git rather than hashing the directory tree is what
/// keeps `target/` out of the sample. Build artefacts churn on every
/// `cargo` invocation; a naive recursive file hash would report change
/// on every sample regardless of what the agent was doing, and no
/// stall would ever be detectable.
///
/// Reads race against the container writing, so a torn read is
/// possible. That is tolerable by construction: the classifier needs a
/// hash to repeat three times before it reports **Oscillation**, so a
/// spurious sample can at worst delay or skip a detection, never
/// invent one.
///
/// # Sampling a workspace the agent controls
///
/// Everything under the bind mount is attacker-controlled from the
/// host's point of view, `.git/` included. Git will happily execute
/// commands named by the repository it is pointed at — `diff.external`,
/// a `.gitattributes` driver backed by `diff.<driver>.command` or
/// `filter.<driver>.clean`, `core.fsmonitor` — so a sampler that ran
/// plain `git diff` against the live workspace would be a path for the
/// container to run code on the host. Flags alone do not close it:
/// `--no-ext-diff --no-textconv` stop the diff helpers, but any command
/// that compares worktree content against the index still runs the
/// clean filter named by `$GIT_DIR/info/attributes`, which the agent
/// can write.
///
/// So the sampler never lets git read the agent's configuration at all.
/// It builds a throwaway, bellows-owned git directory whose only link
/// to the workspace is `objects/info/alternates`, points git at it with
/// `--git-dir` plus `--work-tree`, and reads the worktree through that.
/// Attributes are sourced from the empty tree and the helper-bearing
/// config keys are overridden for good measure, so neither
/// `.git/config`, `.git/info/attributes` nor an in-tree `.gitattributes`
/// can name anything for the host to execute.
pub async fn sample_workspace_state(
    repo: &Path,
) -> Result<crate::policy::SampleHash, WorkspaceError> {
    sample_workspace_state_bounded(repo, SAMPLE_BYTE_LIMIT).await
}

/// [`sample_workspace_state`] with the per-shellout read limit spelled
/// out, so the bound itself is testable without materialising the
/// default's worth of workspace.
pub async fn sample_workspace_state_bounded(
    repo: &Path,
    limit: u64,
) -> Result<crate::policy::SampleHash, WorkspaceError> {
    let sample = SampleRepo::isolate(repo).await?;

    // Populate the throwaway index from the workspace's HEAD tree so
    // `git diff` has something to compare the worktree against.
    stream_sample_git(&sample, &["read-tree", &sample.head], limit, |_| {}).await?;

    let mut hasher = Sha256::new();
    stream_sample_git(
        &sample,
        &["diff", "--no-ext-diff", "--no-textconv", &sample.head],
        limit,
        |chunk| hasher.update(chunk),
    )
    .await?;

    // `git status --porcelain -z` marks untracked entries with `?? `,
    // and already excludes anything .gitignore covers. `-z` is what
    // makes the records parseable a chunk at a time without buffering
    // the whole listing, and leaves paths unquoted. Sorting makes the
    // sample independent of git's enumeration order.
    let mut scan = UntrackedScan::default();
    stream_sample_git(
        &sample,
        &["status", "--porcelain", "-z"],
        limit,
        |chunk| scan.push_chunk(chunk),
    )
    .await?;

    // Domain separator: without it a diff ending in a path-shaped line
    // and an untracked path could hash identically to a different
    // split of the same bytes.
    hasher.update(b"\0untracked\0");
    for path in scan.finish() {
        hasher.update(&path);
        hasher.update(b"\n");
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Ok(crate::policy::SampleHash::new(hex))
}

/// Hard cap on the bytes bellows will read from one sampling shellout.
/// The diff is streamed straight into the hasher rather than buffered,
/// so this is not what keeps memory flat — it is what stops a workspace
/// that has been made pathological on purpose (a multi-gigabyte tracked
/// diff, a flood of untracked paths) from costing the host unbounded
/// time and retained path bytes on every tick. Tripping the cap fails
/// the sample, and a failed sample is skipped by the caller: the
/// classifier needs a hash three times over before it reports anything,
/// so skipping can only delay or miss a detection, never invent one.
pub const SAMPLE_BYTE_LIMIT: u64 = 16 * 1024 * 1024;

/// The empty tree, which git synthesises in every repository without it
/// having to be written. Handed to git as `GIT_ATTR_SOURCE` so the
/// sampler resolves gitattributes against a tree that has none.
const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const EMPTY_TREE_SHA256: &str = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

/// A bellows-owned git directory that can read the agent's worktree
/// without reading the agent's repository configuration. See
/// [`sample_workspace_state`] for why that separation exists.
struct SampleRepo {
    /// Held for its `Drop`: the scratch directory lives exactly as long
    /// as the sample being taken from it.
    _scratch: TempDir,
    git_dir: PathBuf,
    work_tree: PathBuf,
    /// The workspace's `HEAD`, resolved to an object id before
    /// isolation so the sampler never has to consult the agent's refs.
    head: String,
    empty_tree: &'static str,
}

impl SampleRepo {
    async fn isolate(repo: &Path) -> Result<Self, WorkspaceError> {
        // `rev-parse` reads the agent's config but cannot be made to
        // execute anything out of it, so it is safe to ask the
        // workspace itself for the three facts isolation needs.
        let facts = git_stdout(
            repo,
            &[
                "rev-parse",
                "--absolute-git-dir",
                "--show-object-format",
                "HEAD",
            ],
        )
        .await?;
        let facts = String::from_utf8_lossy(&facts);
        let mut lines = facts.lines();
        let real_git_dir = lines.next().unwrap_or_default().trim();
        let object_format = lines.next().unwrap_or_default().trim();
        let head = lines.next().unwrap_or_default().trim();

        if real_git_dir.is_empty() {
            return Err(WorkspaceError::SampleIsolation(
                "git did not report an absolute git dir".to_string(),
            ));
        }
        // Both of these end up on a git command line and inside the
        // scratch config, so neither is taken on trust: an object id is
        // hex of a known width and an object format is one of two
        // words. Anything else fails the sample rather than being
        // passed through.
        if !(head.len() == 40 || head.len() == 64) || !head.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(WorkspaceError::SampleIsolation(format!(
                "HEAD did not resolve to an object id: {head:?}"
            )));
        }
        let (format_version, extensions, empty_tree) = match object_format {
            "sha1" => (0, "", EMPTY_TREE_SHA1),
            "sha256" => (1, "[extensions]\n\tobjectformat = sha256\n", EMPTY_TREE_SHA256),
            other => {
                return Err(WorkspaceError::SampleIsolation(format!(
                    "unknown object format {other:?}"
                )))
            }
        };

        let scratch = TempDir::new()?;
        let git_dir = scratch.path().to_path_buf();
        std::fs::create_dir_all(git_dir.join("objects/info"))?;
        std::fs::create_dir_all(git_dir.join("refs/heads"))?;
        std::fs::write(
            git_dir.join("config"),
            format!(
                "[core]\n\trepositoryformatversion = {format_version}\n\tbare = true\n\tlogallrefupdates = false\n{extensions}"
            ),
        )?;
        // Detached at the workspace's HEAD. The scratch repo borrows
        // the workspace's object store and nothing else.
        std::fs::write(git_dir.join("HEAD"), format!("{head}\n"))?;
        std::fs::write(
            git_dir.join("objects/info/alternates"),
            format!("{real_git_dir}/objects\n"),
        )?;

        Ok(Self {
            _scratch: scratch,
            git_dir,
            work_tree: repo.to_path_buf(),
            head: head.to_string(),
            empty_tree,
        })
    }
}

/// Run one sampling git command against the isolated repo, streaming
/// its stdout through `sink` and refusing to read past `limit` bytes.
///
/// stderr is discarded rather than piped: nothing reads it (a failed
/// sample is swallowed by the caller), and an unread pipe would be one
/// more thing an adversarial workspace could fill to wedge the host.
async fn stream_sample_git(
    sample: &SampleRepo,
    args: &[&str],
    limit: u64,
    mut sink: impl FnMut(&[u8]),
) -> Result<(), WorkspaceError> {
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(&sample.git_dir)
        .arg("--work-tree")
        .arg(&sample.work_tree)
        // Never write to the agent's repository, and never take a lock
        // in it that the container's own git could contend with.
        .arg("--no-optional-locks")
        // Belt and braces on top of the isolated git dir: even a
        // host-level config must not get to run a helper over
        // attacker-chosen attributes.
        .args(["-c", "core.fsmonitor=false", "-c", "core.attributesFile="])
        .args(args)
        .env("GIT_ATTR_SOURCE", sample.empty_tree)
        .env("GIT_ATTR_NOSYSTEM", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| WorkspaceError::SampleIsolation("git stdout was not piped".to_string()))?;

    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let read = tokio::io::AsyncReadExt::read(&mut stdout, &mut buf).await?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > limit {
            // Stop reading and stop the producer, so an oversized
            // sample costs a bounded amount of work rather than
            // whatever the workspace felt like emitting.
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(WorkspaceError::SampleTooLarge {
                args: args.iter().map(|a| (*a).to_string()).collect(),
                limit,
            });
        }
        sink(&buf[..read]);
    }

    let status = child.wait().await?;
    if !status.success() {
        return Err(WorkspaceError::GitFailed {
            args: args.iter().map(|a| (*a).to_string()).collect(),
            status,
            stderr: String::new(),
        });
    }
    Ok(())
}

/// Incremental parser over `git status --porcelain -z`, keeping only
/// the untracked paths. Records arrive NUL-terminated as `XY <path>`,
/// so the scan can run chunk by chunk and retain nothing but the `?? `
/// entries, rather than holding the whole listing in memory.
#[derive(Default)]
struct UntrackedScan {
    pending: Vec<u8>,
    untracked: Vec<Vec<u8>>,
    /// A rename or copy entry is followed by a second record carrying
    /// the origin path; it is a bare path, not an `XY `-prefixed
    /// record, so it must not be read as one.
    skip_next_record: bool,
}

impl UntrackedScan {
    fn push_chunk(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if byte == 0 {
                let record = std::mem::take(&mut self.pending);
                self.take_record(&record);
            } else {
                self.pending.push(byte);
            }
        }
    }

    fn take_record(&mut self, record: &[u8]) {
        if self.skip_next_record {
            self.skip_next_record = false;
            return;
        }
        if record.len() < 3 {
            return;
        }
        let (x, y) = (record[0], record[1]);
        if matches!(x, b'R' | b'C') || matches!(y, b'R' | b'C') {
            self.skip_next_record = true;
        }
        if x == b'?' && y == b'?' {
            self.untracked.push(record[3..].to_vec());
        }
    }

    /// Sorted untracked paths. A trailing unterminated record — a torn
    /// read against the writing container — is taken as a whole record;
    /// the worst it can do is perturb one sample's hash.
    fn finish(mut self) -> Vec<Vec<u8>> {
        if !self.pending.is_empty() {
            let record = std::mem::take(&mut self.pending);
            self.take_record(&record);
        }
        self.untracked.sort_unstable();
        self.untracked
    }
}

/// Run `git <args>` in `repo` and return its stdout, mapping a
/// non-zero exit to [`WorkspaceError::GitFailed`].
async fn git_stdout(repo: &Path, args: &[&str]) -> Result<Vec<u8>, WorkspaceError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await?;
    if !output.status.success() {
        return Err(WorkspaceError::GitFailed {
            args: args.iter().map(|a| (*a).to_string()).collect(),
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(output.stdout)
}

/// Discard every uncommitted change in the workspace, returning it to
/// `HEAD` (issue #164). Used by the **Advance** path: an advance
/// abandons the current engine's attempt and re-runs the phase from
/// base under the next chain entry, so the abandoned engine's
/// uncommitted edits must not leak into the fresh attempt.
///
/// `git clean -fd` (no `-x`) deliberately leaves ignored paths alone,
/// so the `target/` build cache survives the advance — rebuilding it
/// would cost the fresh engine minutes of the very budget the advance
/// exists to preserve.
pub async fn discard_uncommitted_changes(workspace: &Workspace) -> Result<(), WorkspaceError> {
    git_stdout(workspace.path(), &["reset", "--hard", "HEAD"]).await?;
    git_stdout(workspace.path(), &["clean", "-fd"]).await?;
    Ok(())
}

/// Whether `path` is a GitHub Actions workflow file: a `.yml` or
/// `.yaml` file directly under `.github/workflows/`. Mirrors GitHub
/// Actions' own discovery convention so the agent-PR-body callout
/// surfaces exactly the files CI itself would pick up.
///
/// The `.github/workflows/` prefix is load-bearing: a path that
/// merely contains `.github/` elsewhere — e.g. an issue template
/// under `.github/ISSUE_TEMPLATE/foo.yml` — does NOT qualify, since
/// the operator-visibility goal is to flag CI-shape changes and only
/// files under `.github/workflows/` change CI's shape.
///
/// Pure data: the predicate operates on a path string with no
/// filesystem access, so the file-path matching is unit-testable
/// independently of any git invocation.
pub fn is_workflow_file_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix(".github/workflows/") else {
        return false;
    };
    // Disallow nested subdirectories under .github/workflows/ —
    // GitHub Actions only discovers workflows directly under that
    // directory, not in nested subfolders. Equally, the empty `rest`
    // case (the directory entry itself) is not a workflow file.
    if rest.is_empty() || rest.contains('/') {
        return false;
    }
    rest.ends_with(".yml") || rest.ends_with(".yaml")
}

/// Workflow files (under `.github/workflows/`) touched between `base`
/// and `head`. Single `git diff --name-only <base> <head>` shellout
/// filtered through [`is_workflow_file_path`]; pure-data return so the
/// PR-body and run-log composers can call it independently and
/// unit-test the formatting separately from the git invocation.
///
/// Issue #111: surfaces the names of changed workflow files for the
/// operator-visibility callout. The callout warns that bellows's
/// cargo-checks gates only mirror `cargo clippy` and `cargo test`
/// from CI, so any other new steps in the changed workflow(s) are
/// exercised for the first time on the PR's real GitHub Actions run.
///
/// Returns `Ok(vec![])` when no workflow files were touched (the
/// common case) so the composers omit the callout entirely. Returns
/// `Err(WorkspaceError::GitFailed)` if the git invocation itself fails
/// — mirrors the error shape of the sibling
/// [`diff_between_touches_only_agent_notes`] helper.
pub async fn workflow_files_changed_between(
    workspace: &Workspace,
    base: &str,
    head: &str,
) -> Result<Vec<String>, WorkspaceError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace.path())
        .args(["diff", "--name-only", base, head])
        .output()
        .await?;
    if !output.status.success() {
        return Err(WorkspaceError::GitFailed {
            args: vec![
                "diff".into(),
                "--name-only".into(),
                base.into(),
                head.into(),
            ],
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter(|line| !line.is_empty())
        .filter(|line| is_workflow_file_path(line))
        .map(|line| line.to_string())
        .collect())
}

/// Whether the file list touched between `base` and `head` is exactly
/// `["bellows-agent-notes.md"]`. The general-case helper used by the slice-9.6
/// per-finding loop after PR #38: with the agent free to self-commit
/// its code fix under its own commit message, looking only at the most
/// recent commit (as the PR #37 helper did) is not enough — the runner
/// must consider the entire diff between the pre-invocation `HEAD` and
/// the post-invocation `HEAD`, which may span multiple commits authored
/// by either the agent or bellows.
///
/// Returns `Ok(false)` when `base == head` (the empty diff is not
/// exactly `["bellows-agent-notes.md"]`). The runner short-circuits before
/// reaching this helper on the no-advancement path anyway; the
/// `Ok(false)` contract is defensive consistency.
pub async fn diff_between_touches_only_agent_notes(
    workspace: &Workspace,
    base: &str,
    head: &str,
) -> Result<bool, WorkspaceError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace.path())
        .args(["diff", "--name-only", base, head])
        .output()
        .await?;
    if !output.status.success() {
        return Err(WorkspaceError::GitFailed {
            args: vec![
                "diff".into(),
                "--name-only".into(),
                base.into(),
                head.into(),
            ],
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    Ok(files.len() == 1 && files[0] == "bellows-agent-notes.md")
}

/// Capture `git diff <default_branch>...HEAD` and write it to
/// `dest_filename` (a workspace-relative path). Used by the runner to
/// feed the implement-phase diff into the review-phase claude run via
/// a workspace file rather than a `gh pr diff` call inside the
/// container.
///
/// Uses three dots (`<base>...HEAD`) so the diff is exactly what the
/// PR would show — only commits unique to this branch since it
/// diverged from the base.
pub async fn generate_diff(
    workspace: &Workspace,
    dest_filename: &str,
) -> Result<(), WorkspaceError> {
    let diff = compute_diff_against_base(workspace).await?;
    tokio::fs::write(workspace.path().join(dest_filename), diff.as_bytes()).await?;
    Ok(())
}

/// Capture `git log --name-status <default_branch>...HEAD` and write
/// it to `dest_filename` (a workspace-relative path). Sibling of
/// `generate_diff`: the reviewer reads this file alongside the squashed
/// diff as *optional* ordering context (which files arrived in which
/// commit — something the diff cannot show). It no longer feeds any
/// commit-shape check; that test-first review backstop was removed
/// because bellows commits once per phase, making the shape it demanded
/// unreachable (ADR-0012 / issue #154). The artefact is retained
/// deliberately as reviewer context.
///
/// Uses three dots (`<base>...HEAD`) so the range matches what the PR
/// would show — only commits unique to this branch since divergence.
/// `--name-status` annotates each commit with the touched files plus
/// their status (`A`/`M`/`D`), which is what makes test-file vs
/// source-file ordering inspectable. An empty range (branch at parity
/// with base) produces an empty file rather than an error — the
/// reviewer sees "no commits to reason about" rather than a missing
/// artefact.
pub async fn generate_commit_log(
    workspace: &Workspace,
    dest_filename: &str,
) -> Result<(), WorkspaceError> {
    let spec = format!("{}...HEAD", workspace.default_branch);
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace.path())
        .args(["log", "--name-status", &spec])
        .output()
        .await?;
    if !output.status.success() {
        return Err(WorkspaceError::GitFailed {
            args: vec!["log".into(), "--name-status".into(), spec.clone()],
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let log_text = String::from_utf8_lossy(&output.stdout);
    tokio::fs::write(workspace.path().join(dest_filename), log_text.as_bytes()).await?;
    Ok(())
}

/// Capture `git diff <default_branch>...HEAD` and return it as a
/// String. Sibling of `generate_diff` for callers that want to scan
/// the diff directly (the slice-8 weak-test guard) rather than write
/// it to a workspace file.
///
/// Uses three dots (`<base>...HEAD`) so the diff matches what the
/// PR would show — commits unique to this branch since divergence.
/// Returns an empty string when the branch is at parity with base.
///
/// `git diff` output is UTF-8 in practice (Rust source files are
/// UTF-8); `from_utf8_lossy` defends against the rare binary-file
/// case so a stray non-UTF-8 byte in a diff doesn't surface as an
/// IO error.
pub async fn compute_diff_against_base(
    workspace: &Workspace,
) -> Result<String, WorkspaceError> {
    let spec = format!("{}...HEAD", workspace.default_branch);
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace.path())
        .args(["diff", &spec])
        .output()
        .await?;
    if !output.status.success() {
        return Err(WorkspaceError::GitFailed {
            args: vec!["diff".into(), spec.clone()],
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[derive(Debug, Deserialize)]
pub struct Pr {
    pub number: u64,
    pub html_url: String,
}

/// Inputs for `open_pr`. Bundled into a struct rather than passed as
/// 8 positional arguments — clippy was already flagging the count and
/// later slices may add fields (reviewers, assignees, etc.).
pub struct OpenPrRequest<'a> {
    pub owner: &'a str,
    pub repo: &'a str,
    pub head_branch: &'a str,
    pub base_branch: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub draft: bool,
}

pub async fn open_pr(
    client: &octocrab::Octocrab,
    req: OpenPrRequest<'_>,
) -> Result<Pr, octocrab::Error> {
    let route = format!("/repos/{owner}/{repo}/pulls", owner = req.owner, repo = req.repo);
    let payload = serde_json::json!({
        "title": req.title,
        "head": req.head_branch,
        "base": req.base_branch,
        "body": req.body,
        "draft": req.draft,
    });
    let pr: Pr = client.post(&route, Some(&payload)).await?;
    Ok(pr)
}

async fn git(cwd: &Path, args: &[&str]) -> Result<(), WorkspaceError> {
    let status = Command::new("git").arg("-C").arg(cwd).args(args).status().await?;
    if !status.success() {
        return Err(WorkspaceError::GitFailed {
            args: args.iter().map(|s| s.to_string()).collect(),
            status,
            stderr: String::new(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_workflow_file_path_accepts_yml_under_dot_github_workflows() {
        // Issue #111 AC: the canonical workflow path shape qualifies.
        // GitHub Actions discovers workflows under `.github/workflows/`
        // with either `.yml` or `.yaml` extensions; the predicate
        // mirrors that discovery rule.
        assert!(is_workflow_file_path(".github/workflows/ci.yml"));
    }

    #[test]
    fn is_workflow_file_path_accepts_yaml_extension_too() {
        // Issue #111 AC: both `.yml` and `.yaml` qualify, matching
        // GitHub Actions' own discovery convention. The pair must
        // share a single test to pin the equivalence so a future
        // refactor that drops `.yaml` (the less common form in this
        // codebase) flips the test red.
        assert!(is_workflow_file_path(".github/workflows/release.yaml"));
    }

    #[test]
    fn is_workflow_file_path_rejects_other_yml_under_dot_github() {
        // Issue #111 AC: a path that merely contains `.github/`
        // elsewhere — e.g. an issue template — must NOT qualify.
        // The predicate keys on the `.github/workflows/` prefix, not
        // on `.github/` alone.
        assert!(!is_workflow_file_path(".github/ISSUE_TEMPLATE/foo.yml"));
    }

    #[test]
    fn is_workflow_file_path_rejects_non_yaml_extensions_under_workflows() {
        // Defensive: a stray `.md` or `.json` under `.github/workflows/`
        // is not a workflow file as far as GitHub Actions is concerned.
        // Only `.yml` and `.yaml` qualify.
        assert!(!is_workflow_file_path(".github/workflows/README.md"));
        assert!(!is_workflow_file_path(".github/workflows/data.json"));
    }

    #[test]
    fn is_workflow_file_path_rejects_path_outside_dot_github_workflows() {
        // Defensive: a `.yml` file at the repo root, or under `src/`,
        // is not a workflow file. The `.github/workflows/` prefix is
        // load-bearing.
        assert!(!is_workflow_file_path("ci.yml"));
        assert!(!is_workflow_file_path("src/config.yaml"));
    }
}
