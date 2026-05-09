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
}

pub struct Workspace {
    temp_dir: TempDir,
    branch_name: String,
}

impl Workspace {
    pub fn path(&self) -> &Path {
        self.temp_dir.path()
    }

    pub fn branch_name(&self) -> &str {
        &self.branch_name
    }
}

pub const MARKER_FILE: &str = ".bellows-stub-marker";

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
    git(path, &["checkout", "-b", branch_name]).await?;

    Ok(Workspace {
        temp_dir,
        branch_name: branch_name.to_string(),
    })
}

pub async fn commit_marker(workspace: &Workspace, content: &str) -> Result<(), WorkspaceError> {
    tokio::fs::write(workspace.path().join(MARKER_FILE), content).await?;
    git(workspace.path(), &["add", MARKER_FILE]).await?;
    git(workspace.path(), &["commit", "-m", "Bellows stub run marker"]).await?;
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

pub async fn open_pr(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    head_branch: &str,
    base_branch: &str,
    title: &str,
    body: &str,
) -> Result<Pr, octocrab::Error> {
    let route = format!("/repos/{owner}/{repo}/pulls");
    let payload = serde_json::json!({
        "title": title,
        "head": head_branch,
        "base": base_branch,
        "body": body,
        "draft": false,
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
