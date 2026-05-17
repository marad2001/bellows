use std::io::Write;
use std::time::Duration;

use crate::auth::Auth;
use crate::chain_walker::{
    self, format_phase_engine_log, handle_implement_rate_limit, handle_non_implement_rate_limit,
    pick_engine_for_phase, PickError, PickReason, PickedEntry, RateLimitDisposition, StateFile,
};
use crate::config::{
    AuthMethod, ChainEntry, Config, Engine, EngineLabelOverride, RuntimeLabelsConfig,
};
use crate::policy::{
    self, AnalysisOutcome, CheckResult, ExitReason, FixOutcome, GateOutcome, ImplementOutcome,
    PhaseOutcomes, ReviewOutcome,
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
    /// Issue #81 / ADR-0005: both `engine:claude` and `engine:codex`
    /// labels are present on the same issue. Refuse-to-claim parallel
    /// to `MissingAgentBrief` — the polling-loop's `OutcomeTransition`
    /// tracker dedupes recurring ambiguous-label ticks against
    /// identical payloads via the per-variant `shape()` key.
    #[error(
        "issue #{0} carries both `engine:claude` and `engine:codex` labels; \
         operator must pick one. Refusing to claim."
    )]
    AmbiguousEngineLabels(u64),
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
            RunError::AmbiguousEngineLabels(n) => format!("ambiguous_engine_labels:{n}"),
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
    /// The polling tick refused to claim. The shape carries WHY so the
    /// log line and status-file rendering can distinguish the slice-b
    /// case (open `agent/*` PR may still gate master, #42) from the
    /// slice-#76 case (ADR-0003: deletion of a stale `agent/<N>-*`
    /// remote branch failed before claim). Both cases are recoverable
    /// across ticks: closing/merging the PR, or fixing branch
    /// protection / PAT scope, lets the next tick proceed.
    Blocked {
        reason: BlockReason,
    },
}

struct PrRouting<'a> {
    draft: bool,
    outcome_label: &'a str,
    pr_label: Option<&'a str>,
    fallback_announcement: Option<&'static str>,
}

const ADR_0006_DRAFT_FALLBACK_ANNOUNCEMENT: &str =
    "bellows: ADR-0006 fallback — target auto-merge workflow does not advertise \
     an `agent-noted` filter; opening SuccessWithNotes PR as draft so the \
     agent note cannot auto-merge unread";

fn pr_routing_for_reason<'a>(
    reason: &ExitReason,
    labels: &'a RuntimeLabelsConfig,
    auto_merge_workflow_supports_agent_noted_filter: bool,
) -> PrRouting<'a> {
    match reason {
        ExitReason::Success => PrRouting {
            draft: false,
            outcome_label: &labels.agent_done,
            pr_label: None,
            fallback_announcement: None,
        },
        ExitReason::SuccessWithNotes => {
            let draft = !auto_merge_workflow_supports_agent_noted_filter;
            PrRouting {
                draft,
                outcome_label: &labels.agent_noted,
                pr_label: Some(&labels.agent_noted),
                fallback_announcement: draft.then_some(ADR_0006_DRAFT_FALLBACK_ANNOUNCEMENT),
            }
        }
        ExitReason::AgentSelfReportedFailure
        | ExitReason::Crash
        | ExitReason::FinalTestsRed
        | ExitReason::WallClockExceeded
        | ExitReason::AuthError => PrRouting {
            draft: true,
            outcome_label: &labels.agent_failed,
            pr_label: None,
            fallback_announcement: None,
        },
        ExitReason::RateLimited => PrRouting {
            draft: true,
            outcome_label: &labels.agent_rate_limited,
            pr_label: None,
            fallback_announcement: None,
        },
        ExitReason::Cancelled => PrRouting {
            draft: true,
            outcome_label: &labels.agent_cancelled,
            pr_label: None,
            fallback_announcement: None,
        },
    }
}

/// Why a `RunOutcome::Blocked` tick refused to claim. Split into one
/// variant per block source so the status file's on-disk schema, the
/// polling-loop log line, and `bellows status`'s human-readable
/// summary can all be specific about the action item.
///
/// Tagged-snake-case representation keeps the JSON readable in the
/// status file:
/// `{"kind": "open_agent_prs", "pr_numbers": [...]}` /
/// `{"kind": "stale_agent_branch_deletion_failed", "branch": "...", "error": "..."}`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockReason {
    /// Slice b (#42): one or more open `agent/*` PRs may still gate
    /// master. `pr_numbers` is empty in the fail-closed case (the
    /// list-PRs API call failed); the renderer distinguishes that
    /// from a known set.
    OpenAgentPrs { pr_numbers: Vec<u64> },
    /// Issue #76 / ADR-0003: the pre-claim sweep of `agent/<N>-*` refs
    /// on origin failed for `branch`. `error` is the formatted
    /// `octocrab::Error` so the operator can read it in `bellows
    /// status` or the polling log without grepping for the underlying
    /// request. Retried on every subsequent tick.
    StaleAgentBranchDeletionFailed {
        branch: String,
        error: String,
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

/// Per-repo claim candidate the multi-repo polling tick (#35)
/// collects across configured repos before picking the
/// globally-lowest-number issue. Holds the parsed `(owner, repo)`
/// tuple plus the original repo URL so the downstream pipeline can
/// keep using `workspace::prepare(&repo_url, ...)` without
/// re-parsing.
///
/// `repo_order` (issue #116 / ADR-0007) is the candidate's index
/// into `config.repos`. The polling-tick sort uses it as the final
/// tier of the tiebreak — issue.number ascending, then created_at
/// ascending, then declared `[[repo]]` order. We embed the order
/// in the candidate itself so the sort closure is local to the
/// candidate list and doesn't need a parallel position table.
struct RepoCandidate {
    owner: String,
    repo: String,
    repo_url: String,
    issue: tracker::Issue,
    repo_order: usize,
}

fn auth_for_chain_entry(config: &Config, entry: &ChainEntry) -> Auth {
    match &config.auth.method {
        AuthMethod::Subscription => match entry.engine {
            Engine::Opencode => Auth::EnvFile {
                engine: entry.engine,
                model: entry.model.clone(),
                env_file_path: crate::main_helpers::expand_tilde_path(
                    &config.auth.opencode.api_key_env_file,
                ),
            },
            Engine::Claude | Engine::Codex => Auth::Subscription {
                engine: entry.engine,
                model: entry.model.clone(),
                credentials_volume_name: config
                    .auth
                    .for_engine(entry.engine)
                    .credentials_volume
                    .clone(),
            },
        },
    }
}

/// Re-loop sweep across every cleared repo's `blocked-by` dependents.
/// Issue #116 / ADR-0007: runs only when `run_once`'s normal pass
/// produced an empty filtered candidate set AND at least one cleared
/// repo exists. For each dependent the sweep:
///
/// 1. Lists the dependent issues (pickup_label AND blocked_by_label).
/// 2. Fetches each dependent's brief and parses the `**Blocked by:**`
///    line via `tracker::parse_blocked_by_section_with_log_writer`.
/// 3. For `Blockers(nums)`: queries each `n`'s state; strips the
///    label iff every blocker is `IssueClosureState::Closed`.
/// 4. For `NoBlockers` (explicit `None`, missing section, only
///    unparseable / cross-repo tokens): strips the label
///    unconditionally — there is nothing to wait for.
/// 5. For `Unverifiable` (the dependent has no `## Agent Brief`
///    comment at all): leaves the label in place and logs a warning
///    naming the issue number.
///
/// Per-issue detail (which blockers were open, which closed) writes
/// to the same log writer at a verbose prefix so an operator can
/// `grep` the log file for stuck dependents without spamming the
/// foreground tick line. When at least one dependent is found, the
/// single foreground summary is `bellows: re-loop swept N blocked-by
/// issues, cleared M`; an empty dependent-list check stays quiet so
/// normal idle ticks do not spam the foreground log.
///
/// Errors on individual API calls are caught and logged rather than
/// propagated: the sweep is a best-effort reconciliation. The tick
/// it runs on still returns `RunOutcome::Idle`; the operator sees
/// the warning lines on the next tick.
async fn run_reloop_sweep(
    client: &octocrab::Octocrab,
    cleared_repos: &[(String, String, usize)],
    pickup_label: &str,
    blocked_by_label: &str,
    log_writer: &mut dyn Write,
) {
    let mut swept = 0usize;
    let mut cleared = 0usize;
    for (owner, repo, _) in cleared_repos {
        let dependents = match tracker::list_blocked_by_issues(
            client,
            owner,
            repo,
            pickup_label,
            blocked_by_label,
        )
        .await
        {
            Ok(d) => d,
            Err(e) => {
                let _ = writeln!(
                    log_writer,
                    "bellows: re-loop list-dependents failed for {}/{}: {}",
                    owner, repo, e,
                );
                continue;
            }
        };
        for dependent in dependents {
            swept += 1;
            if sweep_one_dependent(
                client,
                owner,
                repo,
                dependent.number,
                blocked_by_label,
                log_writer,
            )
            .await
            {
                cleared += 1;
            }
        }
    }
    if swept > 0 {
        announce(
            log_writer,
            &format!(
                "bellows: re-loop swept {} blocked-by issues, cleared {}",
                swept, cleared,
            ),
        );
    }
}

/// Reconcile a single `blocked-by`-labelled dependent. Returns
/// `true` when the label was successfully stripped (the dependent
/// has no remaining open blockers); `false` when it was left in
/// place (some blocker still open, the brief was unverifiable, or a
/// per-step API call failed).
async fn sweep_one_dependent(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    issue_number: u64,
    blocked_by_label: &str,
    log_writer: &mut dyn Write,
) -> bool {
    let brief = match tracker::fetch_agent_brief(client, owner, repo, issue_number).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            let _ = writeln!(
                log_writer,
                "bellows: re-loop: leaving blocked-by on issue #{} ({}/{}): no `## Agent Brief` comment found",
                issue_number, owner, repo,
            );
            return false;
        }
        Err(e) => {
            let _ = writeln!(
                log_writer,
                "bellows: re-loop: leaving blocked-by on issue #{} ({}/{}): brief fetch failed: {}",
                issue_number, owner, repo, e,
            );
            return false;
        }
    };

    let parsed = tracker::parse_blocked_by_section_with_log_writer(&brief, log_writer);
    match parsed {
        tracker::BlockedBySection::Unverifiable => {
            let _ = writeln!(
                log_writer,
                "bellows: re-loop: leaving blocked-by on issue #{} ({}/{}): brief comment present but malformed (no `## Agent Brief` header inside the parsed body)",
                issue_number, owner, repo,
            );
            false
        }
        tracker::BlockedBySection::NoBlockers => {
            // No parseable blockers — explicit `None`, missing
            // section, or only unparseable / cross-repo tokens.
            // Strip the label; the dependent becomes claimable on
            // the next tick.
            match tracker::strip_issue_label(
                client,
                owner,
                repo,
                issue_number,
                blocked_by_label,
            )
            .await
            {
                Ok(_) => {
                    let _ = writeln!(
                        log_writer,
                        "bellows: re-loop: stripped blocked-by from issue #{} ({}/{}): brief lists no blockers",
                        issue_number, owner, repo,
                    );
                    true
                }
                Err(e) => {
                    let _ = writeln!(
                        log_writer,
                        "bellows: re-loop: strip-label PATCH failed for issue #{} ({}/{}): {}",
                        issue_number, owner, repo, e,
                    );
                    false
                }
            }
        }
        tracker::BlockedBySection::Blockers(blockers) => {
            let mut all_closed = true;
            for blocker in &blockers {
                match tracker::fetch_issue_state(client, owner, repo, *blocker).await {
                    Ok(tracker::IssueClosureState::Closed) => {
                        let _ = writeln!(
                            log_writer,
                            "bellows: re-loop: blocker #{} for dependent #{} ({}/{}) is closed",
                            blocker, issue_number, owner, repo,
                        );
                    }
                    Ok(tracker::IssueClosureState::Open) => {
                        let _ = writeln!(
                            log_writer,
                            "bellows: re-loop: blocker #{} for dependent #{} ({}/{}) is still open",
                            blocker, issue_number, owner, repo,
                        );
                        all_closed = false;
                    }
                    Err(e) => {
                        let _ = writeln!(
                            log_writer,
                            "bellows: re-loop: state check for blocker #{} (dependent #{} {}/{}) failed: {}",
                            blocker, issue_number, owner, repo, e,
                        );
                        all_closed = false;
                    }
                }
            }
            if !all_closed {
                return false;
            }
            match tracker::strip_issue_label(
                client,
                owner,
                repo,
                issue_number,
                blocked_by_label,
            )
            .await
            {
                Ok(_) => {
                    let _ = writeln!(
                        log_writer,
                        "bellows: re-loop: stripped blocked-by from issue #{} ({}/{}): every blocker closed",
                        issue_number, owner, repo,
                    );
                    true
                }
                Err(e) => {
                    let _ = writeln!(
                        log_writer,
                        "bellows: re-loop: strip-label PATCH failed for issue #{} ({}/{}): {}",
                        issue_number, owner, repo, e,
                    );
                    false
                }
            }
        }
    }
}

