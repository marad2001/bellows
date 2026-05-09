use crate::config::Config;
use crate::tracker::{self, ClaimError};
use crate::workspace::{self, WorkspaceError};

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("github: {0}")]
    Octocrab(#[from] octocrab::Error),
    #[error("workspace: {0}")]
    Workspace(#[from] WorkspaceError),
    #[error("repo url is not in the form https://host/owner/repo: {0}")]
    InvalidRepoUrl(String),
}

impl From<ClaimError> for RunError {
    fn from(e: ClaimError) -> Self {
        match e {
            ClaimError::Contended => unreachable!("Contended is handled inline in run_once"),
            ClaimError::Octocrab(e) => RunError::Octocrab(e),
        }
    }
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
    let branch_name = format!(
        "agent/{}-{}",
        claimed.number,
        crate::slugify_title(&claimed.title)
    );

    let workspace = workspace::prepare(&config.repo.url, &branch_name).await?;
    let marker_content = format!(
        "issue=#{} timestamp={}\n",
        claimed.number,
        started.to_rfc3339()
    );
    workspace::commit_marker(&workspace, &marker_content).await?;
    workspace::push_branch(&workspace).await?;

    let pr_title = format!("Bellows stub run for issue #{}", claimed.number);
    let pr_body = format!(
        "Closes #{}.\n\n_(Stub run produced by Bellows v1, slice 1.)_",
        claimed.number
    );
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
        "<details><summary>Bellows run log</summary>\n\nIssue: #{}\nClaimed at: {}\nFinalised at: {}\nBranch: `{}`\nMarker file: `.bellows-stub-marker`\n</details>",
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
    let stripped = url.trim_end_matches('/').trim_end_matches(".git");
    let mut parts: Vec<&str> = stripped.rsplitn(3, '/').take(2).collect();
    parts.reverse();
    if parts.len() < 2 || parts.iter().any(|s| s.is_empty()) {
        return Err(RunError::InvalidRepoUrl(url.to_string()));
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}
