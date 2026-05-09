use std::io::Write;

use crate::auth::Auth;
use crate::config::{AuthMethod, Config};
use crate::policy::{
    self, CheckResult, ExitReason, GateOutcome, ImplementOutcome, PhaseOutcomes,
};
use crate::sandbox::{self, CargoTestRun, SandboxError};
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
    // Slice X1 introduces PhaseOutcomes. Until the review/end-gate phases
    // land later in this slice, only the post-implement gate is populated;
    // the rest stays `None`. classify_exit's behaviour is preserved.
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: agent_run.exit_code,
            stderr_tail: agent_run.stderr_tail.clone(),
        },
        post_implement_gate: GateOutcome {
            cargo_clippy: None,
            cargo_test: cargo_test_run.as_ref().map(|r| CheckResult {
                exit_code: r.exit_code,
                output: r.output.clone(),
            }),
        },
        review: None,
        review_fix: None,
        end_pipeline_gate: None,
    };
    let reason = policy::classify_exit(agent_notes.is_some(), &outcomes);

    let draft = !matches!(reason, ExitReason::Success);
    // Exhaustive match — no `_ => ...` fallthrough — so when slice 6
    // adds RateLimited / WallClockExceeded variants the compiler will
    // refuse to build until we make an explicit decision per variant.
    let outcome_label = match reason {
        ExitReason::Success => &config.runtime_labels.agent_done,
        ExitReason::AgentSelfReportedFailure
        | ExitReason::Crash
        | ExitReason::FinalTestsRed => &config.runtime_labels.agent_failed,
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
        &outcomes,
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
    outcomes: &PhaseOutcomes,
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
        agent_exit = outcomes.implement.exit_code,
    );

    let post_clippy = outcomes.post_implement_gate.cargo_clippy.as_ref();
    let post_test = outcomes.post_implement_gate.cargo_test.as_ref();

    if !matches!(reason, ExitReason::Success) {
        body.push_str("\n### Agent output tail\n\n```\n");
        body.push_str(&outcomes.implement.stderr_tail);
        body.push_str("\n```\n");

        if let Some(clippy) = post_clippy {
            body.push_str(&format!(
                "\n### `cargo clippy` output (exit {code})\n\n```\n{output}\n```\n",
                code = clippy.exit_code,
                output = clippy.output,
            ));
        }
        if let Some(test_run) = post_test {
            body.push_str(&format!(
                "\n### `cargo test` output (exit {code})\n\n```\n{output}\n```\n",
                code = test_run.exit_code,
                output = test_run.output,
            ));
        }
    } else if let Some(test_run) = post_test {
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

    /// Construct a slice-5-shaped `PhaseOutcomes` for build_log_body tests:
    /// implement run with the given exit + stderr tail, and an
    /// `Option<cargo test>` in the post-implement gate. Clippy stays
    /// `None` (slice-5 didn't run clippy); review/end-gate stay `None`
    /// (slice-5 didn't have those phases).
    fn slice5_log_outcomes(implement_exit: i64, tail: &str, test: Option<(i64, &str)>) -> PhaseOutcomes {
        PhaseOutcomes {
            implement: ImplementOutcome {
                exit_code: implement_exit,
                stderr_tail: tail.to_string(),
            },
            post_implement_gate: GateOutcome {
                cargo_clippy: None,
                cargo_test: test.map(|(exit, output)| CheckResult {
                    exit_code: exit,
                    output: output.to_string(),
                }),
            },
            review: None,
            review_fix: None,
            end_pipeline_gate: None,
        }
    }

    fn fixed_timestamp() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-05-09T20:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn build_pr_body_for_success_uses_claude_pr_body_when_present() {
        let body = build_pr_body(&ExitReason::Success, 42, Some("My PR body."), None);
        assert!(body.starts_with("Closes #42.\n\n"));
        assert!(body.contains("My PR body."));
    }

    #[test]
    fn build_pr_body_for_success_uses_boilerplate_when_no_pr_body() {
        let body = build_pr_body(&ExitReason::Success, 42, None, None);
        assert!(body.contains("the agent did not write a PR description"));
    }

    #[test]
    fn build_pr_body_for_self_reported_failure_quotes_agent_notes() {
        let body = build_pr_body(
            &ExitReason::AgentSelfReportedFailure,
            42,
            None,
            Some("I got stuck on the brief."),
        );
        assert!(body.contains("self-reported failure"));
        assert!(body.contains("I got stuck on the brief."));
    }

    #[test]
    fn build_pr_body_for_crash_mentions_stderr_tail_pointer() {
        let body = build_pr_body(&ExitReason::Crash, 42, None, None);
        assert!(body.contains("crashed"));
        assert!(body.contains("stderr tail"));
    }

    #[test]
    fn build_pr_body_for_final_tests_red_mentions_test_output_pointer() {
        let body = build_pr_body(&ExitReason::FinalTestsRed, 42, None, None);
        assert!(body.contains("`cargo test` failed"));
        assert!(body.contains("full test output"));
    }

    #[test]
    fn build_log_body_for_success_skips_failure_sections() {
        let started = fixed_timestamp();
        let finished = started;
        let body = build_log_body(
            &ExitReason::Success,
            42,
            started,
            finished,
            "agent/42-x",
            &slice5_log_outcomes(0, "not shown", None),
        );
        assert!(body.contains("Bellows run log (Success)"));
        assert!(!body.contains("### Agent output tail"));
        assert!(!body.contains("### `cargo test` output"));
    }

    #[test]
    fn build_log_body_for_final_tests_red_includes_agent_tail_and_cargo_output() {
        let started = fixed_timestamp();
        let finished = started;
        let body = build_log_body(
            &ExitReason::FinalTestsRed,
            42,
            started,
            finished,
            "agent/42-x",
            &slice5_log_outcomes(0, "agent told you it was done", Some((101, "test foo ... FAILED"))),
        );
        assert!(body.contains("FinalTestsRed"));
        assert!(body.contains("### Agent output tail"));
        assert!(body.contains("agent told you it was done"));
        assert!(body.contains("### `cargo test` output (exit 101)"));
        assert!(body.contains("test foo ... FAILED"));
    }

    #[test]
    fn build_log_body_for_final_tests_red_attributes_clippy_failure() {
        // Clippy failed (exit 101) in the post-implement gate; cargo test
        // never ran. The log body should name clippy and include its
        // output, and should NOT include a cargo-test section.
        let started = fixed_timestamp();
        let finished = started;
        let outcomes = PhaseOutcomes {
            implement: ImplementOutcome {
                exit_code: 0,
                stderr_tail: "agent done".to_string(),
            },
            post_implement_gate: GateOutcome {
                cargo_clippy: Some(CheckResult {
                    exit_code: 101,
                    output: "warning: this is a clippy lint".to_string(),
                }),
                cargo_test: None,
            },
            review: None,
            review_fix: None,
            end_pipeline_gate: None,
        };
        let body = build_log_body(
            &ExitReason::FinalTestsRed,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        );
        assert!(body.contains("FinalTestsRed"));
        assert!(body.contains("clippy"));
        assert!(body.contains("warning: this is a clippy lint"));
        assert!(!body.contains("`cargo test` output"));
    }

    #[test]
    fn build_log_body_for_self_reported_failure_includes_agent_tail_only() {
        let started = fixed_timestamp();
        let finished = started;
        let body = build_log_body(
            &ExitReason::AgentSelfReportedFailure,
            42,
            started,
            finished,
            "agent/42-x",
            &slice5_log_outcomes(0, "stuck on something", None),
        );
        assert!(body.contains("AgentSelfReportedFailure"));
        assert!(body.contains("### Agent output tail"));
        assert!(body.contains("stuck on something"));
        assert!(!body.contains("### `cargo test` output"));
    }
}
