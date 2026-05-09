use std::io::Write;

use crate::auth::Auth;
use crate::config::{AuthMethod, Config};
use crate::policy::{
    self, CheckResult, ExitReason, FixOutcome, GateOutcome, ImplementOutcome, PhaseOutcomes,
    ReviewOutcome,
};
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

    // Run the cargo checks gate inside a fresh container, but ONLY
    // when the workspace looks like a Rust project (has Cargo.toml at
    // the root). For non-Rust briefs the gate is skipped (GateOutcome
    // default = both checks `None`) and the run is treated as success.
    let post_implement_gate: GateOutcome = if workspace.path().join("Cargo.toml").exists() {
        sandbox::run_cargo_checks(&workspace, log_writer).await?
    } else {
        GateOutcome::default()
    };

    // Slice X1: review/review-fix/end-gate phases land in K2-K4. For
    // now PhaseOutcomes carries implement + post-implement gate; review
    // and the rest stay `None` until those phases ship.
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: agent_run.exit_code,
            stderr_tail: agent_run.stderr_tail.clone(),
        },
        post_implement_gate,
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
        workspace::OpenPrRequest {
            owner: &owner,
            repo: &repo,
            head_branch: &branch_name,
            base_branch: workspace.default_branch(),
            title: &pr_title,
            body: &pr_body,
            draft,
        },
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
        tracker::FinaliseRequest {
            owner: &owner,
            repo: &repo,
            issue_number: claimed.number,
            pr_number: pr.number,
            in_progress_label: &config.runtime_labels.agent_in_progress,
            outcome_label,
            log_body: &log_body,
        },
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
            "## Cargo checks failed after the agent's run\n\n\
             The agent reported done with exit 0 but a post-run cargo check (clippy or test, in either the post-implement or end-of-pipeline gate) failed. See the run-log comment on this PR for the per-phase summary and the failing output."
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
         Agent exit code: {agent_exit}\n\n\
         ## Phase summary\n\n\
         - Implement: exit {agent_exit}\n\
         - Cargo checks (post-implement): {post_gate}\n\
         - Review: {review}\n\
         - Review-fix: {review_fix}\n\
         - Cargo checks (end-pipeline): {end_gate}\n",
        started_rfc = started.to_rfc3339(),
        finished_rfc = finished.to_rfc3339(),
        agent_exit = outcomes.implement.exit_code,
        post_gate = gate_summary_line(&outcomes.post_implement_gate),
        review = review_summary(&outcomes.review),
        review_fix = review_fix_summary(&outcomes.review_fix),
        end_gate = match &outcomes.end_pipeline_gate {
            Some(gate) => gate_summary_line(gate),
            None => "did not run".to_string(),
        },
    );

    if !matches!(reason, ExitReason::Success) {
        body.push_str("\n### Agent output tail\n\n```\n");
        body.push_str(&outcomes.implement.stderr_tail);
        body.push_str("\n```\n");

        emit_failed_gate_outputs(
            &mut body,
            "post-implement gate",
            &outcomes.post_implement_gate,
        );
        if let Some(end_gate) = &outcomes.end_pipeline_gate {
            emit_failed_gate_outputs(&mut body, "end-pipeline gate", end_gate);
        }
    }

    body.push_str("\n</details>");
    body
}

fn gate_summary_line(gate: &GateOutcome) -> String {
    fn part(name: &str, check: &Option<CheckResult>) -> String {
        match check {
            None => format!("{name} did not run"),
            Some(r) if r.exit_code == 0 => format!("{name} PASSED"),
            Some(r) => format!("{name} FAILED (exit {})", r.exit_code),
        }
    }
    format!(
        "{}, {}",
        part("clippy", &gate.cargo_clippy),
        part("tests", &gate.cargo_test),
    )
}

fn review_summary(review: &Option<ReviewOutcome>) -> String {
    match review {
        None => "did not run".to_string(),
        Some(r) if r.exit_code != 0 => format!("crashed (exit {})", r.exit_code),
        Some(r) => match &r.findings_text {
            Some(_) => "findings produced".to_string(),
            None => "no findings".to_string(),
        },
    }
}

fn review_fix_summary(fix: &Option<FixOutcome>) -> String {
    match fix {
        None => "did not run".to_string(),
        Some(f) if f.exit_code != 0 => format!("crashed (exit {})", f.exit_code),
        Some(_) => "exit 0".to_string(),
    }
}