pub async fn run_once(
    client: &octocrab::Octocrab,
    config: &Config,
    log_writer: &mut dyn Write,
    status_ctx: Option<&StatusContext>,
) -> Result<RunOutcome, RunError> {
    // Issue #35: multi-repo polling. For each configured repo, do the
    // per-repo pre-claim PR check (#42), and on cleared repos collect
    // the oldest open `ready-for-agent` issue. Across the combined set
    // of cleared repos, claim the GLOBALLY-oldest issue by
    // `created_at`.
    //
    // Per-repo pre-claim isolation: repo A being blocked by its own
    // open `agent/*` PR does NOT block claims from unblocked repo B —
    // the gating rationale (prior agent's PR may still gate THIS
    // repo's master) is per-repo, and the cross-repo invariant is just
    // concurrency=1 which the loop maintains by virtue of being serial
    // anyway. Only when EVERY repo is blocked does the tick return
    // `RunOutcome::Blocked`.
    let mut blocked_prs: Vec<u64> = Vec::new();
    let mut any_blocked_unknown = false;
    let mut blocked_any = false;
    let mut candidates: Vec<RepoCandidate> = Vec::new();
    let mut any_clear = false;
    // Issue #116 / ADR-0007: parallel per-repo list of (owner, repo,
    // repo_order) on cleared repos, used by the re-loop sweep when
    // the normal pass produces an empty filtered candidate set.
    let mut cleared_repos: Vec<(String, String, usize)> = Vec::new();

    for (repo_order, repo_cfg) in config.repos.iter().enumerate() {
        let (owner, repo) = parse_owner_repo(&repo_cfg.url)?;
        let preclaim = match tracker::list_open_agent_prs(client, &owner, &repo).await {
            Ok(prs) if prs.is_empty() => PreClaim::Clear,
            Ok(prs) => PreClaim::Blocked(prs),
            Err(e) => {
                let _ = writeln!(
                    log_writer,
                    "bellows: pre-claim PR check failed for {}/{}; failing closed (treating that repo as blocked this tick): {}",
                    owner, repo, e,
                );
                PreClaim::BlockedUnknown
            }
        };
        match preclaim {
            PreClaim::Blocked(prs) => {
                blocked_any = true;
                blocked_prs.extend(prs);
            }
            PreClaim::BlockedUnknown => {
                blocked_any = true;
                any_blocked_unknown = true;
            }
            PreClaim::Clear => {
                any_clear = true;
                let next = tracker::find_next_issue(
                    client,
                    &owner,
                    &repo,
                    &config.polling.pickup_label,
                    &config.runtime_labels.agent_in_progress,
                    &config.runtime_labels.blocked_by,
                )
                .await?;
                if let Some(issue) = next {
                    candidates.push(RepoCandidate {
                        owner: owner.clone(),
                        repo: repo.clone(),
                        repo_url: repo_cfg.url.clone(),
                        issue,
                        repo_order,
                    });
                }
                cleared_repos.push((owner, repo, repo_order));
            }
        }
    }

    // No cleared repo produced a claimable issue. If at least one repo
    // was blocked, this whole tick is "blocked" from the operator's
    // perspective; otherwise it's idle.
    if candidates.is_empty() {
        if blocked_any && !any_clear {
            // Every configured repo was blocked. Report the union of
            // known blocking PR numbers (empty when every blocker was
            // an unknown / list-PRs failure — same fail-closed contract
            // as the single-repo path).
            let prs = if any_blocked_unknown && blocked_prs.is_empty() {
                Vec::new()
            } else {
                blocked_prs
            };
            let reason = BlockReason::OpenAgentPrs { pr_numbers: prs };
            if let Some(ctx) = status_ctx
                && let Err(e) = ctx.write_blocked(&reason).await
            {
                let _ = writeln!(
                    log_writer,
                    "bellows: could not write blocked status (continuing): {}",
                    e,
                );
            }
            return Ok(RunOutcome::Blocked { reason });
        }

        // Some repos cleared but produced no candidates (or all repos
        // were blocked but at least one cleared — but that branch
        // can't happen here because candidates is empty). Either way,
        // the steady-state log line is Idle — but FIRST, if any
        // cleared repo has blocked-by-labelled issues, run the
        // re-loop sweep (issue #116 / ADR-0007). The sweep runs only
        // when there's no unblocked work to claim; a cleared
        // dependent becomes claimable on the NEXT tick's normal
        // pass.
        if !cleared_repos.is_empty() {
            run_reloop_sweep(
                client,
                &cleared_repos,
                &config.polling.pickup_label,
                &config.runtime_labels.blocked_by,
                log_writer,
            )
            .await;
        }
        if let Some(ctx) = status_ctx
            && let Err(e) = ctx.write_idle().await
        {
            let _ = writeln!(
                log_writer,
                "bellows: could not clear status (continuing): {}",
                e,
            );
        }
        return Ok(RunOutcome::Idle);
    }

    // At least one cleared repo produced a candidate. Status: clear
    // any prior blocked state so `bellows status` doesn't lie until
    // we either claim (write_busy) or short-circuit on a per-repo
    // error.
    if let Some(ctx) = status_ctx
        && let Err(e) = ctx.write_idle().await
    {
        let _ = writeln!(
            log_writer,
            "bellows: could not clear blocked status (continuing): {}",
            e,
        );
    }

    // Pick the lowest-`issue.number` candidate. Issue #116 /
    // ADR-0007 promoted issue.number to the primary sort key (with
    // created_at as the cross-repo tie-breaker, declared `[[repo]]`
    // order as the final tie-breaker). The `Option<DateTime<Utc>>`
    // shape on created_at means an issue whose payload didn't
    // include the field defaults to `MIN_UTC` (treated as "older
    // than anything else") — defensive, since real GitHub payloads
    // always include the field and the only path to a missing
    // value is a test fixture that didn't bother.
    candidates.sort_by(|a, b| {
        a.issue.number.cmp(&b.issue.number).then_with(|| {
            let a_t = a
                .issue
                .created_at
                .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC);
            let b_t = b
                .issue
                .created_at
                .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC);
            a_t.cmp(&b_t).then_with(|| a.repo_order.cmp(&b.repo_order))
        })
    });
    let RepoCandidate {
        owner,
        repo,
        repo_url,
        issue,
        repo_order: _,
    } = candidates.into_iter().next().expect("non-empty candidates");

    // Issue #76 / ADR-0003: pre-claim sweep. Delete every `agent/<N>-*`
    // ref on origin for the candidate's issue number before we attempt
    // to claim. A stale ref from a prior failed run would otherwise
    // crash the next push with a non-fast-forward — only after the
    // agent has already spent ~30 minutes. Failure here surfaces as
    // RunOutcome::Blocked so the next tick retries idempotently. The
    // count drives a one-line summary log (suppressed when 0 deletions
    // — the steady-state case).
    let swept = match tracker::delete_stale_agent_branches(client, &owner, &repo, issue.number)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            // Both error variants carry a usable branch identifier — a
            // specific `agent/<N>-<slug>` for the DELETE-failed case,
            // and the anchored prefix `agent/<N>-*` for the
            // list-failed case (the operator's recovery action is the
            // same either way: unblock GitHub access, retry the tick).
            let (branch, source) = match &e {
                tracker::DeleteStaleBranchError::ListFailed { issue_number, source } => {
                    (format!("agent/{}-*", issue_number), source.to_string())
                }
                tracker::DeleteStaleBranchError::DeleteFailed { branch, source } => {
                    (branch.clone(), source.to_string())
                }
            };
            let reason = BlockReason::StaleAgentBranchDeletionFailed {
                branch,
                error: source,
            };
            if let Some(ctx) = status_ctx
                && let Err(write_err) = ctx.write_blocked(&reason).await
            {
                let _ = writeln!(
                    log_writer,
                    "bellows: could not write blocked status (continuing): {}",
                    write_err,
                );
            }
            return Ok(RunOutcome::Blocked { reason });
        }
    };

    // Per-claim sweep summary (suppressed on the steady-state
    // zero-deletion case, so clean ticks don't spam the log). The line
    // fires after a successful sweep and ahead of the brief-fetch /
    // claim PATCH; the wording — "before claiming issue #N" — names
    // an intent, so the log is still honest if a subsequent step
    // short-circuits (MissingAgentBrief, ClaimError::Contended) before
    // the matching `claimed issue` announce.
    if !swept.is_empty() {
        announce(
            log_writer,
            &format!(
                "bellows: pre-claim swept {} stale agent/{}-* branch(es) before claiming issue #{}",
                swept.len(),
                issue.number,
                issue.number,
            ),
        );
    }

    // Fetch the agent brief BEFORE claiming. If it's missing we return
    // an error without label-swapping the issue — the next polling tick
    // will see it fresh once a human posts the brief, instead of leaving
    // it stuck in agent-in-progress with no automated recovery.
    let brief = tracker::fetch_agent_brief(client, &owner, &repo, issue.number)
        .await?
        .ok_or(RunError::MissingAgentBrief(issue.number))?;
    let repo_label = format!("{}/{}", owner, repo);

    // Issue #81 / ADR-0005: engine-override resolution from labels,
    // checked BEFORE claim (parallel to MissingAgentBrief). Both
    // `engine:claude` AND `engine:codex` present is operator error —
    // refuse-to-claim and surface a stable shape so the polling-loop's
    // transition tracker dedupes recurring ambiguous-label ticks. A
    // single `engine:<name>` label forces every phase to that engine
    // (model defaults to the CLI's pick — labels don't carry a model
    // pin). The chain walk produces the engine when no override is
    // present.
    let issue_label_names: Vec<&str> =
        issue.labels.iter().map(|l| l.name.as_str()).collect();
    let engine_label_override = EngineLabelOverride::parse(&issue_label_names)
        .map_err(|_| RunError::AmbiguousEngineLabels(issue.number))?;

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
            repo: repo_label.clone(),
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

    // ADR-0004: parse the target repo's CI workflow once at prepare
    // time and snapshot the resolved gate commands (or fallback) on
    // the Workspace. Both the post-implement and end-pipeline gates
    // read from this snapshot so the in-flight verdict is stable
    // across mid-pipeline `.github/workflows/ci.yml` edits.
    let workspace = workspace::prepare_with_gates(&repo_url, &branch_name, &config.gates).await?;

    let repo_slug = crate::repo_slug(&repo_url);

    // Per-repo SSH deploy-keys opt-in (issue #69 / ADR-0002). The
    // matching `[[repo]]` block's `deploy_keys` decides whether the
    // sandbox containers (run_agent + run_cargo_checks) get the
    // deploy-keys volume mounted read-only at /home/bellows/.ssh/.
    // Cloned because the candidate was moved out of `config.repos`
    // above; cheap because the typical list is one or two key names.
    let deploy_keys: Vec<String> = config
        .repos
        .iter()
        .find(|r| r.url == repo_url)
        .map(|r| r.deploy_keys.clone())
        .unwrap_or_default();
    let ssh_keys_volume = config.auth.ssh_keys_volume.clone();

    // Issue #82 / ADR-0005: persisted per-engine rate-limit state.
    // Lives alongside the bellows.log file; absent on first claim →
    // empty state (every engine hot).
    let state_path = state_file_path_alongside_log(&config.logging.path);
    let mut state = StateFile::load(&state_path)?;
    if let Some(parent) = state_path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = std::fs::create_dir_all(parent);
    }

    // Implementer-CLI for this run. `None` until implement-phase end;
    // set to whichever engine actually committed (accounting for any
    // in-place advancement). Subsequent phases consult it for the
    // diversity preference.
    let mut implementer_cli: Option<Engine> = None;

    let auth_for = |entry: &ChainEntry| -> Auth { auth_for_chain_entry(config, entry) };

    // Per-issue wall-clock budget. Threaded through every container call
    // below; `mark_killed_if` flips `exceeded` whenever a sandbox run
    // reports the deadline fired.
    let mut budget = WallClockBudget::new(Duration::from_secs(
        config.agent.wall_clock_minutes.get() * 60,
    ));
    let mut agent_note_synth_spans = Vec::new();

    // Pre-implement chain walk (issue #82). Picks the engine for the
    // implement phase from `config.phases.implement.cli_chain` under
    // the freshly-loaded state file, then runs the implement phase in
    // a bounded two-iteration loop so a rate-limit at base SHA can
    // in-place-advance once before terminating.
    announce(
        log_writer,
        "bellows: phase 1/8 — implement (running agent in sandbox container, this is the long one)",
    );
    let head_before_implement = workspace::head_sha(&workspace).await?;
    let mut implement_advances_used: u8 = 0;
    let mut rate_limited_phase: Option<&'static str> = None;
    let (implement_chain_entry, implement_agent_run, head_after_implement, claude_pr_body) = loop {
        let now = chrono::Utc::now();
        let pick_result = pick_engine_for_phase(
            &config.phases.implement.cli_chain,
            &state,
            None,
            engine_label_override,
            now,
        );
        let mut picked = match pick_result {
            Ok(p) => p,
            Err(PickError::AllCooling) => {
                // Every chain entry is cooling per the state file →
                // terminate as RateLimited without invoking any agent.
                rate_limited_phase = Some("implement");
                announce(
                    log_writer,
                    "bellows: every implement chain entry is cooling per bellows-state.json; terminating as RateLimited",
                );
                let _ = state.save(&state_path);
                break (
                    ChainEntry {
                        engine: Engine::Claude,
                        model: None,
                    },
                    sandbox::AgentRun {
                        exit_code: 0,
                        stderr_tail: "(implement skipped: all chain entries cooling)".to_string(),
                        killed_by_deadline: false,
                    },
                    head_before_implement.clone(),
                    None,
                );
            }
        };
        // After the first in-place advance, override the picker's
        // reason so the run-log line tells the operator the second
        // invocation happened because of a rate-limit, not because
        // chain[0] was cooling.
        if implement_advances_used > 0 {
            picked = PickedEntry {
                entry: picked.entry,
                reason: PickReason::InPlaceAdvancementAfterRateLimit,
            };
        }
        if engine_label_override.is_some() {
            announce(
                log_writer,
                &format!(
                    "bellows: engine forced via engine:{} label; chain walking skipped",
                    picked.entry.engine.as_name(),
                ),
            );
        }
        announce(
            log_writer,
            &format_phase_engine_log("implement", &picked.entry, picked.reason),
        );

        // Render kickoff for this engine. Each iteration re-renders so
        // an in-place advance to a different engine sees the
        // engine-specific kickoff body.
        let kickoff = policy::render_kickoff_for_engine(
            picked.entry.engine,
            &brief,
            &repo_url,
            &branch_name,
        );
        tokio::fs::write(workspace.path().join(".bellows-kickoff.md"), &kickoff).await?;

        let auth = auth_for(&picked.entry);

        let agent_run = sandbox::run_agent(
            &workspace,
            &auth,
            claimed.number,
            &repo_label,
            &repo_slug,
            &ssh_keys_volume,
            &deploy_keys,
            log_writer,
            budget.deadline_or_halt(),
        )
        .await?;
        budget.mark_killed_if(agent_run.killed_by_deadline);
        announce(
            log_writer,
            &format!(
                "bellows: implement done (exit {}{})",
                agent_run.exit_code,
                if agent_run.killed_by_deadline {
                    ", killed by wall-clock"
                } else {
                    ""
                },
            ),
        );

        // If the agent wrote a PR description file, capture + remove
        // it before committing so it does NOT appear in the diff.
        let pr_description_path = workspace.path().join(".bellows-pr-description.md");
        let pr_body = if pr_description_path.exists() {
            let body = tokio::fs::read_to_string(&pr_description_path).await?;
            tokio::fs::remove_file(&pr_description_path).await?;
            Some(body.trim().to_string())
        } else {
            None
        };

        announce(log_writer, "bellows: committing + pushing implement branch");
        let head_after =
            workspace::commit_all_and_push_if_advanced(&workspace, &head_before_implement).await?;

        // Rate-limit handling (issue #82). Forced engine bypasses
        // chain walking: any rate-limit terminates without an
        // in-place advance.
        if agent_run.exit_code != 0
            && policy::is_rate_limit_signature(&agent_run.stderr_tail)
            && !budget.exceeded
        {
            let at_base_sha = head_after == head_before_implement;
            let now = chrono::Utc::now();
            if engine_label_override.is_some() {
                let parsed = chain_walker::parse_cooling_until(
                    picked.entry.engine,
                    &agent_run.stderr_tail,
                    now,
                );
                state.record_rate_limit(picked.entry.engine, parsed.cooling_until);
                if parsed.used_fallback {
                    announce(
                        log_writer,
                        "bellows: rate-limit stderr had no parseable timestamp; using conservative 5-minute fallback cooldown",
                    );
                }
                let _ = state.save(&state_path);
                rate_limited_phase = Some("implement");
                break (picked.entry, agent_run, head_after, pr_body);
            }
            // Record fallback flag for log visibility before
            // delegating to the composed handler.
            let parsed = chain_walker::parse_cooling_until(
                picked.entry.engine,
                &agent_run.stderr_tail,
                now,
            );
            if parsed.used_fallback {
                announce(
                    log_writer,
                    "bellows: rate-limit stderr had no parseable timestamp; using conservative 5-minute fallback cooldown",
                );
            }
            let disposition = handle_implement_rate_limit(
                &mut state,
                picked.entry.engine,
                &agent_run.stderr_tail,
                now,
                at_base_sha,
                implement_advances_used,
            );
            let _ = state.save(&state_path);
            match disposition {
                RateLimitDisposition::InPlaceAdvance => {
                    implement_advances_used += 1;
                    announce(
                        log_writer,
                        &format!(
                            "bellows: implement rate-limited at base SHA with engine={}; in-place-advancing to next hot chain entry (max 1 per phase invocation)",
                            picked.entry.engine.as_name(),
                        ),
                    );
                    continue;
                }
                RateLimitDisposition::Terminate => {
                    rate_limited_phase = Some("implement");
                    break (picked.entry, agent_run, head_after, pr_body);
                }
            }
        }

        // Implement phase produced output (or crashed for a non-
        // rate-limit reason). Record the engine that actually ran as
        // the implementer-CLI so subsequent phases can apply the
        // diversity preference.
        implementer_cli = Some(picked.entry.engine);
        break (picked.entry, agent_run, head_after, pr_body);
    };

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
        let synth_entry = policy::synthesize_implement_crash_entry(
            implement_agent_run.exit_code,
            &implement_agent_run.stderr_tail,
        );
        let synth_span = policy::append_bellows_synth_entry(
            &mut new_notes,
            &synth_entry,
            policy::BellowsSynthCause::ImplementCrash,
        );
        agent_note_synth_spans.push(synth_span);
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
            "bellows: phase 2/8 — cargo checks gate (clippy + test, fresh container)",
        );
        for line in workspace.gate_commands().announcement_lines() {
            announce(log_writer, &line);
        }
        let run = sandbox::run_cargo_checks(
            &workspace,
            claimed.number,
            &repo_label,
            &repo_slug,
            &ssh_keys_volume,
            &deploy_keys,
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
        // Issue #103: short-circuit the guard on diffs that touch no
        // Rust source at all. Doc-only briefs (ADRs, markdown
        // updates) otherwise trip the has_new_tests scan as false-
        // positives — the scan is looking for `#[test]` attributes
        // in `+`-prefixed lines, of which there are none in a
        // markdown-only diff, so the synth path fires even though
        // there is no Rust code to test. The diff-shape check is
        // a mechanical guard against that false-positive. The log
        // line is distinct from the label-skip and fired paths so
        // an operator reading bellows.log can tell which branch the
        // guard took on this run.
        if !policy::diff_contains_rs_files(&diff) {
            announce(
                log_writer,
                "bellows: weak-test guard: diff contains no Rust source — skipping",
            );
        } else if !policy::has_new_tests(&diff) {
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
            let synth_entry = policy::synthesize_no_new_tests_entry();
            let synth_span = policy::append_bellows_synth_entry(
                &mut new_notes,
                &synth_entry,
                policy::BellowsSynthCause::WeakTestGuard,
            );
            agent_note_synth_spans.push(synth_span);
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
    // Slice X2: security-review and security-fix phase outcomes, sitting
    // between review-fix and the end-of-pipeline cargo gate. Same
    // `None`-when-skipped contract as `review` / `review_fix`.
    let mut security_outcome: Option<AnalysisOutcome> = None;
    let mut security_fix_outcome: Option<FixOutcome> = None;
    let mut end_pipeline_gate: Option<GateOutcome> = None;
    // Issue #123 / ADR-0009 slice 1: phase-8 merger verdict. Stored
    // in PhaseOutcomes for the PR/log build sites; does NOT yet feed
    // classify_exit (slice 2 / #124 wires routing).
    let mut merger_verdict: Option<policy::MergerVerdict> = None;
    // Slice 9.6: parser-as-backstop violations. Populated after the
    // per-finding/nit-batch review-fix invocations complete and the
    // parser cross-references findings against agent-notes sections.
    // Non-empty values force the run to AgentSelfReportedFailure via
    // a synthetic agent-notes entry.
    let mut backstop_violations: Vec<policy::ParsedFinding> = Vec::new();

    if !halt_after_post_implement && rate_limited_phase.is_none() {
        // ---- Phase 3: Review ----
        announce(
            log_writer,
            "bellows: phase 3/8 — review (reads diff, produces findings)",
        );
        let review_picked_opt = pick_non_implement_engine(
            &config.phases.review.cli_chain,
            &state,
            implementer_cli,
            engine_label_override,
            "review",
            log_writer,
        );
        if review_picked_opt.is_none() {
            rate_limited_phase = Some("review");
        }
        let review_picked = review_picked_opt.unwrap_or(PickedEntry {
            entry: ChainEntry { engine: Engine::Claude, model: None },
            reason: PickReason::ChainFirstHotEntry,
        });
        let review_chain_entry = review_picked.entry.clone();
        let review_auth = auth_for(&review_chain_entry);
        let review_agent_run = if rate_limited_phase.is_some() {
            // Picker terminated; skip the agent invocation.
            sandbox::AgentRun {
                exit_code: 0,
                stderr_tail: String::new(),
                killed_by_deadline: false,
            }
        } else {
            workspace::generate_diff(&workspace, policy::REVIEW_DIFF_FILE).await?;
            workspace::generate_commit_log(&workspace, policy::REVIEW_COMMIT_LOG_FILE).await?;
            let review_kickoff = policy::wrap_phase_prompt_for_engine(
                review_chain_entry.engine,
                policy::REVIEW_PROMPT,
            );
            tokio::fs::write(
                workspace.path().join(".bellows-kickoff.md"),
                review_kickoff,
            )
            .await?;
            let run = sandbox::run_agent(
                &workspace,
                &review_auth,
                claimed.number,
                &repo_label,
                &repo_slug,
                &ssh_keys_volume,
                &deploy_keys,
                log_writer,
                budget.deadline_or_halt(),
            )
            .await?;
            budget.mark_killed_if(run.killed_by_deadline);
            if process_non_implement_rate_limit(
                &mut state,
                &state_path,
                review_chain_entry.engine,
                "review",
                &run,
                log_writer,
            ) {
                rate_limited_phase = Some("review");
            }
            run
        };

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

            // Per-phase engine + auth (issue #81 / ADR-0005). The
            // review-fix phase has its own `[phases.review_fix]
            // cli_chain`; the per-finding loop and the nit-batch both
            // dispatch to the same engine for the phase.
            announce(
                log_writer,
                &format!(
                    "bellows: phase 4/8 — review-fix ({} blocker/important per-finding invocation(s) + {} batched nit(s))",
                    urgent_findings.len(),
                    nit_findings.len(),
                ),
            );
            let review_fix_picked_opt = pick_non_implement_engine(
                &config.phases.review_fix.cli_chain,
                &state,
                implementer_cli,
                engine_label_override,
                "review-fix",
                log_writer,
            );
            if review_fix_picked_opt.is_none() {
                rate_limited_phase = Some("review-fix");
            }
            let review_fix_picked = review_fix_picked_opt.unwrap_or(PickedEntry {
                entry: ChainEntry { engine: Engine::Claude, model: None },
                reason: PickReason::ChainFirstHotEntry,
            });
            let review_fix_chain_entry = review_fix_picked.entry.clone();
            let review_fix_auth = auth_for(&review_fix_chain_entry);

            // Per-finding loop: one container per blocker/important
            // finding. Each invocation respects the remaining wall-
            // clock budget; one slow finding cannot blow the cap
            // without halting subsequent ones.
            let mut coverage: Vec<policy::FindingCoverage> = Vec::new();
            let mut review_fix_exit: i64 = 0;
            for (idx, finding) in urgent_findings.iter().enumerate() {
                if rate_limited_phase.is_some() {
                    // Earlier per-finding invocation rate-limited; skip
                    // the rest so they don't burn additional API calls.
                    coverage.push(policy::FindingCoverage {
                        finding: finding.clone(),
                        commit_landed: false,
                    });
                    continue;
                }
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

                let kickoff = policy::wrap_phase_prompt_for_engine(
                    review_fix_chain_entry.engine,
                    &policy::per_finding_kickoff(
                        finding,
                        policy::REVIEW_DIFF_FILE,
                        "agent-notes.md",
                    ),
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
                    &review_fix_auth,
                    claimed.number,
                    &repo_label,
                    &repo_slug,
                    &ssh_keys_volume,
                    &deploy_keys,
                    log_writer,
                    budget.deadline_or_halt(),
                )
                .await?;
                budget.mark_killed_if(per_finding_run.killed_by_deadline);
                if process_non_implement_rate_limit(
                    &mut state,
                    &state_path,
                    review_fix_chain_entry.engine,
                    "review-fix",
                    &per_finding_run,
                    log_writer,
                ) {
                    rate_limited_phase = Some("review-fix");
                }
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
            if !nit_findings.is_empty() && !budget.exceeded && rate_limited_phase.is_none() {
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
                let nit_kickoff = policy::wrap_phase_prompt_for_engine(
                    review_fix_chain_entry.engine,
                    &nit_kickoff,
                );
                tokio::fs::write(workspace.path().join(".bellows-kickoff.md"), &nit_kickoff)
                    .await?;
                let head_before = workspace::head_sha(&workspace).await?;
                let nit_batch_run = sandbox::run_agent(
                    &workspace,
                    &review_fix_auth,
                    claimed.number,
                    &repo_label,
                    &repo_slug,
                    &ssh_keys_volume,
                    &deploy_keys,
                    log_writer,
                    budget.deadline_or_halt(),
                )
                .await?;
                budget.mark_killed_if(nit_batch_run.killed_by_deadline);
                if process_non_implement_rate_limit(
                    &mut state,
                    &state_path,
                    review_fix_chain_entry.engine,
                    "review-fix",
                    &nit_batch_run,
                    log_writer,
                ) {
                    rate_limited_phase = Some("review-fix");
                }
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
                let synth_span = policy::append_bellows_synth_entry(
                    &mut new_notes,
                    &synth,
                    policy::BellowsSynthCause::ParserBackstop,
                );
                agent_note_synth_spans.push(synth_span);
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

        // ---- Phase 5 (slice X2): Security-review ----
        //
        // Mirrors the review/review-fix pair but with a tighter focus on
        // the five security categories (input validation, auth, crypto,
        // injection, data exposure). Regenerates the diff file from the
        // POST-review-fix workspace state so the security agent sees the
        // delta including review fixups, not the original implement
        // diff. The diff is regenerated here even when no review-fix
        // ran (no findings path) because the diff file from the review
        // phase may have already been removed by the prior cleanup step,
        // and to keep the file content deterministic at this phase
        // boundary either way.
        //
        // Halt-on-phase-failure: skip security-review (and therefore
        // security-fix and the end-pipeline gate) when an earlier phase
        // halted or the wall-clock budget is spent — same plumbing as
        // X1's review halt.
        let mut halt_after_security = false;
        let mut halt_after_security_fix = false;
        if !halt_after_review && !halt_after_fix && !budget.exceeded && rate_limited_phase.is_none()
        {
            announce(
                log_writer,
                "bellows: phase 5/8 — security-review (reads diff for the five security focus categories, produces findings)",
            );
            let security_review_picked_opt = pick_non_implement_engine(
                &config.phases.security_review.cli_chain,
                &state,
                implementer_cli,
                engine_label_override,
                "security-review",
                log_writer,
            );
            if security_review_picked_opt.is_none() {
                rate_limited_phase = Some("security-review");
            }
            let security_review_picked = security_review_picked_opt.unwrap_or(PickedEntry {
                entry: ChainEntry { engine: Engine::Claude, model: None },
                reason: PickReason::ChainFirstHotEntry,
            });
            let security_review_chain_entry = security_review_picked.entry.clone();
            let security_review_auth = auth_for(&security_review_chain_entry);
            let security_agent_run = if rate_limited_phase.is_some() {
                sandbox::AgentRun {
                    exit_code: 0,
                    stderr_tail: String::new(),
                    killed_by_deadline: false,
                }
            } else {
                workspace::generate_diff(&workspace, policy::REVIEW_DIFF_FILE).await?;
                let security_kickoff = policy::wrap_phase_prompt_for_engine(
                    security_review_chain_entry.engine,
                    policy::SECURITY_REVIEW_PROMPT,
                );
                tokio::fs::write(
                    workspace.path().join(".bellows-kickoff.md"),
                    security_kickoff,
                )
                .await?;
                let run = sandbox::run_agent(
                    &workspace,
                    &security_review_auth,
                    claimed.number,
                    &repo_label,
                    &repo_slug,
                    &ssh_keys_volume,
                    &deploy_keys,
                    log_writer,
                    budget.deadline_or_halt(),
                )
                .await?;
                budget.mark_killed_if(run.killed_by_deadline);
                if process_non_implement_rate_limit(
                    &mut state,
                    &state_path,
                    security_review_chain_entry.engine,
                    "security-review",
                    &run,
                    log_writer,
                ) {
                    rate_limited_phase = Some("security-review");
                }
                run
            };

            // Read the security findings file. Don't remove it yet —
            // security-fix may need to read it. The security-fix phase
            // is expected to remove the file when all findings are
            // addressed; defensive cleanup at the end of the pipeline
            // catches any leftover.
            let security_findings_path = workspace.path().join(policy::SECURITY_FINDINGS_FILE);
            let security_findings_text = if security_findings_path.exists() {
                let raw = tokio::fs::read_to_string(&security_findings_path).await?;
                let trimmed = raw.trim();
                if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("(no findings)") {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            } else {
                None
            };
            let has_security_findings = security_findings_text.is_some();
            security_outcome = Some(AnalysisOutcome {
                findings_text: security_findings_text,
                exit_code: security_agent_run.exit_code,
            });

            halt_after_security = security_agent_run.exit_code != 0;

            announce(
                log_writer,
                &format!(
                    "bellows: security-review done (findings: {})",
                    if has_security_findings { "yes" } else { "none" },
                ),
            );

            // ---- Phase 6 (slice X2): Security-fix ----
            //
            // Only runs when security-review exited cleanly AND produced
            // findings AND the wall-clock budget allows. The fix prompt
            // is a single batch invocation (mirrors the brief's "read
            // findings, address each, commit each fix, remove the
            // findings file" shape — not the slice-9.6 per-finding loop;
            // security fixups are scoped tightly enough that the
            // per-finding overhead would dwarf the work).
            if !halt_after_security
                && has_security_findings
                && !budget.exceeded
                && rate_limited_phase.is_none()
            {
                announce(
                    log_writer,
                    "bellows: phase 6/8 — security-fix (reads findings and addresses each)",
                );
                let security_fix_picked_opt = pick_non_implement_engine(
                    &config.phases.security_fix.cli_chain,
                    &state,
                    implementer_cli,
                    engine_label_override,
                    "security-fix",
                    log_writer,
                );
                if security_fix_picked_opt.is_none() {
                    rate_limited_phase = Some("security-fix");
                }
                let security_fix_picked = security_fix_picked_opt.unwrap_or(PickedEntry {
                    entry: ChainEntry { engine: Engine::Claude, model: None },
                    reason: PickReason::ChainFirstHotEntry,
                });
                let security_fix_chain_entry = security_fix_picked.entry.clone();
                let security_fix_auth = auth_for(&security_fix_chain_entry);
                let security_fix_run = if rate_limited_phase.is_some() {
                    sandbox::AgentRun {
                        exit_code: 0,
                        stderr_tail: String::new(),
                        killed_by_deadline: false,
                    }
                } else {
                    let security_fix_kickoff = policy::wrap_phase_prompt_for_engine(
                        security_fix_chain_entry.engine,
                        policy::SECURITY_FIX_PROMPT,
                    );
                    tokio::fs::write(
                        workspace.path().join(".bellows-kickoff.md"),
                        security_fix_kickoff,
                    )
                    .await?;
                    let head_before = workspace::head_sha(&workspace).await?;
                    let run = sandbox::run_agent(
                        &workspace,
                        &security_fix_auth,
                        claimed.number,
                        &repo_label,
                        &repo_slug,
                        &ssh_keys_volume,
                        &deploy_keys,
                        log_writer,
                        budget.deadline_or_halt(),
                    )
                    .await?;
                    budget.mark_killed_if(run.killed_by_deadline);
                    if process_non_implement_rate_limit(
                        &mut state,
                        &state_path,
                        security_fix_chain_entry.engine,
                        "security-fix",
                        &run,
                        log_writer,
                    ) {
                        rate_limited_phase = Some("security-fix");
                    }
                    // Same agent-self-commit-tolerant push plumbing as
                    // the per-finding/nit-batch sites. If the agent
                    // self-committed each fix, bellows's commit_all
                    // sees nothing to stage (NoChangesToCommit) but
                    // HEAD has advanced — commit_all_and_push_if_advanced
                    // pushes either shape.
                    let _ =
                        workspace::commit_all_and_push_if_advanced(&workspace, &head_before).await?;
                    run
                };
                let security_fix_exit = security_fix_run.exit_code;
                security_fix_outcome = Some(FixOutcome {
                    exit_code: security_fix_exit,
                });
                halt_after_security_fix = security_fix_exit != 0;
                announce(
                    log_writer,
                    &format!("bellows: security-fix done (exit {})", security_fix_exit),
                );
            }
        }

        // ---- Phase 7 (slice X2): End-of-pipeline cargo checks gate ----
        //
        // The existing slice-X1 end-gate runs at end-of-pipeline; per
        // the brief, no new gate is needed for slice X2 since this
        // existing one catches regressions introduced by the security
        // fixups too. We gate it on every halt flag (including the new
        // security halts) so a failed security phase short-circuits
        // cleanly without burning a cargo run.
        if !halt_after_review
            && !halt_after_fix
            && !halt_after_security
            && !halt_after_security_fix
            && !budget.exceeded
            && workspace.path().join("Cargo.toml").exists()
        {
            announce(
                log_writer,
                "bellows: phase 7/8 — end-of-pipeline cargo checks gate (clippy + test after fixups)",
            );
            for line in workspace.gate_commands().announcement_lines() {
                announce(log_writer, &line);
            }
            let run = sandbox::run_cargo_checks(
                &workspace,
                claimed.number,
                &repo_label,
                &repo_slug,
                &ssh_keys_volume,
                &deploy_keys,
                log_writer,
                budget.deadline_or_halt(),
            )
            .await?;
            budget.mark_killed_if(run.killed_by_deadline);
            end_pipeline_gate = Some(run.gate);
        }

        // ---- Phase 8 (issue #123 / ADR-0009 slice 1): Merger ----
        //
        // Read-only end-of-pipeline judgement. Reads the squashed
        // diff, the brief's verbatim ACs (carried in the kickoff
        // prompt), the final `agent-notes.md` content, and the
        // cargo-checks gate status; writes its prose review to
        // `MERGER_OUTPUT_FILE` ending with a `VERDICT: <token>` line.
        // Bellows parses the verdict and stores it in `PhaseOutcomes`.
        //
        // Routing in this slice is identical to today — the verdict
        // is logged but does NOT yet feed `classify_exit`. Slice 2
        // (#124) will wire routing.
        //
        // Gated on every halt flag (mirrors the phase-7 gate) so a
        // failed earlier phase short-circuits cleanly without burning
        // a merger run.
        if !halt_after_review
            && !halt_after_fix
            && !halt_after_security
            && !halt_after_security_fix
            && !budget.exceeded
            && rate_limited_phase.is_none()
        {
            announce(
                log_writer,
                "bellows: phase 8/8 — merger (read-only end-of-pipeline verdict on the diff vs ACs)",
            );
            let merger_picked_opt = pick_non_implement_engine(
                &config.phases.merge.cli_chain,
                &state,
                implementer_cli,
                engine_label_override,
                "merger",
                log_writer,
            );
            if merger_picked_opt.is_none() {
                rate_limited_phase = Some("merger");
            }
            let merger_picked = merger_picked_opt.unwrap_or(PickedEntry {
                entry: ChainEntry { engine: Engine::Claude, model: None },
                reason: PickReason::ChainFirstHotEntry,
            });
            let merger_chain_entry = merger_picked.entry.clone();
            let merger_auth = auth_for(&merger_chain_entry);
            if rate_limited_phase.is_none() {
                workspace::generate_diff(&workspace, policy::REVIEW_DIFF_FILE).await?;
                let merger_kickoff = render_merger_kickoff_for_engine(
                    merger_chain_entry.engine,
                    &brief,
                    &end_pipeline_gate,
                );
                tokio::fs::write(
                    workspace.path().join(".bellows-kickoff.md"),
                    merger_kickoff,
                )
                .await?;
                let run = sandbox::run_agent(
                    &workspace,
                    &merger_auth,
                    claimed.number,
                    &repo_label,
                    &repo_slug,
                    &ssh_keys_volume,
                    &deploy_keys,
                    log_writer,
                    budget.deadline_or_halt(),
                )
                .await?;
                budget.mark_killed_if(run.killed_by_deadline);
                if process_non_implement_rate_limit(
                    &mut state,
                    &state_path,
                    merger_chain_entry.engine,
                    "merger",
                    &run,
                    log_writer,
                ) {
                    rate_limited_phase = Some("merger");
                }

                // Parse the verdict from the merger's output file.
                // Missing file / unrecognised / ambiguous → None,
                // logged and stored (does NOT yet feed classify_exit).
                let merger_output_path = workspace.path().join(policy::MERGER_OUTPUT_FILE);
                let merger_output_text = match tokio::fs::read_to_string(&merger_output_path).await
                {
                    Ok(s) => Some(s),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => return Err(e.into()),
                };
                merger_verdict = merger_output_text
                    .as_deref()
                    .and_then(policy::parse_merger_verdict);
                match merger_verdict {
                    Some(v) => announce(
                        log_writer,
                        &format!("bellows: merger verdict: {}", v.as_token()),
                    ),
                    None => announce(
                        log_writer,
                        "bellows: merger produced no parseable verdict (logged; routing unchanged)",
                    ),
                }
            }
        }

        // Defensive cleanup: even on halt paths the diff file should not
        // outlive the run. Findings file is also ensured-removed so it
        // never appears in any subsequent commit.
        cleanup_phase_handoff_files(&workspace).await?;
    }

    // Issue #85: capture `agent-notes.md` content ONCE at the very end,
    // then remove the file + commit + push the deletion. Any phase may
    // have written/appended to it (implement, weak-test guard, review-fix,
    // security-fix, parser-as-backstop synth) and the per-phase commits
    // would have already pushed the file onto the agent branch — we
    // explicitly commit the deletion here so the agent branch's pushed
    // end-state has no `agent-notes.md`. A subsequent squash-merge to
    // `master` therefore cannot inherit stale notes into the next run's
    // fresh clone. The captured content still drives
    // `classify_exit`'s `has_agent_notes` precedence below (unchanged)
    // and is surfaced to the operator via a separate PR comment posted
    // after `open_pr` (replaces the pre-#85 affordance of the file
    // sitting in the PR's diff).
    let agent_notes = capture_and_remove_agent_notes(&workspace).await?;
    let agent_notes_display = agent_notes
        .as_deref()
        .map(str::trim)
        .filter(|notes| !notes.is_empty())
        .map(str::to_string);
    let head_before_ephemeral_cleanup = workspace::head_sha(&workspace).await?;
    let _ = workspace::commit_all_and_push_if_advanced(
        &workspace,
        &head_before_ephemeral_cleanup,
    )
    .await?;

    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: implement_agent_run.exit_code,
            stderr_tail: implement_agent_run.stderr_tail.clone(),
            engine: Some(implement_chain_entry.engine),
        },
        post_implement_gate,
        review: review_outcome,
        review_fix: review_fix_outcome,
        security: security_outcome,
        security_fix: security_fix_outcome,
        end_pipeline_gate,
        wall_clock_exceeded: budget.exceeded,
        backstop_violations,
        implement_crash_synthesised,
        // Issue #123 / ADR-0009 slice 1: phase-8 verdict populated by
        // the merger phase above when it ran; `None` when phase 8 was
        // halted or its output was unparseable.
        merger_verdict,
    };
    // ADR-0006 / issue #95: feed agent-notes content plus the
    // out-of-band Bellows synth spans through note classification.
    // HTML comments in the workspace file are human-readable only;
    // routing strips only spans recorded at Bellows append sites.
    //
    // ADR-0009 slice 2 / issue #124: thread the phase-8 merger
    // verdict captured above (None if phase 8 didn't run or its
    // output was unparseable) into the classifier so it can drive
    // the (α) agent-authored routing branch. Q4-Option-A: None
    // falls back to the pre-slice classifier behaviour.
    let pipeline_reason = policy::classify_exit(
        policy::classify_agent_notes_with_synth_spans(
            agent_notes.as_deref(),
            &agent_note_synth_spans,
        ),
        &outcomes,
        outcomes.merger_verdict,
    );

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

    // Issue #82: a non-implement-phase rate-limit (or implement-phase
    // rate-limit that terminates rather than in-place-advances) sets
    // `rate_limited_phase`. The existing `classify_exit` already
    // recognises implement-phase rate-limit signatures; the override
    // catches non-implement phases (review / review-fix /
    // security-review / security-fix) whose stderr_tail does not flow
    // into `PhaseOutcomes::implement`.
    let reason = if externally_cancelled {
        ExitReason::Cancelled
    } else if rate_limited_phase.is_some() {
        ExitReason::RateLimited
    } else {
        pipeline_reason
    };

    let routing = pr_routing_for_reason(
        &reason,
        &config.runtime_labels,
        workspace.auto_merge_workflow_supports_agent_noted_filter(),
    );
    let draft = routing.draft;
    let outcome_label = routing.outcome_label;
    if let Some(line) = routing.fallback_announcement {
        announce(log_writer, line);
    }

    announce(
        log_writer,
        &format!(
            "bellows: classified as {:?} — opening {} PR",
            reason,
            if draft { "draft" } else { "ready-for-review" },
        ),
    );

    // Issue #111: surface workflow-file changes on both the PR body
    // and the run-log comment when the agent's branch diff against
    // the default branch touches any file under `.github/workflows/`.
    // Best-effort: a git failure here must not block PR open — the
    // operator-visibility callout is informational, not gating. We
    // emit a log line and fall back to an empty list (no callout).
    let workflow_files_changed = match workspace::workflow_files_changed_between(
        &workspace,
        workspace.default_branch(),
        "HEAD",
    )
    .await
    {
        Ok(files) => files,
        Err(e) => {
            let _ = writeln!(
                log_writer,
                "bellows: workflow-file diff failed (continuing without callout): {e}",
            );
            Vec::new()
        }
    };

    let pr_title = format!("Bellows agent run for issue #{}", claimed.number);
    let pr_body = build_pr_body(
        &reason,
        claimed.number,
        claude_pr_body.as_deref(),
        agent_notes_display.as_deref(),
        &workflow_files_changed,
        &outcomes,
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
    if let Some(pr_label) = routing.pr_label {
        tracker::add_issue_labels(client, &owner, &repo, pr.number, &[pr_label]).await?;
    }
    announce(
        log_writer,
        &format!("bellows: opened PR #{} — finalising labels + log comment", pr.number),
    );

    // Issue #85: post the agent's self-flag content as a separate
    // `## Agent notes` PR comment. Replaces the pre-#85 affordance of
    // the operator seeing the file inline in the PR diff — the file is
    // now ephemeral to the run, so we surface the content here so the
    // operator can still see what the agent flagged. No-op when the
    // agent didn't write any notes.
    post_agent_notes_comment_if_present(
        client,
        &owner,
        &repo,
        pr.number,
        agent_notes_display.as_deref(),
    )
    .await?;

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

    // Slice X2: post the security findings as a separate `## Security
    // findings` PR comment if the security-review phase produced any.
    // Posted regardless of whether security-fix succeeded so the reader
    // always sees what was flagged, mirroring the review-findings post
    // immediately above.
    if let Some(security_findings) = outcomes
        .security
        .as_ref()
        .and_then(|r| r.findings_text.as_deref())
    {
        let comment_body = format!("## Security findings\n\n{security_findings}");
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
        &workflow_files_changed,
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

    // Issue #87: the comment POST inside `finalise` is observability-
    // only — the label transition has already completed by the time we
    // get here regardless of whether the run-log comment landed. Surface
    // a failure as an operator-visible warning on both `bellows.log` and
    // the polling-loop's stdout, without collapsing the run.
    if let Some(msg) = finalise_outcome.comment_post_failure.as_deref() {
        let line = format!(
            "bellows: posting the run-log comment on PR #{} failed (label \
             transition completed; continuing): {}",
            pr.number, msg,
        );
        println!("{line}");
        let _ = writeln!(log_writer, "{line}");
    }

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

/// Issue #111: build the labelled callout that flags workflow-file
/// changes on the agent PR. Returns an empty string when
/// `workflow_files_changed` is empty (no callout); otherwise a
/// markdown section that names the changed file(s) and explains the
/// gate-vs-CI gap (bellows's cargo gates only mirror `cargo clippy`
/// and `cargo test`, so any other new steps execute for the first
/// time on the PR's real GitHub Actions run).
///
/// Shared between `build_pr_body` and `build_log_body` so the two
/// surfaces emit equivalent callouts — drift between them is the
/// regression the parametrised test exists to prevent.
fn workflow_files_changed_callout(workflow_files_changed: &[String]) -> String {
    if workflow_files_changed.is_empty() {
        return String::new();
    }
    let file_list = workflow_files_changed
        .iter()
        .map(|p| format!("`{p}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "\n### CI workflow files changed in this PR\n\n\
         {file_list} — bellows's cargo gates only mirror `cargo clippy` and \
         `cargo test` from CI, so any other new steps will execute for the first \
         time on the PR's real GitHub Actions run.\n"
    )
}

/// Render the PR-body callout for `ExitReason::AuthError`, naming
/// the specific engine to refresh based on
/// `outcomes.implement.engine`. AC12 of issue #120: the operator
/// must be able to copy-paste the suggested `bellows refresh-auth`
/// command straight from the PR body. When the implement-phase
/// engine is unknown (legacy / pre-AC12 runs with `engine: None`),
/// fall back to the generic `<engine>` placeholder.
pub fn pr_body_for_auth_error(outcomes: &PhaseOutcomes) -> String {
    let engine_name = match outcomes.implement.engine {
        Some(engine) => engine.as_name().to_string(),
        None => "<engine>".to_string(),
    };
    format!(
        "## Authentication error detected\n\n\
         An implement-phase agent reported an authentication failure (e.g. HTTP 401 from the engine's API). Run `bellows refresh-auth --engine {engine_name}` and re-run. See the run-log comment for the matched signature."
    )
}

fn build_pr_body(
    reason: &ExitReason,
    issue_number: u64,
    claude_pr_body: Option<&str>,
    agent_notes: Option<&str>,
    workflow_files_changed: &[String],
    outcomes: &PhaseOutcomes,
) -> String {
    let header = format!("Closes #{issue_number}.\n\n");
    let body = match reason {
        ExitReason::Success => claude_pr_body
            .map(str::to_string)
            .unwrap_or_else(|| {
                "_(Run produced by Bellows v1; the agent did not write a PR description.)_"
                    .to_string()
            }),
        ExitReason::SuccessWithNotes => {
            let generated = claude_pr_body.unwrap_or(
                "_(Run produced by Bellows v1; the agent did not write a PR description.)_",
            );
            format!(
                "## Agent completed with notes\n\n\
                 The agent completed successfully and wrote an informational `agent-notes.md` note. The file itself is ephemeral to the run (issue #85); its content is posted as a separate `## Agent notes` PR comment for the operator to read before merging.\n\n\
                 {generated}"
            )
        }
        ExitReason::AgentSelfReportedFailure => format!(
            "## Agent self-reported failure\n\n\
             The agent wrote `agent-notes.md` rather than complete the brief. The file itself is ephemeral to the run (issue #85) and does not appear in this PR's diff; its content is posted as a separate `## Agent notes` PR comment for visibility and quoted below for convenience.\n\n\
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
        ExitReason::AuthError => pr_body_for_auth_error(outcomes),
        ExitReason::Cancelled => {
            "## Cancelled by operator\n\n\
             `bellows kill` was invoked against this issue mid-run. Whatever workspace state the agent had produced before cancellation is committed in this PR's diff; the run-log comment captures the per-phase summary at cancellation time. Review the partial work and either salvage it as a starting point or drop the PR."
                .to_string()
        }
    };
    // Issue #111: append the workflow-file change callout when the
    // agent's branch diff touched a file under `.github/workflows/`.
    // The helper returns an empty string for the empty list, so the
    // common case is a no-op and there is no whitespace noise.
    header + &body + &workflow_files_changed_callout(workflow_files_changed)
}

fn build_log_body(
    reason: &ExitReason,
    issue_number: u64,
    started: chrono::DateTime<chrono::Utc>,
    finished: chrono::DateTime<chrono::Utc>,
    branch_name: &str,
    outcomes: &PhaseOutcomes,
    workflow_files_changed: &[String],
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
         - Security: {security}\n\
         - Security-fix: {security_fix}\n\
         - Cargo checks (end-pipeline): {end_gate}\n",
        started_rfc = started.to_rfc3339(),
        finished_rfc = finished.to_rfc3339(),
        agent_exit = outcomes.implement.exit_code,
        post_gate = gate_summary_line(&outcomes.post_implement_gate),
        review = review_summary(&outcomes.review),
        review_fix = review_fix_summary(&outcomes.review_fix),
        security = analysis_summary(&outcomes.security),
        security_fix = review_fix_summary(&outcomes.security_fix),
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
    //
    // Issue #81 / ADR-0005: the callout names the engine to refresh.
    // The codex composite (`401 Unauthorized` AND `Missing bearer or
    // basic authentication`) is the more specific signature; check it
    // first so a stderr that matches both substrings is attributed to
    // codex rather than misattributed to claude on the bare `401
    // Unauthorized` substring.
    if outcomes.implement.exit_code != 0
        && policy::is_codex_auth_error_signature(&outcomes.implement.stderr_tail)
    {
        body.push_str(
            "\n### Authentication error detected in codex stderr\n\n\
             A codex phase exited non-zero with stderr matching the codex auth-error \
             signature (`401 Unauthorized` plus `Missing bearer or basic authentication`). \
             Run `bellows refresh-auth --engine codex` to re-authenticate, then re-label \
             the issue to retry. The agent output tail below contains the matched line.\n",
        );
    } else if outcomes.implement.exit_code != 0
        && policy::is_claude_auth_error_signature(&outcomes.implement.stderr_tail)
    {
        body.push_str(
            "\n### Authentication error detected in claude stderr\n\n\
             A claude phase exited non-zero with stderr matching a known claude auth-error \
             signature (e.g. an expired OAuth refresh token). Run `bellows refresh-auth \
             --engine claude` to re-authenticate, then re-label the issue to retry. The \
             agent output tail below contains the matched line.\n",
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

    // Issue #111: surface workflow-file changes so the operator
    // reviewing the run-log gets a signal that CI's shape has
    // changed. Pure annotation — bellows does NOT execute the new
    // step inside the sandbox. The shared helper returns an empty
    // string for the empty list, so the common case is a no-op.
    body.push_str(&workflow_files_changed_callout(workflow_files_changed));

    if !matches!(reason, ExitReason::Success | ExitReason::SuccessWithNotes) {
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
    // Issue #87: bound the body below GitHub's 64 KiB comment limit
    // (the helper passes short bodies through unchanged so the common
    // path is unaffected).
    tracker::truncate_for_github_comment(&body)
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

fn render_merger_kickoff_for_engine(
    engine: Engine,
    brief: &str,
    end_pipeline_gate: &Option<GateOutcome>,
) -> String {
    let mut body = policy::render_merger_prompt();
    body.push_str("\n\n## Bellows-supplied run inputs\n\n");
    body.push_str(
        "The runner injects these values directly into the kickoff because the merger \
         sandbox only has `/workspace` mounted and cannot read the host run log.\n\n",
    );
    body.push_str("### Agent brief\n\n");
    body.push_str("```markdown\n");
    body.push_str(brief.trim_end());
    body.push_str("\n```\n\n");
    body.push_str("### End-pipeline cargo-checks gate\n\n");
    body.push_str("```text\n");
    body.push_str(&merger_gate_summary(end_pipeline_gate));
    body.push_str("```\n");
    policy::wrap_phase_prompt_for_engine(engine, &body)
}

fn merger_gate_summary(end_pipeline_gate: &Option<GateOutcome>) -> String {
    let mut out = String::new();
    out.push_str("end_pipeline_gate:\n");
    match end_pipeline_gate {
        Some(gate) => {
            out.push_str("  status: ran\n");
            push_merger_check_summary(&mut out, "cargo_clippy", &gate.cargo_clippy);
            push_merger_check_summary(&mut out, "cargo_test", &gate.cargo_test);
        }
        None => {
            out.push_str("  status: did-not-run\n");
            push_merger_check_summary(&mut out, "cargo_clippy", &None);
            push_merger_check_summary(&mut out, "cargo_test", &None);
        }
    }
    out
}

fn push_merger_check_summary(out: &mut String, name: &str, check: &Option<CheckResult>) {
    out.push_str(&format!("  {name}:\n"));
    match check {
        Some(result) if result.exit_code == 0 => {
            out.push_str("    status: passed\n");
            out.push_str(&format!("    exit_code: {}\n", result.exit_code));
        }
        Some(result) => {
            out.push_str("    status: failed\n");
            out.push_str(&format!("    exit_code: {}\n", result.exit_code));
        }
        None => {
            out.push_str("    status: did-not-run\n");
            out.push_str("    exit_code: n/a\n");
        }
    }
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

/// Slice X2: sibling of `review_summary` for the security-review phase
/// (and any future analysis-shaped phase that produces findings).
/// Identical semantics to `review_summary` — kept as a separate
/// function because `AnalysisOutcome` and `ReviewOutcome` are distinct
/// types, so a single generic helper would need a trait + impls for
/// no payoff at this scale.
fn analysis_summary(analysis: &Option<AnalysisOutcome>) -> String {
    match analysis {
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

/// Issue #82: path to the per-engine rate-limit state file, written
/// alongside `bellows.log` so the operator finds both in the same
/// directory. ADR-0005: "written alongside `bellows.log` in the
/// operator's bellows working directory; same parent path as the
/// existing log; same lifecycle owner."
pub fn state_file_path_alongside_log(log_path: &std::path::Path) -> std::path::PathBuf {
    log_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join("bellows-state.json"))
        .unwrap_or_else(|| std::path::PathBuf::from("bellows-state.json"))
}

/// Pick the engine for a non-implement phase and emit the run-log
/// line. Returns `Some(PickedEntry)` on success, `None` when every
/// chain entry is cooling (caller short-circuits to RateLimited).
/// Surfaces the diversity-collapse warning when pass-2 of the picker
/// fired so the operator can see why the implementer-CLI ran review.
fn pick_non_implement_engine(
    chain: &[ChainEntry],
    state: &StateFile,
    implementer: Option<Engine>,
    forced: Option<Engine>,
    phase_name: &'static str,
    log_writer: &mut dyn Write,
) -> Option<PickedEntry> {
    let now = chrono::Utc::now();
    match pick_engine_for_phase(chain, state, implementer, forced, now) {
        Ok(p) => {
            if forced.is_some() {
                announce(
                    log_writer,
                    &format!(
                        "bellows: engine forced via engine:{} label; chain walking skipped",
                        p.entry.engine.as_name(),
                    ),
                );
            } else if p.reason == PickReason::SecondPassAfterCollapse {
                announce(
                    log_writer,
                    &format!(
                        "bellows: diversity preference collapsed for phase `{phase_name}` — every hot chain entry matches the implementer-CLI; proceeding with the same engine",
                    ),
                );
            }
            announce(
                log_writer,
                &format_phase_engine_log(phase_name, &p.entry, p.reason),
            );
            Some(p)
        }
        Err(PickError::AllCooling) => {
            announce(
                log_writer,
                &format!(
                    "bellows: every `{phase_name}` chain entry is cooling per bellows-state.json; terminating as RateLimited",
                ),
            );
            None
        }
    }
}

/// Record a non-implement-phase rate-limit signature in the state
/// file. Returns `true` when the agent run matched a rate-limit
/// signature (caller halts the rest of the pipeline). The composed
/// helper from `chain_walker` does the state-update + decide; this
/// runner-side wrapper adds the log lines.
fn process_non_implement_rate_limit(
    state: &mut StateFile,
    state_path: &std::path::Path,
    engine: Engine,
    phase_name: &'static str,
    agent_run: &sandbox::AgentRun,
    log_writer: &mut dyn Write,
) -> bool {
    if agent_run.exit_code == 0 || !policy::is_rate_limit_signature(&agent_run.stderr_tail) {
        return false;
    }
    let now = chrono::Utc::now();
    let parsed = chain_walker::parse_cooling_until(engine, &agent_run.stderr_tail, now);
    if parsed.used_fallback {
        announce(
            log_writer,
            &format!(
                "bellows: rate-limit stderr had no parseable timestamp; using conservative 5-minute fallback cooldown for engine={}",
                engine.as_name(),
            ),
        );
    }
    let disposition =
        handle_non_implement_rate_limit(state, engine, phase_name, &agent_run.stderr_tail, now);
    let _ = state.save(state_path);
    match disposition {
        RateLimitDisposition::Terminate => {
            announce(
                log_writer,
                &format!(
                    "bellows: phase `{phase_name}` rate-limited on engine={}; terminating run as RateLimited",
                    engine.as_name(),
                ),
            );
        }
        RateLimitDisposition::InPlaceAdvance => {
            // Non-implement phases never in-place-advance; defensive.
            announce(
                log_writer,
                &format!(
                    "bellows: phase `{phase_name}` rate-limited on engine={}; terminating run as RateLimited",
                    engine.as_name(),
                ),
            );
        }
    }
    true
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

/// Issue #85: read `agent-notes.md` from the workspace (if present)
/// and remove the file from disk in a single step. Returns the raw
/// content, or `None` if the file did not exist.
///
/// Pipeline phases (implement, weak-test guard, review-fix's per-finding
/// invocations, security-fix, parser-as-backstop synth) write/append
/// to `agent-notes.md` and commit it as part of their per-phase `git
/// add -A`. Pre-#85 the file then rode along on the agent branch into
/// `master` via squash-merge, and every subsequent fresh clone
/// inherited the stale content — `classify_exit`'s `has_agent_notes`
/// precedence then fired regardless of actual outcome.
///
/// The fix: capture the content for `classify_exit` + the operator-
/// visible PR comment in one shot, then remove the file. The caller is
/// expected to follow this with a commit + push (e.g.
/// [`workspace::commit_all_and_push_if_advanced`]) so the deletion lands
/// on the agent branch's pushed tip. The squash-merged commit on
/// `master` then no longer carries the file forward.
pub async fn capture_and_remove_agent_notes(
    workspace: &workspace::Workspace,
) -> Result<Option<String>, std::io::Error> {
    let path = workspace.path().join("agent-notes.md");
    let raw = match tokio::fs::read_to_string(&path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    tokio::fs::remove_file(&path).await?;
    Ok(Some(raw))
}

/// Issue #85: post the agent's self-flag content as a separate
/// `## Agent notes` PR comment. No-op when `agent_notes` is `None`.
///
/// Mirrors the `## Review findings` / `## Security findings` per-phase
/// comment posts in [`run_once`]: a dedicated, clearly-titled comment
/// the operator can find in the PR's conversation tab without having to
/// scan the file diff. Pre-#85 the content sat in the PR's diff via
/// `agent-notes.md` itself; now the file is ephemeral, and this comment
/// is the operator-visible surface that replaces it.
pub async fn post_agent_notes_comment_if_present(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    pr_number: u64,
    agent_notes: Option<&str>,
) -> Result<(), octocrab::Error> {
    let Some(notes) = agent_notes else {
        return Ok(());
    };
    if notes.trim().is_empty() {
        return Ok(());
    }
    let body = format!("## Agent notes\n\n```\n{notes}\n```");
    tracker::post_pr_comment(client, owner, repo, pr_number, &body).await
}

/// Remove the slice-X1 + X2 phase handoff files from the workspace.
/// The review diff (input to the review prompt), the review findings
/// file (output of review, input of review-fix), the review commit
/// log, and the security findings file (output of security-review,
/// input of security-fix) are all Bellows-internal — they must never
/// land in any subsequent commit. Called after review-fix /
/// security-fix and as a defensive sweep on the halt path.
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
        policy::SECURITY_FINDINGS_FILE,
        policy::MERGER_OUTPUT_FILE,
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
    fn auth_for_chain_entry_opencode_uses_env_file_config_and_expands_tilde() {
        let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"

[auth.opencode]
api_key_env_file = "~/bellows-test-opencode.env"
"#;
        let config: Config = config_text.parse().expect("config parses");
        let entry: ChainEntry = "opencode:deepseek/deepseek-v4-pro"
            .parse()
            .expect("opencode chain entry parses");

        let auth = auth_for_chain_entry(&config, &entry);

        let Auth::EnvFile {
            engine,
            model,
            env_file_path,
        } = auth
        else {
            panic!("opencode chain entries must construct Auth::EnvFile");
        };
        assert_eq!(engine, Engine::Opencode);
        assert_eq!(model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(
            env_file_path,
            dirs::home_dir()
                .expect("test environment has a home directory")
                .join("bellows-test-opencode.env"),
        );
    }

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
                engine: None,
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
            merger_verdict: None,
            security: None,
            security_fix: None,
        }
    }

    fn fixed_timestamp() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-05-09T20:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn build_pr_body_for_success_uses_claude_pr_body_when_present() {
        let body = build_pr_body(&ExitReason::Success, 42, Some("My PR body."), None, &[], &PhaseOutcomes::default());
        assert!(body.starts_with("Closes #42.\n\n"));
        assert!(body.contains("My PR body."));
    }

    #[test]
    fn build_pr_body_for_success_uses_boilerplate_when_no_pr_body() {
        let body = build_pr_body(&ExitReason::Success, 42, None, None, &[], &PhaseOutcomes::default());
        assert!(body.contains("the agent did not write a PR description"));
    }

    #[test]
    fn success_with_notes_routes_ready_for_review_when_agent_noted_filter_supported() {
        let labels = crate::config::RuntimeLabelsConfig::default();
        let routing = pr_routing_for_reason(&ExitReason::SuccessWithNotes, &labels, true);

        assert!(!routing.draft, "supported targets should get a ready PR");
        assert_eq!(routing.outcome_label, "agent-noted");
        assert_eq!(routing.pr_label, Some("agent-noted"));
        assert!(
            routing.fallback_announcement.is_none(),
            "supported targets should not announce a draft fallback",
        );
    }

    #[test]
    fn success_with_notes_routes_draft_with_adr_0006_fallback_when_filter_unsupported() {
        let labels = crate::config::RuntimeLabelsConfig::default();
        let routing = pr_routing_for_reason(&ExitReason::SuccessWithNotes, &labels, false);

        assert!(routing.draft, "unsupported targets must fall back to draft");
        assert_eq!(routing.outcome_label, "agent-noted");
        assert_eq!(routing.pr_label, Some("agent-noted"));
        let announcement = routing
            .fallback_announcement
            .expect("unsupported target should announce draft fallback");
        assert!(announcement.contains("ADR-0006"), "{announcement}");
        assert!(announcement.contains("agent-noted"), "{announcement}");
        assert!(announcement.contains("draft"), "{announcement}");
    }

    #[test]
    fn build_pr_body_for_self_reported_failure_quotes_agent_notes() {
        let body = build_pr_body(
            &ExitReason::AgentSelfReportedFailure,
            42,
            None,
            Some("I got stuck on the brief."),
        &[],
            &PhaseOutcomes::default(),
        );
        assert!(body.contains("self-reported failure"));
        assert!(body.contains("I got stuck on the brief."));
    }

    #[test]
    fn build_pr_body_for_crash_mentions_stderr_tail_pointer() {
        let body = build_pr_body(&ExitReason::Crash, 42, None, None, &[], &PhaseOutcomes::default());
        assert!(body.contains("crashed"));
        assert!(body.contains("stderr tail"));
    }

    #[test]
    fn build_pr_body_for_final_tests_red_mentions_cargo_checks_not_just_tests() {
        // After slice X1 the gate runs both clippy and test, in either the
        // post-implement or end-of-pipeline position — the PR body should
        // reflect that, not pin "test" specifically.
        let body = build_pr_body(&ExitReason::FinalTestsRed, 42, None, None, &[], &PhaseOutcomes::default());
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
        &[],
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
        &[],
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
                engine: None,
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
            merger_verdict: None,
            security: None,
            security_fix: None,
        };
        let body = build_log_body(
            &ExitReason::FinalTestsRed,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        &[],
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
            implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new(), engine: None },
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
            merger_verdict: None,
            security: None,
            security_fix: None,
        };
        let body = build_log_body(
            &ExitReason::FinalTestsRed, 42, started, finished, "agent/42-x", &outcomes,
        &[],
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
            implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new(), engine: None },
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
            merger_verdict: None,
            security: None,
            security_fix: None,
        };
        let body = build_log_body(
            &ExitReason::FinalTestsRed, 42, started, finished, "agent/42-x", &outcomes,
        &[],
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
            implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new(), engine: None },
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
            merger_verdict: None,
            security: None,
            security_fix: None,
        };
        let body = build_log_body(
            &ExitReason::Success, 42, started, finished, "agent/42-x", &outcomes,
        &[],
        );
        assert!(body.contains("Review:"));
        assert!(body.contains("Cargo checks (post-implement)"));
        assert!(body.contains("Cargo checks (end-pipeline)"));
    }

    #[test]
    fn render_merger_kickoff_includes_brief_and_end_pipeline_gate_status() {
        let brief = r#"## Agent Brief

**Acceptance criteria:**
- [ ] Behaviour test: preserve the frobnicator output.
"#;
        let end_pipeline_gate = Some(GateOutcome {
            cargo_clippy: Some(CheckResult {
                exit_code: 0,
                output: "clippy ok".to_string(),
            }),
            cargo_test: Some(CheckResult {
                exit_code: 101,
                output: "one regression failed".to_string(),
            }),
        });

        let kickoff =
            render_merger_kickoff_for_engine(Engine::Claude, brief, &end_pipeline_gate);

        assert!(
            kickoff.contains("preserve the frobnicator output"),
            "merger kickoff must embed the fetched agent brief: {kickoff}",
        );
        assert!(
            kickoff.contains("end_pipeline_gate:"),
            "merger kickoff must embed a structured gate summary: {kickoff}",
        );
        assert!(
            kickoff.contains("status: ran"),
            "merger kickoff must say the end-pipeline gate ran: {kickoff}",
        );
        assert!(
            kickoff.contains("cargo_clippy:")
                && kickoff.contains("status: passed")
                && kickoff.contains("exit_code: 0"),
            "merger kickoff must include clippy status and exit code: {kickoff}",
        );
        assert!(
            kickoff.contains("cargo_test:")
                && kickoff.contains("status: failed")
                && kickoff.contains("exit_code: 101"),
            "merger kickoff must include test status and exit code: {kickoff}",
        );
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
            implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new(), engine: None },
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
            merger_verdict: None,
            security: None,
            security_fix: None,
        };
        let body = build_log_body(
            &ExitReason::Crash, 42, started, finished, "agent/42-x", &outcomes,
        &[],
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
                engine: None,
            },
            post_implement_gate: GateOutcome::default(),
            review: None,
            review_fix: None,
            end_pipeline_gate: None,
            wall_clock_exceeded: true,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
            merger_verdict: None,
            security: None,
            security_fix: None,
        };
        let body = build_log_body(
            &ExitReason::WallClockExceeded,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        &[],
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
                engine: None,
            },
            post_implement_gate: GateOutcome::default(),
            review: None,
            review_fix: None,
            end_pipeline_gate: None,
            wall_clock_exceeded: false,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
            merger_verdict: None,
            security: None,
            security_fix: None,
        };
        let body = build_log_body(
            &ExitReason::RateLimited,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        &[],
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
                engine: None,
            },
            post_implement_gate: GateOutcome::default(),
            review: None,
            review_fix: None,
            end_pipeline_gate: None,
            wall_clock_exceeded: false,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
            merger_verdict: None,
            security: None,
            security_fix: None,
        };
        let body = build_log_body(
            &ExitReason::Crash,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        &[],
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
                engine: None,
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
            merger_verdict: None,
            security: None,
            security_fix: None,
        };
        let body = build_log_body(
            &ExitReason::Success,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        &[],
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
                engine: None,
            },
            post_implement_gate: GateOutcome::default(),
            review: None,
            review_fix: None,
            end_pipeline_gate: None,
            wall_clock_exceeded: true,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
            merger_verdict: None,
            security: None,
            security_fix: None,
        };
        let body = build_log_body(
            &ExitReason::WallClockExceeded,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        &[],
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
            implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new(), engine: None },
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
            merger_verdict: None,
            security: None,
            security_fix: None,
        };
        let body = build_log_body(
            &ExitReason::AgentSelfReportedFailure,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        &[],
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
            implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new(), engine: None },
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
            merger_verdict: None,
            security: None,
            security_fix: None,
        };
        let body = build_log_body(
            &ExitReason::Success, 42, started, finished, "agent/42-x", &outcomes,
        &[],
        );
        assert!(
            !body.contains("Address-or-explain contract violated"),
            "log body must NOT include the violation callout when no violations occurred: {body}"
        );
    }

    #[test]
    fn build_pr_body_for_wall_clock_exceeded_mentions_cap() {
        let body = build_pr_body(&ExitReason::WallClockExceeded, 42, None, None, &[], &PhaseOutcomes::default());
        assert!(body.to_lowercase().contains("wall-clock"));
        assert!(body.contains("run-log comment"));
    }

    #[test]
    fn build_pr_body_for_rate_limited_mentions_rate_limit() {
        let body = build_pr_body(&ExitReason::RateLimited, 42, None, None, &[], &PhaseOutcomes::default());
        assert!(body.to_lowercase().contains("rate limit"));
        assert!(body.contains("run-log comment"));
    }

    #[test]
    fn implement_crash_synth_outcomes_classify_as_crash_and_render_crash_pr_and_log_bodies() {
        // Issue #49 end-to-end shape: a PhaseOutcomes carrying the
        // synth flag (set true by the runner when implement crashed
        // with no commits) and a non-zero implement exit must route
        // through `classify_exit` to `Crash`. The synth wrote
        // agent-notes.md and committed it; per ADR-0006 / issue #95
        // that content now flows through note classification with the
        // recorded Bellows synth span, which recognises the synth-only
        // file as `NotesShape::Absent`. Routing then falls through to
        // Crash on the non-zero implement exit — no per-call
        // `synth_suppresses_notes` shim needed. The resulting PR body
        // must be the Crash body
        // (`crashed`), NOT the AgentSelfReportedFailure body which
        // would quote the bellows-synthesised note as if the agent
        // had self-reported.
        let mut synth_note = String::new();
        let synth_span = policy::append_bellows_synth_entry(
            &mut synth_note,
            &policy::synthesize_implement_crash_entry(
                137,
                "Error: /workspace/entrypoint-user: bad interpreter",
            ),
            policy::BellowsSynthCause::ImplementCrash,
        );
        let outcomes = PhaseOutcomes {
            implement: ImplementOutcome {
                exit_code: 137,
                stderr_tail: "Error: /workspace/entrypoint-user: bad interpreter".to_string(),
                engine: None,
            },
            post_implement_gate: GateOutcome::default(),
            review: None,
            review_fix: None,
            end_pipeline_gate: None,
            wall_clock_exceeded: false,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: true,
            merger_verdict: None,
            security: None,
            security_fix: None,
        };
        let notes_shape =
            policy::classify_agent_notes_with_synth_spans(Some(&synth_note), &[synth_span]);
        assert_eq!(
            notes_shape,
            policy::NotesShape::Absent,
            "synth-only notes must classify to Absent so the routing falls through \
             to Crash on the non-zero implement exit",
        );
        let reason = policy::classify_exit(notes_shape, &outcomes, None);
        assert_eq!(
            reason,
            ExitReason::Crash,
            "synth + non-zero implement exit must classify as Crash, not \
             AgentSelfReportedFailure — the synth note is bellows-authored, \
             not an agent self-report",
        );
        // build_pr_body for Crash must NOT quote the synth note as
        // "self-reported failure" content.
        let pr_body = build_pr_body(&reason, 42, None, Some(synth_note.trim()), &[], &PhaseOutcomes::default());
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
        let log_body = build_log_body(&reason, 42, started, finished, "agent/42-x", &outcomes, &[]);
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
        &[],
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

        // Write all handoff files (slice X1 + slice X2); cleanup must
        // remove every one. Adding the slice-X2 security findings file
        // here pins the cleanup contract for the new phase pair — any
        // leftover would ship in the PR diff.
        for name in &[
            policy::REVIEW_DIFF_FILE,
            policy::REVIEW_FINDINGS_FILE,
            policy::REVIEW_COMMIT_LOG_FILE,
            policy::SECURITY_FINDINGS_FILE,
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
            policy::SECURITY_FINDINGS_FILE,
        ] {
            assert!(
                !workspace.path().join(name).exists(),
                "post-cleanup: {name} must have been removed",
            );
        }
    }

    // ---- Slice X2: security-review and security-fix phase outcomes ----

    /// Build a slice-X2-shaped PhaseOutcomes for tests of the log body's
    /// security phase summary. Defaults to a clean review run (no
    /// findings, exit 0); each test overrides `security` and
    /// `security_fix` to express its scenario.
    fn slice_x2_outcomes(
        security: Option<crate::policy::AnalysisOutcome>,
        security_fix: Option<FixOutcome>,
    ) -> PhaseOutcomes {
        PhaseOutcomes {
            implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new(), engine: None },
            post_implement_gate: GateOutcome {
                cargo_clippy: Some(CheckResult { exit_code: 0, output: String::new() }),
                cargo_test: Some(CheckResult { exit_code: 0, output: String::new() }),
            },
            review: Some(ReviewOutcome { findings_text: None, exit_code: 0 }),
            review_fix: None,
            end_pipeline_gate: Some(GateOutcome {
                cargo_clippy: Some(CheckResult { exit_code: 0, output: String::new() }),
                cargo_test: Some(CheckResult { exit_code: 0, output: String::new() }),
            }),
            wall_clock_exceeded: false,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
            merger_verdict: None,
            security,
            security_fix,
        }
    }

    #[test]
    fn build_log_body_includes_security_phase_summary_with_findings_and_successful_fix() {
        // Acceptance criterion (a) from the brief: security-review
        // produced findings AND the security-fix run addressed them
        // cleanly. The log body's phase summary must surface both lines
        // — without the security entries, an operator scanning the PR
        // comment cannot tell whether the security phase ran or what it
        // found.
        let started = fixed_timestamp();
        let finished = started;
        let outcomes = slice_x2_outcomes(
            Some(crate::policy::AnalysisOutcome {
                findings_text: Some("## Findings\n\n### 1. command injection — blocker\n\nbody".to_string()),
                exit_code: 0,
            }),
            Some(FixOutcome { exit_code: 0 }),
        );
        let body = build_log_body(
            &ExitReason::Success,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        &[],
        );
        assert!(
            body.contains("Security:"),
            "phase summary must include a Security line: {body}"
        );
        assert!(
            body.contains("Security-fix:"),
            "phase summary must include a Security-fix line: {body}"
        );
    }

    #[test]
    fn build_log_body_security_phase_marks_review_crash_explicitly() {
        // Acceptance criterion (c) from the brief: security-review run
        // itself crashes. The log body must name the crash explicitly so
        // a human reading the PR comment knows which phase died, distinct
        // from a clean run that simply found nothing.
        let started = fixed_timestamp();
        let finished = started;
        let outcomes = slice_x2_outcomes(
            Some(crate::policy::AnalysisOutcome {
                findings_text: None,
                exit_code: 137,
            }),
            None,
        );
        let body = build_log_body(
            &ExitReason::Crash,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        &[],
        );
        assert!(
            body.contains("Security: crashed"),
            "phase summary must explicitly name the security-review crash: {body}"
        );
        assert!(
            body.contains("137"),
            "phase summary must surface the security-review exit code: {body}"
        );
        // Halt-on-phase-failure means security-fix should not have run.
        assert!(
            body.contains("Security-fix: did not run"),
            "halted runs must show Security-fix as did not run: {body}"
        );
    }

    #[test]
    fn build_log_body_security_phase_marks_fix_crash_explicitly() {
        // Acceptance criterion (b) from the brief: security-review
        // produced findings AND the security-fix run crashed. The log
        // body must surface the fix-run crash so the operator can
        // distinguish "fix crashed mid-way" from "fix succeeded".
        let started = fixed_timestamp();
        let finished = started;
        let outcomes = slice_x2_outcomes(
            Some(crate::policy::AnalysisOutcome {
                findings_text: Some("findings here".to_string()),
                exit_code: 0,
            }),
            Some(FixOutcome { exit_code: 137 }),
        );
        let body = build_log_body(
            &ExitReason::Crash,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        &[],
        );
        assert!(
            body.contains("Security: findings produced"),
            "phase summary must show that security found something: {body}"
        );
        assert!(
            body.contains("Security-fix: crashed"),
            "phase summary must explicitly name the security-fix crash: {body}"
        );
        assert!(
            body.contains("137"),
            "phase summary must surface the security-fix exit code: {body}"
        );
    }

    #[test]
    fn build_log_body_security_phase_handles_empty_findings_cleanly() {
        // Acceptance criterion (d) from the brief: empty/missing
        // security-findings file is a success path — security-review
        // exited cleanly with no findings, security-fix did not run.
        // The phase summary must NOT make this look like a failure.
        let started = fixed_timestamp();
        let finished = started;
        let outcomes = slice_x2_outcomes(
            Some(crate::policy::AnalysisOutcome {
                findings_text: None,
                exit_code: 0,
            }),
            None,
        );
        let body = build_log_body(
            &ExitReason::Success,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        &[],
        );
        assert!(
            body.contains("Security: no findings"),
            "clean security-review must read as `no findings` in the summary: {body}"
        );
        assert!(
            body.contains("Security-fix: did not run"),
            "security-fix must report as did-not-run when there were no findings: {body}"
        );
        // Defensive: a clean run with no findings must NOT show as a
        // crash — otherwise a routine clean run would look alarming.
        assert!(
            !body.contains("Security: crashed"),
            "clean security-review must NOT show as crashed: {body}"
        );
    }

    #[test]
    fn build_log_body_security_phase_did_not_run_when_security_is_none() {
        // The pipeline halted before reaching security (e.g. review or
        // review-fix crashed, or the wall-clock budget was spent). The
        // phase summary must visibly name security as did-not-run so a
        // reader can tell the difference between "ran and found nothing"
        // and "never executed".
        let started = fixed_timestamp();
        let finished = started;
        let outcomes = slice_x2_outcomes(None, None);
        let body = build_log_body(
            &ExitReason::Crash,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        &[],
        );
        assert!(
            body.contains("Security: did not run"),
            "halted-before-security runs must show Security as did not run: {body}"
        );
        assert!(
            body.contains("Security-fix: did not run"),
            "halted-before-security runs must also show Security-fix as did not run: {body}"
        );
    }

    #[test]
    fn build_log_body_handles_mixed_scenario_with_review_and_security_both_addressed() {
        // Acceptance criterion (e) from the brief: review findings were
        // produced and addressed, then the review fixups introduced new
        // security findings which were also addressed. The phase summary
        // must surface both review and security outcomes side-by-side so
        // the operator can confirm the full pair-of-pairs ran cleanly.
        let started = fixed_timestamp();
        let finished = started;
        let outcomes = PhaseOutcomes {
            implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new(), engine: None },
            post_implement_gate: GateOutcome {
                cargo_clippy: Some(CheckResult { exit_code: 0, output: String::new() }),
                cargo_test: Some(CheckResult { exit_code: 0, output: String::new() }),
            },
            review: Some(ReviewOutcome {
                findings_text: Some("review findings here".to_string()),
                exit_code: 0,
            }),
            review_fix: Some(FixOutcome { exit_code: 0 }),
            end_pipeline_gate: Some(GateOutcome {
                cargo_clippy: Some(CheckResult { exit_code: 0, output: String::new() }),
                cargo_test: Some(CheckResult { exit_code: 0, output: String::new() }),
            }),
            wall_clock_exceeded: false,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
            merger_verdict: None,
            security: Some(crate::policy::AnalysisOutcome {
                findings_text: Some(
                    "## Findings\n\n### 1. command injection in shell call — blocker\n\nbody"
                        .to_string(),
                ),
                exit_code: 0,
            }),
            security_fix: Some(FixOutcome { exit_code: 0 }),
        };
        let body = build_log_body(
            &ExitReason::Success,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
        &[],
        );
        // Both phases must be summarised. The exact phrasing is
        // implementation-detail, but each phase needs its own surface so
        // the operator can map them back to the PR comments.
        assert!(body.contains("Review: findings produced"), "review line missing: {body}");
        assert!(body.contains("Review-fix:"), "review-fix line missing: {body}");
        assert!(body.contains("Security: findings produced"), "security line missing: {body}");
        assert!(body.contains("Security-fix:"), "security-fix line missing: {body}");
        // The end-pipeline cargo gate caught no regressions, so the final
        // tests line must still report PASSED.
        assert!(
            body.to_lowercase().contains("end-pipeline"),
            "end-pipeline cargo gate must still be summarised: {body}"
        );
    }

    #[test]
    fn build_log_body_caps_oversized_bodies_below_the_github_comment_limit() {
        // Issue #87 AC #2. GitHub rejects PR/issue comment bodies above
        // 65536 characters with HTTP 422. Large agent runs (stderr tail
        // + cargo output + per-phase callouts) routinely produce >64 KiB
        // pre-cap. `build_log_body` must bound its output so the
        // downstream POST in `finalise` succeeds.
        let started = fixed_timestamp();
        let finished = started;
        // Stuff the stderr tail with ~80 KiB of content — well past the
        // limit. End-pipeline gate output further pads it.
        let huge_stderr = "stderr line that pads the tail; ".repeat(2500);
        let huge_test_output = "cargo test failure detail; ".repeat(1500);
        let outcomes = PhaseOutcomes {
            implement: ImplementOutcome {
                exit_code: 0,
                stderr_tail: huge_stderr,
                engine: None,
            },
            post_implement_gate: GateOutcome {
                cargo_clippy: None,
                cargo_test: Some(CheckResult {
                    exit_code: 101,
                    output: huge_test_output,
                }),
            },
            review: None,
            review_fix: None,
            end_pipeline_gate: None,
            wall_clock_exceeded: false,
            backstop_violations: Vec::new(),
            implement_crash_synthesised: false,
            merger_verdict: None,
            security: None,
            security_fix: None,
        };
        let body = build_log_body(
            &ExitReason::FinalTestsRed,
            42,
            started,
            finished,
            "agent/42-huge",
            &outcomes,
        &[],
        );

        // Cap leaves headroom for any subsequent encoding / wrapping —
        // the exact constant is an implementation detail, but it must
        // be well under GitHub's 65536 limit AND the body's character
        // count (which is what the API actually counts) must stay below.
        assert!(
            body.chars().count() <= 65000,
            "build_log_body must bound its output below GitHub's 64 KiB \
             comment limit; got {} chars",
            body.chars().count(),
        );
        // The truncated body must remain readable: the operator needs
        // to know the content was clipped and where to find the rest.
        assert!(
            body.to_lowercase().contains("truncated"),
            "expected an explicit truncation marker so the reader can \
             tell the body was clipped; body did not contain 'truncated'",
        );
        assert!(
            body.contains("bellows.log"),
            "expected the truncation footer to point at bellows.log on \
             the operator's host for the full output",
        );
        // The wrapping `<details>` element must still be closed so the
        // PR comment renders cleanly even after a mid-section clip.
        assert!(
            body.trim_end().ends_with("</details>"),
            "truncated body must still close the <details> wrapper; \
             body ended with: {:?}",
            &body[body.len().saturating_sub(80)..],
        );
    }

    #[test]
    fn build_log_body_for_short_runs_is_unchanged_by_the_truncation_path() {
        // Issue #87 AC #4 baseline. Short bodies must not gain a
        // truncation footer or any visible side-effect from the
        // size-handling code — only oversized bodies trigger the clip.
        let started = fixed_timestamp();
        let finished = started;
        let body = build_log_body(
            &ExitReason::Success,
            42,
            started,
            finished,
            "agent/42-x",
            &slice5_log_outcomes(0, "small run", None),
        &[],
        );

        assert!(
            !body.to_lowercase().contains("truncated"),
            "short bodies must not carry a truncation marker; got: {body}",
        );
        // Closing tag is the natural end of an un-truncated body.
        assert!(body.trim_end().ends_with("</details>"));
        // Should be obviously well under the cap.
        assert!(body.chars().count() < 5_000);
    }

    /// Issue #111: invoke both composers (PR body + run-log) and return
    /// the (pr_body, log_body) pair so a parametrised test can assert
    /// equivalent callout shape on both surfaces. Drift between the two
    /// surfaces is the regression to avoid; sharing the helper here
    /// means a one-sided fix flips the test red.
    fn compose_both_surfaces(
        workflow_files_changed: &[String],
    ) -> (String, String) {
        let started = fixed_timestamp();
        let finished = started;
        let outcomes = slice5_log_outcomes(0, "agent done", None);
        let pr_body = build_pr_body(
            &ExitReason::Success,
            42,
            Some("PR body from claude."),
            None,
            workflow_files_changed,
            &outcomes,
        );
        let log_body = build_log_body(
            &ExitReason::Success,
            42,
            started,
            finished,
            "agent/42-x",
            &outcomes,
            workflow_files_changed,
        );
        (pr_body, log_body)
    }

    #[test]
    fn workflow_file_change_callout_appears_on_both_pr_body_and_run_log_when_files_touched() {
        // Issue #111 AC: when the diff touches one or more workflow
        // files, BOTH the PR body and the run-log comment include a
        // labelled section that names the file(s) and explains the
        // gate-vs-CI gap. Parametrised over the two composers so drift
        // between them flips this test red.
        let files = vec![".github/workflows/ci.yml".to_string()];
        let (pr_body, log_body) = compose_both_surfaces(&files);

        for (surface, body) in [("pr_body", &pr_body), ("log_body", &log_body)] {
            // (a) Labelled section header — operator scanning the PR
            // body / run-log should be able to spot it as a callout.
            assert!(
                body.to_lowercase().contains("workflow"),
                "{surface}: callout should mention 'workflow'; got:\n{body}",
            );
            // (b) Names the changed file(s) so the operator knows where
            // to look.
            assert!(
                body.contains(".github/workflows/ci.yml"),
                "{surface}: callout must name the changed file; got:\n{body}",
            );
            // (c) Explains the gate-vs-CI gap: mentions cargo clippy
            // and cargo test (the only two CI steps bellows mirrors).
            // An operator reading the callout cold should understand
            // both facts.
            assert!(
                body.contains("cargo clippy"),
                "{surface}: callout must mention cargo clippy gate scope; got:\n{body}",
            );
            assert!(
                body.contains("cargo test"),
                "{surface}: callout must mention cargo test gate scope; got:\n{body}",
            );
        }
    }

    #[test]
    fn workflow_file_change_callout_omitted_on_both_surfaces_when_no_files_touched() {
        // Issue #111 AC: when the diff does NOT touch any workflow
        // file (the common case), neither surface includes the
        // callout — no empty/header-only section, no whitespace
        // noise. Parametrised over the two composers so drift
        // between them flips this test red.
        let (pr_body, log_body) = compose_both_surfaces(&[]);

        for (surface, body) in [("pr_body", &pr_body), ("log_body", &log_body)] {
            assert!(
                !body.to_lowercase().contains("workflow"),
                "{surface}: empty file list must omit the callout entirely; got:\n{body}",
            );
            // Belt-and-braces: the file path of a typical workflow
            // file must NOT appear in the rendered body when the
            // helper returned an empty list.
            assert!(
                !body.contains(".github/workflows/"),
                "{surface}: empty file list must not leak workflow paths; got:\n{body}",
            );
        }
    }

    #[test]
    fn workflow_file_change_callout_lists_multiple_files_on_both_surfaces() {
        // Issue #111 AC: when multiple workflow files are touched,
        // both surfaces name all of them so the operator can audit
        // every CI-shape change in one pass.
        let files = vec![
            ".github/workflows/ci.yml".to_string(),
            ".github/workflows/release.yaml".to_string(),
        ];
        let (pr_body, log_body) = compose_both_surfaces(&files);

        for (surface, body) in [("pr_body", &pr_body), ("log_body", &log_body)] {
            assert!(
                body.contains(".github/workflows/ci.yml"),
                "{surface}: must name ci.yml; got:\n{body}",
            );
            assert!(
                body.contains(".github/workflows/release.yaml"),
                "{surface}: must name release.yaml; got:\n{body}",
            );
        }
    }
}
