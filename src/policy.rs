/// Classification of how an agent run ended. `policy::classify_exit`
/// produces this from the post-run signals; the runner uses it to choose
/// PR draft state, label, and log-comment shape.
///
/// `FinalTestsRed` covers any failing post-run cargo check — clippy or
/// test, in either the post-implement gate or the end-of-pipeline gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    Success,
    AgentSelfReportedFailure,
    Crash,
    FinalTestsRed,
}

/// Outcome of the implement run: the first phase, where claude reads
/// the agent brief and writes code.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImplementOutcome {
    pub exit_code: i64,
    pub stderr_tail: String,
}

/// One cargo subcommand's exit code + captured output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub exit_code: i64,
    pub output: String,
}

/// Outcome of one cargo checks gate run (clippy followed by test).
/// `None` for either field encodes "the check did not run" — clippy is
/// `None` when the workspace has no `Cargo.toml` at the root; test is
/// `None` when clippy failed and we never got to it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GateOutcome {
    pub cargo_clippy: Option<CheckResult>,
    pub cargo_test: Option<CheckResult>,
}

/// Outcome of the review phase. `findings_text` is `Some` when the agent
/// produced a non-empty findings file; `None` means the review run found
/// nothing to flag (clean diff) and the runner skipped review-fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewOutcome {
    pub findings_text: Option<String>,
    pub exit_code: i64,
}

/// Outcome of the review-fix phase. Only present in `PhaseOutcomes` when
/// review produced findings and the fix run was actually launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixOutcome {
    pub exit_code: i64,
}

/// Aggregated per-phase signals from one agent pipeline run. Drives the
/// PR-body and log-body builders (which consume the per-phase detail)
/// and `classify_exit` (which collapses it into a single `ExitReason`
/// for routing).
///
/// `Option` fields encode "phase did not run" cleanly — e.g. `review` is
/// `None` when the post-implement gate failed and the runner short-
/// circuited before reaching review.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PhaseOutcomes {
    pub implement: ImplementOutcome,
    pub post_implement_gate: GateOutcome,
    pub review: Option<ReviewOutcome>,
    pub review_fix: Option<FixOutcome>,
    pub end_pipeline_gate: Option<GateOutcome>,
}

/// Decide how a finished agent run should be classified.
///
/// Precedence: an agent self-report (notes file present) wins over
/// everything else; the agent's voice always trumps tooling signals.
/// Then any non-zero implement exit is `Crash`. Then any failing cargo
/// gate (clippy or test, post-implement or end-pipeline) is
/// `FinalTestsRed`. Otherwise `Success`.
pub fn classify_exit(has_agent_notes: bool, outcomes: &PhaseOutcomes) -> ExitReason {
    if has_agent_notes {
        return ExitReason::AgentSelfReportedFailure;
    }
    if outcomes.implement.exit_code != 0 {
        return ExitReason::Crash;
    }
    if gate_failed(&outcomes.post_implement_gate) {
        return ExitReason::FinalTestsRed;
    }
    if let Some(end_gate) = &outcomes.end_pipeline_gate
        && gate_failed(end_gate)
    {
        return ExitReason::FinalTestsRed;
    }
    ExitReason::Success
}

fn gate_failed(gate: &GateOutcome) -> bool {
    let nonzero = |c: &Option<CheckResult>| matches!(c, Some(r) if r.exit_code != 0);
    nonzero(&gate.cargo_clippy) || nonzero(&gate.cargo_test)
}

/// Workspace-relative path of the diff file the runner writes before
/// the review phase. Read-only input to the review prompt; the runner
/// generates this on the host (via `git diff`) and removes it after
/// the review-fix phase completes.
pub const REVIEW_DIFF_FILE: &str = ".bellows-review-diff.patch";

/// Workspace-relative path of the findings file the review prompt
/// writes. The runner reads it after the review run and posts the
/// contents as a `## Review findings` PR comment. Review-fix removes
/// the file when all findings are addressed.
pub const REVIEW_FINDINGS_FILE: &str = ".bellows-review-findings.md";

/// Vendored review-phase prompt. Documents the input file path
/// (REVIEW_DIFF_FILE), the output file path (REVIEW_FINDINGS_FILE),
/// the findings markdown format, and the agent-notes append-not-
/// overwrite contract. Bellows-specific (operates on a local diff
/// instead of `gh pr diff`) so the container stays GitHub-credential-
/// free.
pub const REVIEW_PROMPT: &str = r#"You are running as the **review phase** of a Bellows agent pipeline. The implement phase has already produced changes on this branch; your job is to review the diff for correctness, maintainability, project conventions, and test coverage.