/// Append `### \`cargo X\` output` blocks for any failing checks in this
/// gate. Successful checks produce no block — they're already named in
/// the phase summary at the top, no need to dump empty output text.
fn emit_failed_gate_outputs(body: &mut String, label: &str, gate: &GateOutcome) {
    if let Some(clippy) = &gate.cargo_clippy
        && clippy.exit_code != 0
    {
        body.push_str(&format!(
            "\n### `cargo clippy` output ({label}, exit {})\n\n```\n{}\n```\n",
            clippy.exit_code, clippy.output,
        ));
    }
    if let Some(test) = &gate.cargo_test
        && test.exit_code != 0
    {
        body.push_str(&format!(
            "\n### `cargo test` output ({label}, exit {})\n\n```\n{}\n```\n",
            test.exit_code, test.output,
        ));
    }
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
    fn build_pr_body_for_final_tests_red_mentions_cargo_checks_not_just_tests() {
        // After slice X1 the gate runs both clippy and test, in either the
        // post-implement or end-of-pipeline position — the PR body should
        // reflect that, not pin "test" specifically.
        let body = build_pr_body(&ExitReason::FinalTestsRed, 42, None, None);
        assert!(body.to_lowercase().contains("cargo checks"));
        assert!(body.contains("run-log comment"));
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
        // Header includes the gate label (post-implement) and the exit code.
        assert!(body.contains("`cargo test`"));
        assert!(body.contains("exit 101"));
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
        assert!(body.contains("`cargo clippy`"));
        assert!(body.contains("warning: this is a clippy lint"));
        // No cargo test section was emitted (test exit absent / passing).
        assert!(!body.contains("`cargo test` output"));
    }

    #[test]
    fn build_log_body_for_final_tests_red_includes_both_clippy_and_test_when_both_failed() {
        let started = fixed_timestamp();
        let finished = started;
        let outcomes = PhaseOutcomes {
            implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new() },
            post_implement_gate: GateOutcome {
                cargo_clippy: Some(CheckResult { exit_code: 101, output: "clippy lint here".to_string() }),
                cargo_test: Some(CheckResult { exit_code: 1, output: "test panicked".to_string() }),
            },
            review: None,
            review_fix: None,
            end_pipeline_gate: None,
        };
        let body = build_log_body(
            &ExitReason::FinalTestsRed, 42, started, finished, "agent/42-x", &outcomes,
        );
        assert!(body.contains("`cargo clippy`"));
        assert!(body.contains("clippy lint here"));
        assert!(body.contains("`cargo test`"));
        assert!(body.contains("test panicked"));
        assert!(body.contains("exit 101"));
        assert!(body.contains("exit 1"));
    }

    #[test]
    fn build_log_body_for_final_tests_red_includes_end_pipeline_gate_output() {
        // Post-implement gate clean. Review ran (no findings). End-pipeline
        // gate caught a regression. The log body should include the
        // end-pipeline gate's output, distinguishable from the post-implement
        // gate's (which passed cleanly here).
        let started = fixed_timestamp();
        let finished = started;
        let outcomes = PhaseOutcomes {
            implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new() },
            post_implement_gate: GateOutcome {
                cargo_clippy: Some(CheckResult { exit_code: 0, output: String::new() }),
                cargo_test: Some(CheckResult { exit_code: 0, output: String::new() }),
            },
            review: Some(ReviewOutcome { findings_text: None, exit_code: 0 }),
            review_fix: None,
            end_pipeline_gate: Some(GateOutcome {
                cargo_clippy: Some(CheckResult { exit_code: 0, output: String::new() }),
                cargo_test: Some(CheckResult { exit_code: 1, output: "regression here".to_string() }),
            }),
        };
        let body = build_log_body(
            &ExitReason::FinalTestsRed, 42, started, finished, "agent/42-x", &outcomes,
        );
        // The end-pipeline section is named distinctly from post-implement
        // so a reader knows the failure was after review-fix, not before.
        assert!(body.contains("end-pipeline"));
        assert!(body.contains("regression here"));
    }

    #[test]
    fn build_log_body_includes_per_phase_summary() {
        // For a clean run, the summary names each phase that ran with a
        // human-readable outcome. The brief's example: 'Review: 3 findings,
        // all addressed. Cargo checks: clippy PASSED, tests PASSED.'
        let started = fixed_timestamp();
        let finished = started;
        let outcomes = PhaseOutcomes {
            implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new() },
            post_implement_gate: GateOutcome {
                cargo_clippy: Some(CheckResult { exit_code: 0, output: String::new() }),
                cargo_test: Some(CheckResult { exit_code: 0, output: String::new() }),
            },
            review: Some(ReviewOutcome { findings_text: Some("findings".to_string()), exit_code: 0 }),
            review_fix: Some(FixOutcome { exit_code: 0 }),
            end_pipeline_gate: Some(GateOutcome {
                cargo_clippy: Some(CheckResult { exit_code: 0, output: String::new() }),
                cargo_test: Some(CheckResult { exit_code: 0, output: String::new() }),
            }),
        };
        let body = build_log_body(
            &ExitReason::Success, 42, started, finished, "agent/42-x", &outcomes,
        );
        assert!(body.contains("Review:"));
        assert!(body.contains("Cargo checks (post-implement)"));
        assert!(body.contains("Cargo checks (end-pipeline)"));
    }

    #[test]
    fn build_log_body_attributes_review_phase_crash() {
        // Review run crashed (non-zero exit). Per the halt-on-phase-failure
        // contract the runner halts before review-fix or end-gate, so both
        // are None. The log body should name the crash explicitly so a
        // human reading the comment knows which phase died.
        let started = fixed_timestamp();
        let finished = started;
        let outcomes = PhaseOutcomes {
            implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new() },
            post_implement_gate: GateOutcome {
                cargo_clippy: Some(CheckResult { exit_code: 0, output: String::new() }),
                cargo_test: Some(CheckResult { exit_code: 0, output: String::new() }),
            },
            review: Some(ReviewOutcome { findings_text: None, exit_code: 137 }),
            review_fix: None,
            end_pipeline_gate: None,
        };
        let body = build_log_body(
            &ExitReason::Crash, 42, started, finished, "agent/42-x", &outcomes,
        );
        assert!(body.contains("Review: crashed"));
        assert!(body.contains("137"));
        // Phases that didn't run because of the halt are visibly named.
        assert!(body.contains("Review-fix: did not run"));
        assert!(body.contains("Cargo checks (end-pipeline): did not run"));
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
