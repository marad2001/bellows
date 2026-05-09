use std::io::Write;

use crate::auth::Auth;
use crate::config::{AuthMethod, Config};
use crate::policy::{self, ExitReason};
use crate::sandbox::{self, AgentRun, CargoTestRun, SandboxError};
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
    Finalised {
        issue_number: u64,
        pr_number: u64,
        reason: ExitReason,
    },
    Contended {
        issue_number: u64,
    },
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

    // Fetch the agent brief BEFORE claiming. If it's missing we return
    // an error without label-swapping the issue — the next polling tick
    // will see it fresh once a human posts the brief, instead of leaving
    // it stuck in agent-in-progress with no automated recovery.
    let brief = tracker::fetch_agent_brief(client, &owner, &repo, issue.number)
        .await?
        .ok_or(RunError::MissingAgentBrief(issue.number))?;

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

    let workspace = workspace::prepare(&config.repo.url, &branch_name).await?;

    let kickoff = policy::render_kickoff(&brief, &config.repo.url, &branch_name);
    tokio::fs::write(workspace.path().join(".bellows-kickoff.md"), &kickoff).await?;

    let auth = match config.auth.method {
        AuthMethod::Subscription => Auth::Subscription {
            credentials_volume_name: config.auth.credentials_volume.clone(),
        },
    };

    let agent_run = sandbox::run_agent(&workspace, &auth, claimed.number, log_writer).await?;

    // If the agent wrote a self-report blocker file, capture its content.
    // Do NOT remove — it stays in the workspace and ends up in the commit
    // so the human reviewer can see what the agent struggled with.
    let agent_notes_path = workspace.path().join("agent-notes.md");
    let agent_notes = if agent_notes_path.exists() {
        Some(
            tokio::fs::read_to_string(&agent_notes_path)
                .await?
                .trim()
                .to_string(),
        )
    } else {
        None
    };

    // If the agent wrote a PR description file, capture + remove it
    // before committing so it does NOT appear in the diff.
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

    // Run the cargo test sanity gate inside a fresh container, but ONLY
    // when the workspace looks like a Rust project (has Cargo.toml at
    // the root). For non-Rust briefs the gate is skipped and the run is
    // treated as success — see policy::classify_exit's None branch.
    let cargo_test_run: Option<CargoTestRun> = if workspace.path().join("Cargo.toml").exists() {
        Some(sandbox::run_cargo_test(&workspace, log_writer).await?)
    } else {
        None
    };
    let cargo_test_result: Option<i64> = cargo_test_run.as_ref().map(|r| r.exit_code);

    let reason = policy::classify_exit(
        agent_run.exit_code,
        agent_notes.is_some(),
        cargo_test_result,
    );

    let draft = !matches!(reason, ExitReason::Success);
    let outcome_label = match reason {
        ExitReason::Success => &config.runtime_labels.agent_done,
        _ => &config.runtime_labels.agent_failed,
    };

    let pr_title = format!("Bellows agent run for issue #{}", claimed.number);
    let pr_body = build_pr_body(
        &reason,
        claimed.number,
        claude_pr_body.as_deref(),
        agent_notes.as_deref(),
    );

    let pr = workspace::open_pr(
        client,
        &owner,
        &repo,
        &branch_name,
        workspace.default_branch(),
        &pr_title,
        &pr_body,
        draft,
    )
    .await?;

    let finished = chrono::Utc::now();
    let log_body = build_log_body(
        &reason,
        claimed.number,
        started,
        finished,
        &branch_name,
        &agent_run,
        cargo_test_run.as_ref(),
    );

    tracker::finalise(
        client,
        &owner,
        &repo,
        claimed.number,
        pr.number,
        &config.runtime_labels.agent_in_progress,
        outcome_label,
        &log_body,
    )
    .await?;

    Ok(RunOutcome::Finalised {
        issue_number: claimed.number,
        pr_number: pr.number,
        reason,
    })
}

fn build_pr_body(
    reason: &ExitReason,
    issue_number: u64,
    claude_pr_body: Option<&str>,
    agent_notes: Option<&str>,
) -> String {
    let header = format!("Closes #{issue_number}.\n\n");
    let body = match reason {
        ExitReason::Success => claude_pr_body
            .map(str::to_string)
            .unwrap_or_else(|| {
                "_(Run produced by Bellows v1; the agent did not write a PR description.)_"
                    .to_string()
            }),
        ExitReason::AgentSelfReportedFailure => format!(
            "## Agent self-reported failure\n\n\
             The agent wrote `agent-notes.md` rather than complete the brief. The notes are committed in this PR's diff; quoted below for convenience.\n\n\
             ```\n{}\n```\n\n\
             See the run-log comment on this PR for the agent's output tail.",
            agent_notes.unwrap_or("(no notes content captured)")
        ),
        ExitReason::Crash => {
            "## Agent run crashed\n\n\
             The container exited non-zero before the agent could finish. See the run-log comment on this PR for the stderr tail."
                .to_string()
        }
        ExitReason::FinalTestsRed => {
            "## `cargo test` failed after the agent's run\n\n\
             The agent reported done with exit 0 but the post-run test gate caught failing tests. See the run-log comment on this PR for the full test output."
                .to_string()
        }
    };
    header + &body
}

fn build_log_body(
    reason: &ExitReason,
    issue_number: u64,
    started: chrono::DateTime<chrono::Utc>,
    finished: chrono::DateTime<chrono::Utc>,
    branch_name: &str,
    agent_run: &AgentRun,
    cargo_test_run: Option<&CargoTestRun>,
) -> String {
    let mut body = format!(
        "<details><summary>Bellows run log ({reason:?})</summary>\n\n\
         Issue: #{issue_number}\n\
         Claimed at: {started_rfc}\n\
         Finalised at: {finished_rfc}\n\
         Branch: `{branch_name}`\n\
         Agent exit code: {agent_exit}\n",
        started_rfc = started.to_rfc3339(),
        finished_rfc = finished.to_rfc3339(),
        agent_exit = agent_run.exit_code,
    );

    if !matches!(reason, ExitReason::Success) {
        body.push_str("\n### Agent output tail\n\n```\n");
        body.push_str(&agent_run.stderr_tail);
        body.push_str("\n```\n");

        if let Some(test_run) = cargo_test_run {
            body.push_str(&format!(
                "\n### `cargo test` output (exit {code})\n\n```\n{output}\n```\n",
                code = test_run.exit_code,
                output = test_run.output,
            ));
        }
    } else if let Some(test_run) = cargo_test_run {
        body.push_str(&format!(
            "\nCargo test gate: exit {} (passed)\n",
            test_run.exit_code
        ));
    }

    body.push_str("\n</details>");
    body
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
