use std::io::Write;
use std::time::Duration;

use crate::auth::Auth;
use crate::config::{AuthMethod, Config};
use crate::policy::{
    self, CheckResult, ExitReason, FixOutcome, GateOutcome, ImplementOutcome, PhaseOutcomes,
    ReviewOutcome,
};
use crate::sandbox::{self, SandboxError};
use crate::status::{CurrentRun, StatusContext};
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

impl RunError {
    /// A normalised dedup key for this error. Used by the polling loop's
    /// `OutcomeTransition` tracker so that ~30s ticks repeatedly hitting
    /// the same failure (e.g. `MissingAgentBrief(42)` until an operator
    /// posts the brief) collapse to a single log line per uninterrupted
    /// run of identical errors. The contract:
    ///
    /// - Same variant + same payload → same shape (silenced).
    /// - Same variant + different payload → different shape (fresh line).
    /// - Different variant → different shape (fresh line).
    ///
    /// `MissingAgentBrief(N)` is the only variant with a stable
    /// per-issue payload — exactly the case the brief calls out
    /// (transition from `MissingAgentBrief(42)` to `MissingAgentBrief(43)`
    /// must emit a fresh line). For the network/IO/sandbox variants the
    /// shape key folds in the `Display` string so two genuinely
    /// identical failures dedup while transient-detail changes
    /// (different rate-limit message, different sandbox container id)
    /// surface as a fresh transition. The cost of a few extra fresh
    /// lines on transient-detail churn is worth less to operators than
    /// the cost of silencing a legitimate change.
    pub fn shape(&self) -> String {
        match self {
            RunError::Octocrab(e) => format!("octocrab:{e}"),
            RunError::Workspace(e) => format!("workspace:{e}"),
            RunError::Sandbox(e) => format!("sandbox:{e}"),
            RunError::Io(e) => format!("io:{e}"),
            RunError::InvalidRepoUrl(url) => format!("invalid_repo_url:{url}"),
            RunError::MissingAgentBrief(n) => format!("missing_agent_brief:{n}"),
        }
    }
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
    /// The orchestrator detected mid-run that the operator already
    /// transitioned the issue's label out from under it (slice-10
    /// `bellows kill <N>` path). The PR was still opened (workspace
    /// state at kill time, as a draft) and the log comment still
    /// posted, but the runner skipped the label transition and
    /// surfaces this variant so the polling loop can log "cancelled
    /// by operator" rather than "finalised."
    Cancelled {
        issue_number: u64,
        pr_number: u64,
    },
    /// The pre-claim PR check (#42) found at least one open `agent/*`
    /// PR — that PR may still gate master, so the tick refuses to
    /// claim a new issue. `pr_numbers` is empty if the GitHub list-PRs
    /// call failed (fail-closed: we don't know whether master is
    /// gated, so we behave as if it is). The polling loop turns this
    /// into a transition-only log line and a Blocked status-file
    /// write; the next tick retries.
    Blocked {
        pr_numbers: Vec<u64>,
    },
}

/// Outcome of `tracker::list_open_agent_prs` as the pre-claim check sees
/// it. Three variants because the polling loop has to distinguish the
/// API-failure case (we don't know which PRs are open) from the
/// genuinely-blocked case (we have a list) — both fail-close to
/// `RunOutcome::Blocked`, but the status file's rendering differs.
enum PreClaim {
    Clear,
    Blocked(Vec<u64>),
    BlockedUnknown,
}