## Inputs

- `/workspace/.bellows-review-diff.patch` contains `git diff <base>...HEAD` — the entire delta the implement phase produced. Read this file as the primary input. Do not browse the wider codebase except to disambiguate symbols referenced in the diff; the patch is the contract.
- `/workspace/agent-notes.md` may exist (the implement phase appended to it if it could not complete some part of the brief). Read it for context on deliberate gaps or known limitations.

## Output

Write your findings to `/workspace/.bellows-review-findings.md` in this markdown format:

```
## Findings

### 1. <one-line title> — <severity: blocker | important | nit>

<one-paragraph description of what is wrong and why it matters>

**Suggestion:** <concrete change that would address this finding>

### 2. ...
```

If you find no issues worth flagging, write the file with a single line: `(no findings)`. The file MUST exist either way — Bellows reads it after the run and treats it as the contract for the review-fix phase.

## What this phase does NOT do

You are read-only. Do NOT edit any files except `.bellows-review-findings.md` and (optionally) `agent-notes.md`. Do NOT create commits. Do NOT push. The review-fix phase that follows you will read your findings and address them.

## When you cannot complete

If the diff is malformed, missing, or you genuinely cannot review it, append a section to `/workspace/agent-notes.md` explaining what stopped you. APPEND — do not overwrite. The file may already contain notes from the implement phase that must remain visible to the human reviewer.
"#;

/// Vendored review-fix-phase prompt. Documents the findings file path,
/// the address-each-finding-and-commit contract, the remove-on-
/// completion contract, and the agent-notes append contract.
pub const REVIEW_FIX_PROMPT: &str = r#"You are running as the **review-fix phase** of a Bellows agent pipeline. The review phase wrote findings to a file; your job is to address each finding by making code changes and committing them.

## Inputs

- `/workspace/.bellows-review-findings.md` — the findings file written by the review phase. Each finding has a title, a description, and a suggestion. Read every finding before making changes.
- `/workspace/agent-notes.md` may exist with notes from earlier phases. Read it for context.

## Process

Address each finding from the findings file:

1. Read the finding's description and suggestion.
2. Decide whether the suggested change is correct (you are not bound to apply it verbatim — if a different change addresses the same root cause, that is fine).
3. Make the change.
4. Run `cargo check` (or equivalent) to confirm you have not broken compilation.
5. Commit each fix with a clear, scoped commit message (one commit per finding is ideal; bundling a few that touch the same file is acceptable).

After all findings are addressed, REMOVE the findings file:

```
rm /workspace/.bellows-review-findings.md
```

Bellows treats a missing or empty findings file as "this phase completed cleanly."

## When you cannot address a finding

If a finding requires a judgement call you cannot make (architectural decision, requires human context, etc.), append a section to `/workspace/agent-notes.md` explaining which finding you could not address and why. APPEND — do not overwrite previous content from earlier phases. The agent-notes.md file present at the end of the pipeline triggers `agent-self-reported-failure` regardless of which phase wrote it.

Do NOT remove the findings file in that case — leaving it in place tells Bellows that not all findings were addressed.

## Stop conditions

Stop when:

- All findings are addressed AND `cargo test` is green AND the findings file is removed; OR
- You could not address one or more findings and have logged that to agent-notes.md.
"#;

/// Render the kickoff prompt that gets fed into `claude -p` inside the
/// sandbox. Pure function so it can be unit-tested without spinning up
/// a container.
pub fn render_kickoff(brief: &str, repo_url: &str, branch_name: &str) -> String {
    format!(
        "You are working on {repo_url} on branch `{branch_name}`.\n\
         \n\
         {brief}\n\
         \n\
         ## How to work\n\
         \n\
         Use the `tdd` skill: write failing tests first, then implement to green, then refactor.\n\
         The skill is available in your skills directory; invoke it before doing implementation work.\n\
         \n\
         ## Stop conditions\n\
         \n\
         Stop only when `cargo test` is green and your changes satisfy every acceptance criterion in the brief above.\n\
         Do NOT write a `.bellows-stub-marker` (or any other marker) file — the slice-2 stub agent is gone; only your real changes should appear in the resulting commit.\n\
         \n\
         When you are done, write a PR description body to `/workspace/.bellows-pr-description.md` summarising what you built, mapping each new test to the brief's acceptance criteria.\n"
    )
}
