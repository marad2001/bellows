use std::path::Path;

use serde::Deserialize;
use tempfile::TempDir;
use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("git clone failed (status {0})")]
    CloneFailed(std::process::ExitStatus),
    #[error("git {args:?} failed (status {status})")]
    GitFailed {
        args: Vec<String>,
        status: std::process::ExitStatus,
    },
    #[error("agent produced no changes to commit; the brief was probably unmet")]
    NoChangesToCommit,
}

pub struct Workspace {
    temp_dir: TempDir,
    branch_name: String,
    default_branch: String,
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
}

pub async fn prepare(repo_url: &str, branch_name: &str) -> Result<Workspace, WorkspaceError> {
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

    Ok(Workspace {
        temp_dir,
        branch_name: branch_name.to_string(),
        default_branch,
    })
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

pub async fn push_branch(workspace: &Workspace) -> Result<(), WorkspaceError> {
    git(
        workspace.path(),
        &["push", "-u", "origin", &workspace.branch_name],
    )
    .await
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
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .await?;
    if !output.status.success() {
        return Err(WorkspaceError::GitFailed {
            args: vec!["rev-parse".into(), "HEAD".into()],
            status: output.status,
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Whether the file list touched between `base` and `head` is exactly
/// `["agent-notes.md"]`. The general-case helper used by the slice-9.6
/// per-finding loop after PR #38: with the agent free to self-commit
/// its code fix under its own commit message, looking only at the most
/// recent commit (as the PR #37 helper did) is not enough — the runner
/// must consider the entire diff between the pre-invocation `HEAD` and
/// the post-invocation `HEAD`, which may span multiple commits authored
/// by either the agent or bellows.
///
/// Returns `Ok(false)` when `base == head` (the empty diff is not
/// exactly `["agent-notes.md"]`). The runner short-circuits before
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
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    Ok(files.len() == 1 && files[0] == "agent-notes.md")
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
        });
    }
    Ok(())
}