pub async fn run_once(
    client: &octocrab::Octocrab,
    config: &Config,
    log_writer: &mut dyn Write,
    status_ctx: Option<&StatusContext>,
) -> Result<RunOutcome, RunError> {
    let (owner, repo) = parse_owner_repo(&config.repo.url)?;

    // Issue #42: pre-claim PR check. Before any other API call, ask
    // GitHub which open PRs exist on this repo and filter to bellows-
    // authored `agent/*` branches. If any are open, the prior agent's
    // PR may still gate master (CI running, in review, or stuck-draft)
    // and claiming a new issue now would run the next agent against a
    // master that hasn't absorbed the prior PR — the concrete race PR
    // #41 was designed to fix and which #42 was filed for after PR #41
    // hit it on itself.
    //
    // Fail-closed: any error talking to GitHub maps to Blocked with an
    // empty pr_numbers list. We can't tell whether master is gated; the
    // safe answer matches the answer we'd give if we knew it was. The
    // next polling tick is the retry — no inner backoff, no retry loop.
    let preclaim = match tracker::list_open_agent_prs(client, &owner, &repo).await {
        Ok(prs) if prs.is_empty() => PreClaim::Clear,
        Ok(prs) => PreClaim::Blocked(prs),
        Err(e) => {
            let _ = writeln!(
                log_writer,
                "bellows: pre-claim PR check failed; failing closed (treating tick as blocked): {}",
                e,
            );
            PreClaim::BlockedUnknown
        }
    };
    match preclaim {
        PreClaim::Blocked(pr_numbers) => {
            if let Some(ctx) = status_ctx
                && let Err(e) = ctx.write_blocked(&pr_numbers).await
            {
                let _ = writeln!(
                    log_writer,
                    "bellows: could not write blocked status (continuing): {}",
                    e,
                );
            }
            return Ok(RunOutcome::Blocked { pr_numbers });
        }
        PreClaim::BlockedUnknown => {
            if let Some(ctx) = status_ctx
                && let Err(e) = ctx.write_blocked(&[]).await
            {
                let _ = writeln!(
                    log_writer,
                    "bellows: could not write blocked status (continuing): {}",
                    e,
                );
            }
            return Ok(RunOutcome::Blocked {
                pr_numbers: Vec::new(),
            });
        }
        PreClaim::Clear => {
            // The pre-claim check cleared. If a prior tick left the
            // status file in a Blocked state, that state is stale —
            // write idle so `bellows status` no longer lies. Also
            // covers the first claim after a merge: the status was
            // Blocked at the last tick, now we're idle until we
            // either claim (write_busy) or short-circuit.
            if let Some(ctx) = status_ctx
                && let Err(e) = ctx.write_idle().await
            {
                let _ = writeln!(
                    log_writer,
                    "bellows: could not clear blocked status (continuing): {}",
                    e,
                );
            }
        }
    }

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

    // Slice 8: record whether the weak-test-guard skip-label is on the
    // issue at claim time. The post-implement guard reads this flag; we
    // snapshot here so a label flip mid-run cannot accidentally bypass
    // or trigger the guard.
    let weak_test_guard_skipped = claimed
        .labels
        .iter()
        .any(|l| l.name == config.agent.weak_test_guard_skip_label);

    let started = chrono::Utc::now();
    let branch_name = crate::agent_branch_name(claimed.number, &claimed.title);

    announce(
        log_writer,
        &format!(
            "bellows: claimed issue #{} (\"{}\") — pipeline starting on branch `{}`",
            claimed.number, claimed.title, branch_name,
        ),
    );

    // Slice 9: announce that we've claimed an issue. Best-effort —
    // a status-write failure is logged but does not abort the run.
    if let Some(ctx) = status_ctx {
        let current = CurrentRun {
            issue_number: claimed.number,
            issue_title: claimed.title.clone(),
            repo: format!("{}/{}", owner, repo),
            claimed_at: started,
        };
        if let Err(e) = ctx.write_busy(current).await {
            let _ = writeln!(
                log_writer,
                "bellows: could not write busy status (continuing): {}",
                e,
            );
        }
    }

    let workspace = workspace::prepare(&config.repo.url, &branch_name).await?;

    let repo_slug = crate::repo_slug(&config.repo.url);

    let kickoff = policy::render_kickoff(&brief, &config.repo.url, &branch_name);
    tokio::fs::write(workspace.path().join(".bellows-kickoff.md"), &kickoff).await?;

    let auth = match config.auth.method {
        AuthMethod::Subscription => Auth::Subscription {
            credentials_volume_name: config.auth.credentials_volume.clone(),
        },
    };

    // Per-issue wall-clock budget. Threaded through every container call
    // below; `mark_killed_if` flips `exceeded` whenever a sandbox run
    // reports the deadline fired.
    let mut budget = WallClockBudget::new(Duration::from_secs(
        config.agent.wall_clock_minutes.get() * 60,
    ));

    // ---- Phase 1: Implement ----
    announce(
        log_writer,
        "bellows: phase 1/5 — implement (running claude in sandbox container, this is the long one)",
    );
    // Issue #52 asymmetry audit: capture HEAD before the implement
    // agent invocation for the same reason the per-finding and nit-batch
    // sites do. The agent brief discourages self-committing inside the
    // sandbox but does not prevent it; if the implement agent self-
    // commits and leaves nothing else staged, `commit_all` returns
    // `NoChangesToCommit` and the legacy
    // `commit_all().await?; push_branch().await?;` shape aborts the run
    // outright — strictly worse than the silent-drop case, because the
    // self-committed work lives on local HEAD but never reaches origin
    // and the pipeline dies before producing a PR. The shared
    // `commit_all_and_push_if_advanced` helper collapses the four-corner
    // pattern (agent self-commit / bellows-on-behalf / mixed / no-op)
    // so this site is tolerant of every commit shape the implement
    // agent can leave behind.
    let head_before_implement = workspace::head_sha(&workspace).await?;
    let implement_agent_run = sandbox::run_agent(
        &workspace,
        &auth,
        claimed.number,
        &repo_slug,
        log_writer,
        budget.deadline_or_halt(),
    )
    .await?;
    budget.mark_killed_if(implement_agent_run.killed_by_deadline);
    announce(
        log_writer,
        &format!(
            "bellows: implement done (exit {}{})",
            implement_agent_run.exit_code,
            if implement_agent_run.killed_by_deadline {
                ", killed by wall-clock"
            } else {
                ""
            },
        ),
    );

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

    announce(log_writer, "bellows: committing + pushing implement branch");
    let head_after_implement =
        workspace::commit_all_and_push_if_advanced(&workspace, &head_before_implement).await?;

    // Issue #49: implement-crash recovery. When the implement-phase
    // agent exited non-zero AND produced no commits, the legacy path
    // produced no branch on origin — `open_pr` later either fails or
    // opens a no-content PR, and the source issue silently stays at
    // `agent-in-progress` with no `agent-failed` label, no draft PR,
    // and no log comment. Witnessed live on issue #42 (CRLF shebang
    // in policy-image/entrypoint-user).
    //
    // The fix: synthesise a bellows-authored `agent-notes.md` entry
    // capturing the implement-phase exit code + a bounded prefix of
    // its stderr/stdout tail. That gives the run a single commit on
    // the agent branch (so the push succeeds) AND something readable
    // for the operator in the resulting draft PR's diff. The synth
    // also flips `implement_crash_synthesised` in PhaseOutcomes so
    // `classify_exit` knows the agent-notes content is bellows-
    // authored (suppress the usual `has_agent_notes →
    // AgentSelfReportedFailure` precedence — the run did not self-
    // report, it crashed).
    //
    // Gated tightly:
    //   - `implement_agent_run.exit_code != 0` — the new path only
    //     activates on a true crash. A clean exit with no commits
    //     (agent decided the brief was unnecessary or wrote
    //     agent-notes.md instead of code) still routes through the
    //     existing `agent-notes.md` precedence, per the brief.
    //   - `head_after_implement == head_before_implement` — and the
    //     workspace must genuinely be at base. If the agent self-
    //     committed before crashing, HEAD has advanced; the existing
    //     halt-with-partial-progress path handles that and must not
    //     be regressed.
    let implement_crash_synthesised = if implement_agent_run.exit_code != 0
        && head_after_implement == head_before_implement
    {
        announce(
            log_writer,
            "bellows: implement crashed with no commits — synthesising agent-notes entry so the run produces a draft PR + agent-failed label rather than silently stalling at agent-in-progress",
        );
        let notes_path = workspace.path().join("agent-notes.md");
        let existing = if notes_path.exists() {
            tokio::fs::read_to_string(&notes_path).await?
        } else {
            String::new()
        };
        let mut new_notes = existing;
        if !new_notes.is_empty() && !new_notes.ends_with('\n') {
            new_notes.push('\n');
        }
        new_notes.push_str(&policy::synthesize_implement_crash_entry(
            implement_agent_run.exit_code,
            &implement_agent_run.stderr_tail,
        ));
        tokio::fs::write(&notes_path, new_notes).await?;
        // Bellows-write-then-bellows-commit with no intervening agent
        // invocation, so HEAD cannot have advanced under an agent
        // commit message between the write and the commit. The legacy
        // match-on-result shape (mirrors the weak-test guard and
        // parser-as-backstop synth sites) is correct here.
        match workspace::commit_all(&workspace).await {
            Ok(()) => workspace::push_branch(&workspace).await?,
            Err(WorkspaceError::NoChangesToCommit) => {}
            Err(e) => return Err(e.into()),
        }
        true
    } else {
        false
    };

    // ---- Phase 2: Post-implement cargo checks gate ----
    let post_implement_gate: GateOutcome = if !budget.exceeded
        && workspace.path().join("Cargo.toml").exists()
    {
        announce(
            log_writer,
            "bellows: phase 2/5 — cargo checks gate (clippy + test, fresh container)",
        );
        let run = sandbox::run_cargo_checks(
            &workspace,
            claimed.number,
            &repo_slug,
            log_writer,
            budget.deadline_or_halt(),
        )
        .await?;
        budget.mark_killed_if(run.killed_by_deadline);
        run.gate
    } else {
        GateOutcome::default()
    };

    // Slice 8: weak-test guard. After the implement-phase commit lands
    // and the cargo gate has run, scan `git diff <base>...HEAD` for new
    // Rust test attributes. A run with implementation code but no new
    // tests trips a green cargo gate (passing tests over an unchanged
    // test suite) but is otherwise indistinguishable from a real
    // Success — falling through to a non-draft PR a reviewer might
    // merge. The guard catches this by synthesising an `## Unaddressed
    // finding: no new tests added` section in agent-notes.md, which
    // routes the run to AgentSelfReportedFailure via the existing
    // slice-9.6 has_agent_notes precedence in classify_exit.
    //
    // Short-circuited when the issue carries the configurable skip-
    // label (default `refactor`): renames / dependency bumps / pure
    // refactors legitimately produce no new tests, so the cargo gate
    // alone is the right contract for those briefs.
    //
    // Gated on a clean implement + post-implement-gate path: running
    // the guard on a crashed implement or failing cargo gate would
    // misattribute the failure mode (the run was already going to
    // route to Crash / FinalTestsRed before notes-precedence took
    // over). Skipping here keeps the operator-facing classification
    // accurate.
    if !weak_test_guard_skipped
        && implement_agent_run.exit_code == 0
        && !policy::gate_failed(&post_implement_gate)
        && !budget.exceeded
    {
        let diff = workspace::compute_diff_against_base(&workspace).await?;
        if !policy::has_new_tests(&diff) {
            announce(
                log_writer,
                "bellows: weak-test guard fired — diff against base has no new Rust test attributes; synthesising agent-notes entry to force agent-self-reported-failure",
            );
            let notes_path = workspace.path().join("agent-notes.md");
            let existing = if notes_path.exists() {
                tokio::fs::read_to_string(&notes_path).await?
            } else {
                String::new()
            };
            let mut new_notes = existing;
            if !new_notes.is_empty() && !new_notes.ends_with('\n') {
                new_notes.push('\n');
            }
            new_notes.push_str(&policy::synthesize_no_new_tests_entry());
            tokio::fs::write(&notes_path, new_notes).await?;
            // Issue #52 asymmetry audit: this site looks like the
            // nit-batch shape but does NOT need
            // `commit_all_and_push_if_advanced`. There is no agent
            // invocation between the bellows-side `tokio::fs::write`
            // above and this `commit_all` — bellows is the only writer
            // and the only committer, so HEAD cannot have advanced
            // under a different commit message. `commit_all` returns
            // `Ok(())` whenever the synth write produced staged
            // content (the bytes are bellows-authored and always
            // non-empty), and `NoChangesToCommit` only when the synth
            // bytes were already present byte-for-byte from a prior
            // run — neither case loses a push.
            match workspace::commit_all(&workspace).await {
                Ok(()) => workspace::push_branch(&workspace).await?,
                Err(WorkspaceError::NoChangesToCommit) => {}
                Err(e) => return Err(e.into()),
            }
        }
    } else if weak_test_guard_skipped {
        announce(
            log_writer,
            &format!(
                "bellows: weak-test guard short-circuited — issue carries the `{}` skip-label",
                config.agent.weak_test_guard_skip_label,
            ),
        );
    }

    // Halt-on-phase-failure: if implement crashed, the post-implement
    // gate failed, OR the wall-clock budget is already spent, skip
    // review/review-fix/end-gate and short-circuit. agent-notes from
    // the implement phase will be picked up in the final read just
    // before classify_exit.
    let halt_after_post_implement = implement_agent_run.exit_code != 0
        || policy::gate_failed(&post_implement_gate)
        || budget.exceeded;

    let mut review_outcome: Option<ReviewOutcome> = None;
    let mut review_fix_outcome: Option<FixOutcome> = None;
    let mut end_pipeline_gate: Option<GateOutcome> = None;
    // Slice 9.6: parser-as-backstop violations. Populated after the
    // per-finding/nit-batch review-fix invocations complete and the
    // parser cross-references findings against agent-notes sections.
    // Non-empty values force the run to AgentSelfReportedFailure via
    // a synthetic agent-notes entry.
    let mut backstop_violations: Vec<policy::ParsedFinding> = Vec::new();

    if !halt_after_post_implement {
        // ---- Phase 3: Review ----
        announce(
            log_writer,
            "bellows: phase 3/5 — review (claude reads diff, produces findings)",
        );
        workspace::generate_diff(&workspace, policy::REVIEW_DIFF_FILE).await?;
        workspace::generate_commit_log(&workspace, policy::REVIEW_COMMIT_LOG_FILE).await?;
        tokio::fs::write(
            workspace.path().join(".bellows-kickoff.md"),
            policy::REVIEW_PROMPT,
        )
        .await?;
        let review_agent_run = sandbox::run_agent(
            &workspace,
            &auth,
            claimed.number,
            &repo_slug,
            log_writer,
            budget.deadline_or_halt(),
        )
        .await?;
        budget.mark_killed_if(review_agent_run.killed_by_deadline);

        // Read the findings file. Don't remove it yet — review-fix may
        // need to read it. If review-fix runs successfully it removes
        // the file itself. Bellows removes any leftover before the next
        // commit_all so the file never lands in the PR diff.
        let findings_path = workspace.path().join(policy::REVIEW_FINDINGS_FILE);
        let findings_text = if findings_path.exists() {
            let raw = tokio::fs::read_to_string(&findings_path).await?;
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("(no findings)") {
                None
            } else {
                Some(trimmed.to_string())
            }
        } else {
            None
        };
        let has_findings = findings_text.is_some();
        review_outcome = Some(ReviewOutcome {
            findings_text,
            exit_code: review_agent_run.exit_code,
        });

        let halt_after_review = review_agent_run.exit_code != 0;

        announce(
            log_writer,
            &format!(
                "bellows: review done (findings: {})",
                if has_findings { "yes" } else { "none" },
            ),
        );

        let mut halt_after_fix = false;
        if !halt_after_review && has_findings && !budget.exceeded {
            // ---- Phase 4: Review-fix (slice 9.6 per-finding shape) ----
            // The slice-9.5 prompt-level tightening failed empirically:
            // 4 consecutive bellows-on-bellows PRs (#26, #28, #30, #33)
            // silently skipped `important` findings even with imperative
            // MUST language. Slice 9.6 closes the loop with TWO
            // interlocking mechanisms:
            //
            //   (1) Per-finding scoping: one claude -p invocation per
            //       blocker/important finding. The agent never sees a
            //       list of findings; there is nothing to "decide is
            //       skippable" because the prompt presents a single
            //       finding and exactly two options.
            //   (2) Parser-as-backstop: after all invocations complete,
            //       bellows independently verifies that every blocker/
            //       important finding has EITHER a commit OR a matching
            //       `## Unaddressed finding: <title>` section. Coverage
            //       gaps force AgentSelfReportedFailure via a synthetic
            //       agent-notes entry.
            //
            // Nits batch through a separate permissive invocation.
            let findings_for_fix = review_outcome
                .as_ref()
                .and_then(|r| r.findings_text.as_deref())
                .map(policy::parse_findings)
                .unwrap_or_default();
            if !findings_for_fix.malformed_titles.is_empty() {
                announce(
                    log_writer,
                    &format!(
                        "bellows: review produced {} malformed finding title(s) (off-vocabulary severity or no ` — <tag>` suffix); they will not flow into review-fix:",
                        findings_for_fix.malformed_titles.len(),
                    ),
                );
                for raw in &findings_for_fix.malformed_titles {
                    announce(log_writer, &format!("  malformed: {}", raw));
                }
            }

            let (urgent_findings, nit_findings): (
                Vec<policy::ParsedFinding>,
                Vec<policy::ParsedFinding>,
            ) = findings_for_fix.findings.into_iter().partition(|f| {
                matches!(
                    f.severity,
                    policy::Severity::Blocker | policy::Severity::Important
                )
            });

            announce(
                log_writer,
                &format!(
                    "bellows: phase 4/5 — review-fix ({} blocker/important per-finding invocation(s) + {} batched nit(s))",
                    urgent_findings.len(),
                    nit_findings.len(),
                ),
            );

            // Per-finding loop: one container per blocker/important
            // finding. Each invocation respects the remaining wall-
            // clock budget; one slow finding cannot blow the cap
            // without halting subsequent ones.
            let mut coverage: Vec<policy::FindingCoverage> = Vec::new();
            let mut review_fix_exit: i64 = 0;
            for (idx, finding) in urgent_findings.iter().enumerate() {
                if budget.exceeded {
                    announce(
                        log_writer,
                        &format!(
                            "bellows: wall-clock budget spent; skipping per-finding invocation {}/{} (\"{}\")",
                            idx + 1,
                            urgent_findings.len(),
                            finding.title,
                        ),
                    );
                    // The remaining findings have no commit AND no
                    // chance for the agent to write an Unaddressed
                    // finding section. Record them as coverage entries
                    // so the parser-as-backstop fires on them too.
                    coverage.push(policy::FindingCoverage {
                        finding: finding.clone(),
                        commit_landed: false,
                    });
                    continue;
                }

                announce(
                    log_writer,
                    &format!(
                        "bellows: per-finding invocation {}/{} — {} ({})",
                        idx + 1,
                        urgent_findings.len(),
                        finding.title,
                        finding.severity.as_tag(),
                    ),
                );

                let kickoff = policy::per_finding_kickoff(
                    finding,
                    policy::REVIEW_DIFF_FILE,
                    "agent-notes.md",
                );
                tokio::fs::write(workspace.path().join(".bellows-kickoff.md"), &kickoff)
                    .await?;
                // Capture HEAD BEFORE the agent runs so we can detect
                // whether the agent self-committed during the
                // invocation. See the four-corners comment below the
                // run for the full contract.
                let head_before = workspace::head_sha(&workspace).await?;
                let per_finding_run = sandbox::run_agent(
                    &workspace,
                    &auth,
                    claimed.number,
                    &repo_slug,
                    log_writer,
                    budget.deadline_or_halt(),
                )
                .await?;
                budget.mark_killed_if(per_finding_run.killed_by_deadline);
                if review_fix_exit == 0 && per_finding_run.exit_code != 0 {
                    review_fix_exit = per_finding_run.exit_code;
                }

                // Did a CODE commit land for this finding? The four
                // corners we must classify:
                //
                //   1. Agent self-commits the fix (PR #38 case). HEAD
                //      advances during the invocation under whatever
                //      commit message the agent chose; bellows's
                //      subsequent commit_all sees nothing to stage and
                //      returns NoChangesToCommit. commit_landed=true
                //      iff the diff between head_before..head_after
                //      touched any file other than agent-notes.md.
                //   2. Bellows commits on the agent's behalf (existing
                //      case). Agent left uncommitted edits; commit_all
                //      produces the boilerplate "Bellows agent run"
                //      commit and HEAD advances. Same diff-based check
                //      as case 1 — agent-notes-only commits still mean
                //      commit_landed=false.
                //   3. Notes-only edit (any author). The agent only
                //      wrote an Unaddressed-finding section; HEAD may
                //      advance via bellows's commit but the diff is
                //      exactly [agent-notes.md]. commit_landed=false
                //      so the verbatim-title fallback in
                //      compute_coverage_violations runs.
                //   4. No commit at all. HEAD did not advance (agent
                //      did nothing and left no uncommitted edits).
                //      commit_landed=false.
                //
                // commit_all_and_push_if_advanced collapses corners 1
                // and 2 into a single call: it treats NoChangesToCommit
                // as a normal post-agent outcome and gates the push on
                // HEAD movement rather than commit_all's return. Issue
                // #52 introduced the helper so the nit-batch site
                // shares the same shape.
                let head_after =
                    workspace::commit_all_and_push_if_advanced(&workspace, &head_before).await?;
                let commit_landed = head_after != head_before
                    && !workspace::diff_between_touches_only_agent_notes(
                        &workspace,
                        &head_before,
                        &head_after,
                    )
                    .await?;
                coverage.push(policy::FindingCoverage {
                    finding: finding.clone(),
                    commit_landed,
                });
            }

            // Nit batch: single permissive invocation, silent skip
            // allowed. Skipped entirely when there are no nits or the
            // budget is spent.
            //
            // Issue #52: the nit-batch invocation is just as exposed
            // to the agent-self-commit shape as the per-finding loop —
            // if the agent self-commits a nit fix inside the sandbox,
            // bellows's subsequent commit_all sees nothing to stage and
            // returns NoChangesToCommit. The legacy
            // `Ok(()) => push, NoChangesToCommit => {}` shape silently
            // dropped that commit (HEAD on the local branch had
            // advanced, but no push happened, so origin lagged). The
            // end-pipeline cargo-checks gate would then run against a
            // workspace whose HEAD had diverged from the pushed branch,
            // producing false-positive FinalTestsRed classifications
            // (witnessed live on PR #51). We capture head_before and
            // use the shared `commit_all_and_push_if_advanced` helper
            // so the per-finding and nit-batch sites are identically
            // tolerant of either commit shape. Diff-based
            // classification (commit_landed) is not needed here — the
            // nit batch does not contribute to address-or-explain
            // coverage — so we discard `head_after`.
            if !nit_findings.is_empty() && !budget.exceeded {
                announce(
                    log_writer,
                    &format!(
                        "bellows: nit batch — {} nit(s) in one invocation",
                        nit_findings.len(),
                    ),
                );
                let mut nit_kickoff = String::from("## Nit findings to consider\n\n");
                for nit in &nit_findings {
                    nit_kickoff.push_str(&format!(
                        "### {title} — nit\n\n{body}\n\n",
                        title = nit.title,
                        body = nit.body,
                    ));
                }
                nit_kickoff.push_str("\n---\n\n");
                nit_kickoff.push_str(policy::BATCH_REVIEW_FIX_NIT_PROMPT);
                tokio::fs::write(workspace.path().join(".bellows-kickoff.md"), &nit_kickoff)
                    .await?;
                let head_before = workspace::head_sha(&workspace).await?;
                let nit_batch_run = sandbox::run_agent(
                    &workspace,
                    &auth,
                    claimed.number,
                    &repo_slug,
                    log_writer,
                    budget.deadline_or_halt(),
                )
                .await?;
                budget.mark_killed_if(nit_batch_run.killed_by_deadline);
                if review_fix_exit == 0 && nit_batch_run.exit_code != 0 {
                    review_fix_exit = nit_batch_run.exit_code;
                }
                let _ = workspace::commit_all_and_push_if_advanced(&workspace, &head_before)
                    .await?;
            }

            review_fix_outcome = Some(FixOutcome {
                exit_code: review_fix_exit,
            });
            halt_after_fix = review_fix_exit != 0;

            // Parser-as-backstop: independently parse agent-notes.md
            // and cross-reference with the urgent-finding coverage we
            // tracked above. Findings with neither a commit nor a
            // verbatim-title section are violations; we synthesise
            // entries for them so the existing has_agent_notes →
            // AgentSelfReportedFailure precedence in classify_exit
            // fires.
            let notes_path = workspace.path().join("agent-notes.md");
            let notes_text = if notes_path.exists() {
                tokio::fs::read_to_string(&notes_path).await?
            } else {
                String::new()
            };
            let sections = policy::parse_agent_notes_sections(&notes_text);
            let violations = policy::compute_coverage_violations(&coverage, &sections);
            if !violations.is_empty() {
                announce(
                    log_writer,
                    &format!(
                        "bellows: parser-as-backstop detected {} address-or-explain violation(s); synthesising agent-notes entries to force agent-self-reported-failure",
                        violations.len(),
                    ),
                );
                let synth = policy::synthesize_unaddressed_entries(&violations);
                let mut new_notes = notes_text.clone();
                if !new_notes.ends_with('\n') && !new_notes.is_empty() {
                    new_notes.push('\n');
                }
                new_notes.push_str(&synth);
                tokio::fs::write(&notes_path, new_notes).await?;
                // Issue #52 asymmetry audit: like the weak-test-guard
                // synth above, this site is bellows-write-then-bellows-
                // commit with no intervening agent invocation, so HEAD
                // cannot move under an agent commit message between
                // the write and `commit_all`. `commit_all_and_push_if_advanced`
                // is not needed here — the legacy match-on-result
                // shape is correct because the only way to reach this
                // branch is via a bellows-side write that genuinely
                // produced staged content.
                match workspace::commit_all(&workspace).await {
                    Ok(()) => workspace::push_branch(&workspace).await?,
                    Err(WorkspaceError::NoChangesToCommit) => {}
                    Err(e) => return Err(e.into()),
                }
                backstop_violations = violations;
            }
        }

        // ---- Phase 5: End-of-pipeline cargo checks gate ----
        if !halt_after_review
            && !halt_after_fix
            && !budget.exceeded
            && workspace.path().join("Cargo.toml").exists()
        {
            announce(
                log_writer,
                "bellows: phase 5/5 — end-of-pipeline cargo checks gate (clippy + test after fixups)",
            );
            let run = sandbox::run_cargo_checks(
                &workspace,
                claimed.number,
                &repo_slug,
                log_writer,
                budget.deadline_or_halt(),
            )
            .await?;
            budget.mark_killed_if(run.killed_by_deadline);
            end_pipeline_gate = Some(run.gate);
        }

        // Defensive cleanup: even on halt paths the diff file should not
        // outlive the run. Findings file is also ensured-removed so it
        // never appears in any subsequent commit.
        cleanup_phase_handoff_files(&workspace).await?;
    }

    // Read agent-notes.md ONCE at the very end. Any phase may have
    // written/appended to it (implement, review, review-fix). Notes
    // stay in the workspace and the diff so the human reviewer sees
    // them — not removed.
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

    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: implement_agent_run.exit_code,
            stderr_tail: implement_agent_run.stderr_tail.clone(),
        },
        post_implement_gate,
        review: review_outcome,
        review_fix: review_fix_outcome,
        end_pipeline_gate,
        wall_clock_exceeded: budget.exceeded,
        backstop_violations,
        implement_crash_synthesised,
    };
    let pipeline_reason = policy::classify_exit(agent_notes.is_some(), &outcomes);

    // PR #33 review finding #2: detect external cancellation BEFORE
    // opening the PR. Without this check, a kill that fires in the
    // gap between phases (where no container is alive for SIGKILL to
    // touch) and a pipeline that happens to complete its remaining
    // phases successfully would land here with reason=Success →
    // draft=false → open_pr(draft=false) → ready-for-review PR with
    // a "Success" log body. The cancellation would only be noticed
    // in finalise's GET, too late to fix the PR-open semantics. A
    // reviewer scanning the PR list could plausibly merge the
    // cancelled run.
    //
    // Lightweight GET on the issue's labels; best-effort. If the
    // network is flaky and we can't tell, fall back to the pipeline-
    // computed reason — finalise's own check is the safety net.
    let externally_cancelled = match tracker::issue_in_progress(
        client,
        &owner,
        &repo,
        claimed.number,
        &config.runtime_labels.agent_in_progress,
    )
    .await
    {
        Ok(in_progress) => !in_progress,
        Err(e) => {
            let _ = writeln!(
                log_writer,
                "bellows: pre-PR cancellation check failed (continuing): {}",
                e,
            );
            false
        }
    };

    let reason = if externally_cancelled {
        ExitReason::Cancelled
    } else {
        pipeline_reason
    };

    // Cancelled runs always draft + always use agent_cancelled. Other
    // non-Success reasons stay draft per the slice-5 contract.
    let draft = !matches!(reason, ExitReason::Success);
    // Exhaustive match — no `_ => ...` fallthrough — so when later
    // slices add new variants the compiler refuses to build until we
    // make an explicit decision per variant.
    let outcome_label = match reason {
        ExitReason::Success => &config.runtime_labels.agent_done,
        ExitReason::AgentSelfReportedFailure
        | ExitReason::Crash
        | ExitReason::FinalTestsRed
        | ExitReason::WallClockExceeded => &config.runtime_labels.agent_failed,
        ExitReason::RateLimited => &config.runtime_labels.agent_rate_limited,
        ExitReason::Cancelled => &config.runtime_labels.agent_cancelled,
    };

    announce(
        log_writer,
        &format!(
            "bellows: classified as {:?} — opening {} PR",
            reason,
            if draft { "draft" } else { "ready-for-review" },
        ),
    );

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
    announce(
        log_writer,
        &format!("bellows: opened PR #{} — finalising labels + log comment", pr.number),
    );

    // Post the review findings as a separate `## Review findings` PR
    // comment if the review phase produced any. Posted regardless of
    // whether review-fix succeeded — readers always see what was
    // flagged. Reads from the `outcomes` PhaseOutcomes since
    // review_outcome was moved into that struct above.
    if let Some(findings) = outcomes
        .review
        .as_ref()
        .and_then(|r| r.findings_text.as_deref())
    {
        let comment_body = format!("## Review findings\n\n{findings}");
        tracker::post_pr_comment(client, &owner, &repo, pr.number, &comment_body).await?;
    }

    let finished = chrono::Utc::now();
    let log_body = build_log_body(
        &reason,
        claimed.number,
        started,
        finished,
        &branch_name,
        &outcomes,
    );

    let finalise_outcome = tracker::finalise(
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

    // Slice 9: announce we're back to idle. Best-effort —
    // halt paths still reach finalise above, so a single
    // write here covers all exit-reason variants.
    if let Some(ctx) = status_ctx
        && let Err(e) = ctx.write_idle().await
    {
        let _ = writeln!(
            log_writer,
            "bellows: could not write idle status after finalise (continuing): {}",
            e,
        );
    }

    // Either signal can route to Cancelled:
    //   - Pre-PR check (PR #33 review finding #2 fix): we detected the
    //     cancellation before opening the PR, set reason=Cancelled,
    //     opened draft, used the Cancelled log header.
    //   - Finalise check (slice-10 safety net): the cancellation
    //     happened in the narrow window between our pre-PR check and
    //     finalise's GET. PR was opened with whatever the pipeline
    //     reason was; finalise then skipped the label PATCH.
    // Both routes return RunOutcome::Cancelled so the polling loop
    // logs the right line.
    if matches!(reason, ExitReason::Cancelled) || finalise_outcome.externally_cancelled {
        Ok(RunOutcome::Cancelled {
            issue_number: claimed.number,
            pr_number: pr.number,
        })
    } else {
        Ok(RunOutcome::Finalised {
            issue_number: claimed.number,
            pr_number: pr.number,
            reason,
        })
    }
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
        ExitReason::WallClockExceeded => {
            "## Wall-clock cap reached\n\n\
             The pipeline exceeded the configured per-issue wall-clock budget and was halted. See the run-log comment on this PR for elapsed minutes and a per-phase breakdown of where the time went."
                .to_string()
        }
        ExitReason::RateLimited => {
            "## Anthropic API rate limit detected\n\n\
             A claude phase exited non-zero with stderr matching a known rate-limit signature. The PR is left open for re-run once the rate-limit window clears. See the run-log comment for the matched signature."
                .to_string()
        }
        ExitReason::Cancelled => {
            "## Cancelled by operator\n\n\
             `bellows kill` was invoked against this issue mid-run. Whatever workspace state the agent had produced before cancellation is committed in this PR's diff; the run-log comment captures the per-phase summary at cancellation time. Review the partial work and either salvage it as a starting point or drop the PR."
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

    // Per-reason callout block, before the agent output tail. Surfaces
    // the operator-relevant headline for non-generic failures so the
    // log comment communicates "what kind of failure this was" without
    // having to scan the per-phase summary.
    match reason {
        ExitReason::WallClockExceeded => {
            let elapsed_minutes = (finished - started).num_minutes();
            body.push_str(&format!(
                "\n### Wall-clock cap reached after {elapsed_minutes} minutes\n\n\
                 The pipeline was halted because the per-issue wall-clock budget was \
                 exceeded. The phase summary above shows where the time went.\n",
            ));
        }
        ExitReason::RateLimited => {
            body.push_str(
                "\n### Anthropic API rate limit detected\n\n\
                 A claude phase exited non-zero with stderr matching a known rate-limit \
                 signature. The agent output tail below contains the matched line. \
                 Re-run the issue once the rate-limit window clears.\n",
            );
        }
        ExitReason::Cancelled => {
            body.push_str(
                "\n### Cancelled by operator\n\n\
                 `bellows kill` was invoked against this issue mid-run. The phase \
                 summary above reflects whichever phases completed before the \
                 cancellation was detected; subsequent phases were short-circuited. \
                 The PR's diff captures whatever workspace state was committed.\n",
            );
        }
        _ => {}
    }

    // Auth-error pointer: signature-driven, not ExitReason-driven, per
    // the slice-X1 routing-focused-enum principle. The run still
    // classifies as Crash (or another existing variant) — this callout
    // just tells the operator that the underlying cause is an expired
    // OAuth session and points them at `bellows refresh-auth`. Gated on
    // a non-zero implement exit so a clean run that happens to mention
    // "refresh_token_expired" in committed docs doesn't get the
    // callout.
    if outcomes.implement.exit_code != 0
        && policy::is_auth_error_signature(&outcomes.implement.stderr_tail)
    {
        body.push_str(
            "\n### Authentication error detected in agent stderr\n\n\
             A claude phase exited non-zero with stderr matching a known auth-error \
             signature (e.g. an expired OAuth refresh token). Run `bellows refresh-auth` \
             to re-authenticate, then re-label the issue to retry. The agent output tail \
             below contains the matched line.\n",
        );
    }

    // Slice 9.6 parser-as-backstop: when the per-finding agent silently
    // skipped blocker/important findings, bellows synthesised
    // agent-notes entries to force agent-self-reported-failure. Surface
    // the violation list explicitly here so a reader of the PR comment
    // sees which findings the agent dropped, rather than having to
    // diff agent-notes.md to figure out why the run was forced into
    // failure.
    if !outcomes.backstop_violations.is_empty() {
        body.push_str(&policy::build_violation_callout(&outcomes.backstop_violations));
    }

    if !matches!(reason, ExitReason::Success) {
        // When SIGKILL fires before any agent output flushes (typical for
        // wall-clock kill mid-startup), `stderr_tail` is empty; emitting
        // the section anyway produces an empty markdown code fence in the
        // PR comment. Surface a placeholder instead so the operator sees
        // why the section is empty.
        if outcomes.implement.stderr_tail.trim().is_empty() {
            body.push_str("\n_(No agent output was captured before termination.)_\n");
        } else {
            body.push_str("\n### Agent output tail\n\n```\n");
            body.push_str(&outcomes.implement.stderr_tail);
            body.push_str("\n```\n");
        }

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

/// Write a phase-boundary announcement to BOTH stdout AND the log file.
/// The runner's `log_writer` parameter is the same File handle main.rs
/// uses for the log file; we additionally print to stdout so an operator
/// running `bellows run` interactively sees what phase is in flight
/// without having to tail the log file.
///
/// Per-line write errors are swallowed (consistent with the rest of the
/// codebase's log-writing policy: log lines are diagnostic, not
/// load-bearing, and a failed write shouldn't halt the pipeline).
fn announce(log_writer: &mut dyn Write, line: &str) {
    println!("{}", line);
    let _ = writeln!(log_writer, "{}", line);
}

/// Tracks the per-issue wall-clock budget across the slice-X1
/// multi-phase pipeline. Each phase that spawns a container asks for
/// `deadline_or_halt()` to compute its own deadline, and reports back
/// via `mark_killed_if(...)` whether the container was actually killed.
/// Once `exceeded` flips to true the runner short-circuits the rest of
/// the pipeline and produces a `WallClockExceeded` outcome.
struct WallClockBudget {
    start: std::time::Instant,
    cap: Duration,
    exceeded: bool,
}

impl WallClockBudget {
    fn new(cap: Duration) -> Self {
        Self {
            start: std::time::Instant::now(),
            cap,
            exceeded: false,
        }
    }

    /// Returns the remaining budget as a `Duration` to use as a phase
    /// deadline, or `None` if the budget is already spent (and marks
    /// `exceeded` as a side effect so subsequent phases skip too).
    fn deadline_or_halt(&mut self) -> Option<Duration> {
        if self.exceeded {
            return None;
        }
        let elapsed = self.start.elapsed();
        if elapsed >= self.cap {
            self.exceeded = true;
            None
        } else {
            Some(self.cap - elapsed)
        }
    }

    fn mark_killed_if(&mut self, killed: bool) {
        if killed {
            self.exceeded = true;
        }
    }
}

/// Remove the slice-X1 phase handoff files from the workspace. The
/// review diff (input to the review prompt) and the review findings
/// file (output of review, input of review-fix) are both Bellows-
/// internal — they must never land in any subsequent commit. Called
/// after review-fix and as a defensive sweep on the halt path.
///
/// Best-effort: a missing file is not an error. A genuinely failing
/// remove (permissions, IO error) is propagated.
async fn cleanup_phase_handoff_files(
    workspace: &workspace::Workspace,
) -> Result<(), std::io::Error> {
    for name in [
        policy::REVIEW_DIFF_FILE,
        policy::REVIEW_FINDINGS_FILE,
        policy::REVIEW_COMMIT_LOG_FILE,
    ] {
        let path = workspace.path().join(name);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Parse a GitHub repo URL like `https://github.com/owner/repo` (with or
/// without `.git` suffix or trailing slash) into `(owner, repo)`. Public
/// so the binary's `main.rs` can re-use it from the `Kill` subcommand —
/// PR #33 review finding #3 fix replaced the per-crate duplicate copy
/// with a single source of truth here, where the tests already live.
pub fn parse_owner_repo(url: &str) -> Result<(String, String), RunError> {
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

    #[test]
    fn run_error_shape_same_missing_brief_issue_matches() {
        // Brief AC #3: `Err(MissingAgentBrief(N))` recurring on the same
        // issue must collapse to a single log line. The shape key is the
        // tracker's input for that dedup; identical issue numbers must
        // produce identical shape keys.
        let a = RunError::MissingAgentBrief(42).shape();
        let b = RunError::MissingAgentBrief(42).shape();
        assert_eq!(a, b);
    }

    #[test]
    fn run_error_shape_distinguishes_different_missing_brief_issue() {
        // Brief AC #4: `MissingAgentBrief(42)` → `MissingAgentBrief(43)`
        // must emit a fresh line because the payload differs. The shape
        // key carries the issue number so the tracker can tell.
        let a = RunError::MissingAgentBrief(42).shape();
        let b = RunError::MissingAgentBrief(43).shape();
        assert_ne!(a, b, "shape keys for different issue numbers must differ");
    }

    #[test]
    fn run_error_shape_distinguishes_different_variants() {
        // Brief AC #4: different variant = different shape, even if a
        // payload happens to match. MissingAgentBrief(42) and
        // InvalidRepoUrl("42") must not collapse.
        let a = RunError::MissingAgentBrief(42).shape();
        let b = RunError::InvalidRepoUrl("42".to_string()).shape();
        assert_ne!(a, b);
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
            wall_clock_exceeded: false,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
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
            wall_clock_exceeded: false,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
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
            wall_clock_exceeded: false,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
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
            wall_clock_exceeded: false,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
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
            wall_clock_exceeded: false,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
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
            wall_clock_exceeded: false,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
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
    fn build_log_body_for_wall_clock_exceeded_mentions_cap_and_elapsed_minutes() {
        // Operator should be able to see at-a-glance that the run was
        // killed because the wall-clock cap fired, and how much time
        // elapsed before it did.
        let started = fixed_timestamp();
        let finished = started + chrono::Duration::minutes(60);
        let outcomes = PhaseOutcomes {
            implement: ImplementOutcome {
                exit_code: 137, // SIGKILL exit code
                stderr_tail: "(killed by deadline)".to_string(),
            },
            post_implement_gate: GateOutcome::default(),
            review: None,
            review_fix: None,
            end_pipeline_gate: None,
            wall_clock_exceeded: true,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
        };
        let body = build_log_body(
            &ExitReason::WallClockExceeded,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        );
        assert!(body.contains("WallClockExceeded"));
        assert!(body.to_lowercase().contains("wall-clock"));
        // 60 minutes elapsed should appear in human-readable form.
        assert!(body.contains("60"));
    }

    #[test]
    fn build_log_body_for_rate_limited_quotes_the_matched_signature() {
        // The stderr tail is already shown in the body for non-Success
        // reasons, so the matched signature naturally appears. The test
        // pins that the body specifically calls out the rate-limit
        // detection so an operator can verify the classification.
        let started = fixed_timestamp();
        let finished = started + chrono::Duration::seconds(30);
        let outcomes = PhaseOutcomes {
            implement: ImplementOutcome {
                exit_code: 1,
                stderr_tail:
                    r#"Error: API request failed: {"type":"rate_limit_error","message":"slow down"}"#
                        .to_string(),
            },
            post_implement_gate: GateOutcome::default(),
            review: None,
            review_fix: None,
            end_pipeline_gate: None,
            wall_clock_exceeded: false,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
        };
        let body = build_log_body(
            &ExitReason::RateLimited,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        );
        assert!(body.contains("RateLimited"));
        assert!(body.to_lowercase().contains("rate limit"));
        // The matched signature appears in the body via the stderr tail.
        assert!(body.contains("rate_limit_error"));
    }

    #[test]
    fn build_log_body_for_crash_with_auth_error_stderr_emits_refresh_auth_callout() {
        // Implement phase exited non-zero with stderr matching an Anthropic
        // auth-error signature. The run still classifies as Crash (no new
        // ExitReason variant per the routing-focused-enum principle), but
        // the log comment must include a callout telling the operator to
        // run `bellows refresh-auth` so the diagnostic pointer isn't
        // buried in the stderr tail.
        let started = fixed_timestamp();
        let finished = started + chrono::Duration::seconds(15);
        let outcomes = PhaseOutcomes {
            implement: ImplementOutcome {
                exit_code: 1,
                stderr_tail:
                    r#"Error: 401 Unauthorized: {"error":{"type":"authentication_error","message":"refresh_token_expired"}}"#
                        .to_string(),
            },
            post_implement_gate: GateOutcome::default(),
            review: None,
            review_fix: None,
            end_pipeline_gate: None,
            wall_clock_exceeded: false,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
        };
        let body = build_log_body(
            &ExitReason::Crash,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        );
        // The callout names the operator action explicitly.
        assert!(
            body.to_lowercase().contains("authentication error"),
            "auth-error callout must name the failure: {body}"
        );
        assert!(
            body.contains("bellows refresh-auth"),
            "auth-error callout must point the operator at `bellows refresh-auth`: {body}"
        );
        // Sanity: the stderr tail is also still surfaced (so a curious
        // operator can read the matched signature themselves).
        assert!(body.contains("refresh_token_expired"));
    }

    #[test]
    fn build_log_body_omits_refresh_auth_callout_when_exit_was_clean() {
        // Signature alone is NOT enough — the run must have actually exited
        // non-zero for the callout to appear. A clean Success run that
        // happens to mention "refresh_token_expired" somewhere benign
        // (e.g. inside committed docs) must not get the callout.
        let started = fixed_timestamp();
        let finished = started;
        let outcomes = PhaseOutcomes {
            implement: ImplementOutcome {
                exit_code: 0,
                stderr_tail:
                    "Documented how to handle refresh_token_expired in docs.md.".to_string(),
            },
            post_implement_gate: GateOutcome {
                cargo_clippy: Some(CheckResult { exit_code: 0, output: String::new() }),
                cargo_test: Some(CheckResult { exit_code: 0, output: String::new() }),
            },
            review: None,
            review_fix: None,
            end_pipeline_gate: None,
            wall_clock_exceeded: false,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
        };
        let body = build_log_body(
            &ExitReason::Success,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        );
        assert!(
            !body.contains("bellows refresh-auth"),
            "clean Success run must not show the refresh-auth callout: {body}"
        );
    }

    #[test]
    fn build_log_body_emits_placeholder_when_stderr_tail_is_empty() {
        // S1 smoke regression: when SIGKILL fires before any agent output
        // flushes, the stderr_tail is empty. Without the placeholder, the
        // body emitted an empty code fence which rendered as a useless
        // empty block in the PR comment. The placeholder explains why the
        // section is empty so the operator isn't left wondering.
        let started = fixed_timestamp();
        let finished = started + chrono::Duration::minutes(2);
        let outcomes = PhaseOutcomes {
            implement: ImplementOutcome {
                exit_code: 137,
                stderr_tail: String::new(), // empty — kill happened before any flush
            },
            post_implement_gate: GateOutcome::default(),
            review: None,
            review_fix: None,
            end_pipeline_gate: None,
            wall_clock_exceeded: true,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
        };
        let body = build_log_body(
            &ExitReason::WallClockExceeded,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        );
        assert!(body.to_lowercase().contains("no agent output was captured"));
        // The empty code fence section header is NOT emitted.
        assert!(!body.contains("### Agent output tail"));
    }

    #[test]
    fn build_log_body_emits_address_or_explain_violation_callout_when_backstop_fires() {
        // Slice 9.6: when the parser-as-backstop synthesises agent-notes
        // entries because blocker/important findings were silently
        // skipped, the log comment must surface the
        // `### Address-or-explain contract violated` callout naming
        // each offending finding by verbatim title + severity. Without
        // this surface, a reader of the PR comment would have to diff
        // agent-notes.md to figure out which findings the agent
        // skipped — defeating the point of the explicit-failure mode.
        use crate::policy::{ParsedFinding, Severity};
        let started = fixed_timestamp();
        let finished = started + chrono::Duration::seconds(120);
        let outcomes = PhaseOutcomes {
            implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new() },
            post_implement_gate: GateOutcome {
                cargo_clippy: Some(CheckResult { exit_code: 0, output: String::new() }),
                cargo_test: Some(CheckResult { exit_code: 0, output: String::new() }),
            },
            review: Some(ReviewOutcome {
                findings_text: Some("important: silently skipped — important".to_string()),
                exit_code: 0,
            }),
            review_fix: Some(FixOutcome { exit_code: 0 }),
            end_pipeline_gate: None,
            wall_clock_exceeded: false,
            backstop_violations: vec![
                ParsedFinding {
                    title: "important: silently skipped".to_string(),
                    severity: Severity::Important,
                    body: "body".to_string(),
                },
                ParsedFinding {
                    title: "blocker also silently skipped".to_string(),
                    severity: Severity::Blocker,
                    body: "body".to_string(),
                },
            ],
            implement_crash_synthesised: false,
        };
        let body = build_log_body(
            &ExitReason::AgentSelfReportedFailure,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        );
        assert!(
            body.contains("### Address-or-explain contract violated"),
            "log body must include the canonical violation callout heading: {body}"
        );
        assert!(
            body.contains("important: silently skipped"),
            "log body must name the first offending finding by verbatim title: {body}"
        );
        assert!(
            body.contains("blocker also silently skipped"),
            "log body must name the second offending finding by verbatim title: {body}"
        );
    }

    #[test]
    fn build_log_body_omits_violation_callout_when_no_backstop_violations() {
        // Defensive: a clean run with no violations must NOT surface the
        // callout — otherwise the canonical heading would appear in
        // every PR's log body, robbing it of meaning.
        let started = fixed_timestamp();
        let finished = started;
        let outcomes = PhaseOutcomes {
            implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new() },
            post_implement_gate: GateOutcome {
                cargo_clippy: Some(CheckResult { exit_code: 0, output: String::new() }),
                cargo_test: Some(CheckResult { exit_code: 0, output: String::new() }),
            },
            review: None,
            review_fix: None,
            end_pipeline_gate: None,
            wall_clock_exceeded: false,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
        };
        let body = build_log_body(
            &ExitReason::Success, 42, started, finished, "agent/42-x", &outcomes,
        );
        assert!(
            !body.contains("Address-or-explain contract violated"),
            "log body must NOT include the violation callout when no violations occurred: {body}"
        );
    }

    #[test]
    fn build_pr_body_for_wall_clock_exceeded_mentions_cap() {
        let body = build_pr_body(&ExitReason::WallClockExceeded, 42, None, None);
        assert!(body.to_lowercase().contains("wall-clock"));
        assert!(body.contains("run-log comment"));
    }

    #[test]
    fn build_pr_body_for_rate_limited_mentions_rate_limit() {
        let body = build_pr_body(&ExitReason::RateLimited, 42, None, None);
        assert!(body.to_lowercase().contains("rate limit"));
        assert!(body.contains("run-log comment"));
    }

    #[test]
    fn implement_crash_synth_outcomes_classify_as_crash_and_render_crash_pr_and_log_bodies() {
        // Issue #49 end-to-end shape: a PhaseOutcomes carrying the
        // synth flag (set true by the runner when implement crashed
        // with no commits) and a non-zero implement exit must route
        // through `classify_exit` to `Crash` even with
        // `has_agent_notes=true` (the synth wrote agent-notes.md
        // and committed it). The resulting PR body must be the
        // Crash body (`crashed`), NOT the AgentSelfReportedFailure
        // body which would quote the bellows-synthesised note as if
        // the agent had self-reported.
        let synth_note = policy::synthesize_implement_crash_entry(
            137,
            "Error: /workspace/entrypoint-user: bad interpreter",
        );
        let outcomes = PhaseOutcomes {
            implement: ImplementOutcome {
                exit_code: 137,
                stderr_tail: "Error: /workspace/entrypoint-user: bad interpreter".to_string(),
            },
            post_implement_gate: GateOutcome::default(),
            review: None,
            review_fix: None,
            end_pipeline_gate: None,
            wall_clock_exceeded: false,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: true,
        };
        let reason = policy::classify_exit(true, &outcomes);
        assert_eq!(
            reason,
            ExitReason::Crash,
            "synth + non-zero implement exit must classify as Crash, not \
             AgentSelfReportedFailure — the synth note is bellows-authored, \
             not an agent self-report",
        );
        // build_pr_body for Crash must NOT quote the synth note as
        // "self-reported failure" content.
        let pr_body = build_pr_body(&reason, 42, None, Some(synth_note.trim()));
        assert!(
            pr_body.contains("crashed"),
            "PR body for the synth-driven Crash must say `crashed`: {pr_body}"
        );
        assert!(
            !pr_body.to_lowercase().contains("self-reported failure"),
            "PR body must NOT frame the synth note as an agent self-report: {pr_body}"
        );
        // build_log_body for Crash surfaces the stderr tail directly
        // (from outcomes.implement.stderr_tail) so the operator can
        // see what the implement-phase agent printed before exiting,
        // independent of whatever the synth note also embedded.
        let started = fixed_timestamp();
        let finished = started + chrono::Duration::seconds(5);
        let log_body = build_log_body(&reason, 42, started, finished, "agent/42-x", &outcomes);
        assert!(
            log_body.contains("Crash"),
            "log body must include the Crash classification header: {log_body}"
        );
        assert!(
            log_body.contains("bad interpreter"),
            "log body must surface the implement-phase stderr tail: {log_body}"
        );
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

    // ---- Issue #40: cleanup of the test-first commit-log artefact ----

    #[tokio::test]
    async fn cleanup_phase_handoff_files_removes_review_commit_log_artefact() {
        // Acceptance criterion (brief): "The runner writes the
        // commit-log artefact alongside the diff artefact before the
        // review phase and cleans it up before the final `commit_all`
        // so it does not land in the PR diff."
        //
        // The existing `cleanup_phase_handoff_files` already removes
        // REVIEW_DIFF_FILE and REVIEW_FINDINGS_FILE; this test pins
        // that the new REVIEW_COMMIT_LOG_FILE is also swept by the
        // same helper. Without this guarantee the file would survive
        // through to `commit_all` and ship in the PR diff — exactly
        // what the cleanup step exists to prevent (mirrors the
        // existing contract for the other two handoff files).
        let remote_dir = tempfile::TempDir::new().unwrap();
        // Initialise a tiny git repo to act as the "remote" for prepare().
        for args in &[
            &["init"][..],
            &["config", "user.email", "test@example.com"][..],
            &["config", "user.name", "Test"][..],
        ] {
            let status = std::process::Command::new("git")
                .args(*args)
                .current_dir(remote_dir.path())
                .status()
                .unwrap();
            assert!(status.success());
        }
        std::fs::write(remote_dir.path().join("README.md"), "test\n").unwrap();
        for args in &[&["add", "."][..], &["commit", "-m", "initial"][..]] {
            let status = std::process::Command::new("git")
                .args(*args)
                .current_dir(remote_dir.path())
                .status()
                .unwrap();
            assert!(status.success());
        }

        let remote_url = remote_dir.path().to_string_lossy().to_string();
        let workspace = workspace::prepare(&remote_url, "agent/40-cleanup")
            .await
            .unwrap();

        // Write all three handoff files; cleanup must remove every one.
        for name in &[
            policy::REVIEW_DIFF_FILE,
            policy::REVIEW_FINDINGS_FILE,
            policy::REVIEW_COMMIT_LOG_FILE,
        ] {
            tokio::fs::write(workspace.path().join(name), b"handoff\n")
                .await
                .unwrap();
            assert!(
                workspace.path().join(name).exists(),
                "pre-cleanup: {name} must exist",
            );
        }

        cleanup_phase_handoff_files(&workspace).await.unwrap();

        for name in &[
            policy::REVIEW_DIFF_FILE,
            policy::REVIEW_FINDINGS_FILE,
            policy::REVIEW_COMMIT_LOG_FILE,
        ] {
            assert!(
                !workspace.path().join(name).exists(),
                "post-cleanup: {name} must have been removed",
            );
        }
    }
}
