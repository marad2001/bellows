use std::io::Write;

use crate::auth::Auth;
use crate::config::{AuthMethod, Config};
use crate::policy;
use crate::sandbox::{self, SandboxError};
use crate::tracker::{self, ClaimError};
use crate::workspace::{self, WorkspaceError};

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("github: {0}")]
    Octocrab(#[from] octocrab::Error),
    #[error("workspace: {0}")]
    Workspace(#[from] WorkspaceError),
    #[error("sandbox: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("repo url is not in the form https://host/owner/repo: {0}")]
    InvalidRepoUrl(String),
    #[error(
        "issue #{0} is labelled ready-for-agent but no `## Agent Brief` comment was found; \
         move it back to needs-triage and write the brief"
    )]
    MissingAgentBrief(u64),
}

#[derive(Debug)]
pub enum RunOutcome {
    Idle,
    Finalised { issue_number: u64, pr_number: u64 },
    Contended { issue_number: u64 },
}

pub async fn run_once(
    client: &octocrab::Octocrab,
    config: &Config,
    log_writer: &mut dyn Write,
) -> Result<RunOutcome, RunError> {
    let (owner, repo) = parse_owner_repo(&config.repo.url)?;

    let issue = tracker::find_next_issue(
        client,
        &owner,
        &repo,
        &config.polling.pickup_label,
        &config.runtime_labels.agent_in_progress,
    )
    .await?;

    let Some(issue) = issue else {
        return Ok(RunOutcome::Idle);
    };

    let claimed = match tracker::claim(
        client,
        &owner,
        &repo,
        issue.number,
        &config.polling.pickup_label,
        &config.runtime_labels.agent_in_progress,
    )
    .await
    {
        Ok(c) => c,
        Err(ClaimError::Contended) => {
            return Ok(RunOutcome::Contended {
                issue_number: issue.number,
            });
        }
        Err(ClaimError::Octocrab(e)) => return Err(RunError::Octocrab(e)),
    };

    let started = chrono::Utc::now();
    let branch_name = crate::agent_branch_name(claimed.number, &claimed.title);

    let brief = tracker::fetch_agent_brief(client, &owner, &repo, claimed.number)
        .await?
        .ok_or(RunError::MissingAgentBrief(claimed.number))?;

    let workspace = workspace::prepare(&config.repo.url, &branch_name).await?;

    let kickoff = policy::render_kickoff(&brief, &config.repo.url, &branch_name);
    tokio::fs::write(workspace.path().join(".bellows-kickoff.md"), &kickoff).await?;

    let auth = match config.auth.method {
        AuthMethod::Subscription => Auth::Subscription {
            credentials_volume_name: config.auth.credentials_volume.clone(),
        },
    };

    sandbox::run_agent(&workspace, &auth, claimed.number, log_writer).await?;

    // If claude wrote a PR description file, capture + remove it before
    // committing so it does not appear in the diff. Fall back to a
    // boilerplate body otherwise.
    let pr_description_path = workspace.path().join(".bellows-pr-description.md");
    let claude_pr_body = if pr_description_path.exists() {
        let body = tokio::fs::read_to_string(&pr_description_path).await?;
        tokio::fs::remove_file(&pr_description_path).await?;
        Some(body.trim().to_string())
    } else {
        None
    };

    workspace::commit_all(&workspace).await?;
    workspace::push_branch(&workspace).await?;

    let pr_title = format!("Bellows agent run for issue #{}", claimed.number);
    let pr_body = match claude_pr_body {
        Some(body) => format!("Closes #{}.\n\n{}", claimed.number, body),
        None => format!(
            "Closes #{}.\n\n_(Run produced by Bellows v1; the agent did not write a PR description.)_",
            claimed.number
        ),
    };
    let pr = workspace::open_pr(
        client,
        &owner,
        &repo,
        &branch_name,
        workspace.default_branch(),
        &pr_title,
        &pr_body,
    )
    .await?;

    let finished = chrono::Utc::now();
    let log_body = format!(
        "<details><summary>Bellows run log</summary>\n\nIssue: #{}\nClaimed at: {}\nFinalised at: {}\nBranch: `{}`\n</details>",
        claimed.number,
        started.to_rfc3339(),
        finished.to_rfc3339(),
        branch_name,
    );

    tracker::finalise_success(
        client,
        &owner,
        &repo,
        claimed.number,
        pr.number,
        &config.runtime_labels.agent_in_progress,
        &config.runtime_labels.agent_done,
        &log_body,
    )
    .await?;

    Ok(RunOutcome::Finalised {
        issue_number: claimed.number,
        pr_number: pr.number,
    })
}

fn parse_owner_repo(url: &str) -> Result<(String, String), RunError> {
    // Only http(s):// URLs are supported. SSH (`git@host:owner/repo`) and
    // local paths can clone fine, but the (owner, repo) tuple they produce
    // would be wrong for the GitHub API calls.
    let after_scheme = match url.split_once("://") {
        Some((scheme, rest)) if scheme == "http" || scheme == "https" => rest,
        _ => return Err(RunError::InvalidRepoUrl(url.to_string())),
    };
    let trimmed = after_scheme.trim_end_matches('/').trim_end_matches(".git");
    let segments: Vec<&str> = trimmed.split('/').collect();
    // Expecting host / owner / repo at minimum.
    if segments.len() < 3 || segments.iter().any(|s| s.is_empty()) {
        return Err(RunError::InvalidRepoUrl(url.to_string()));
    }
    Ok((segments[1].to_string(), segments[2].to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_owner_repo_https_happy_path() {
        let (owner, repo) =
            parse_owner_repo("https://github.com/marad2001/bellows-test").unwrap();
        assert_eq!(owner, "marad2001");
        assert_eq!(repo, "bellows-test");
    }

    #[test]
    fn parse_owner_repo_strips_trailing_slash_and_dot_git() {
        let (owner, repo) =
            parse_owner_repo("https://github.com/marad2001/bellows-test.git/").unwrap();
        assert_eq!(owner, "marad2001");
        assert_eq!(repo, "bellows-test");
    }

    #[test]
    fn parse_owner_repo_rejects_ssh_url() {
        let err = parse_owner_repo("git@github.com:marad2001/bellows-test.git").unwrap_err();
        assert!(matches!(err, RunError::InvalidRepoUrl(_)), "got {:?}", err);
    }

    #[test]
    fn parse_owner_repo_rejects_local_path() {
        let err = parse_owner_repo("/tmp/bellows-test").unwrap_err();
        assert!(matches!(err, RunError::InvalidRepoUrl(_)), "got {:?}", err);
    }

    #[test]
    fn parse_owner_repo_rejects_url_with_too_few_segments() {
        let err = parse_owner_repo("https://github.com/marad2001").unwrap_err();
        assert!(matches!(err, RunError::InvalidRepoUrl(_)), "got {:?}", err);
    }
}
