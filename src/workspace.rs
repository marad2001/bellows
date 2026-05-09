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

pub async fn push_branch(workspace: &Workspace) -> Result<(), WorkspaceError> {
    git(
        workspace.path(),
        &["push", "-u", "origin", &workspace.branch_name],
    )
    .await
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
