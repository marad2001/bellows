/// Classification of how an agent run ended. `policy::classify_exit`
/// produces this from the post-run signals; the runner uses it to choose
/// PR draft state, label, and log-comment shape.
///
/// `FinalTestsRed` covers any failing post-run cargo check — clippy or
/// test, in either the post-implement gate or the end-of-pipeline gate.
///
/// `WallClockExceeded` covers any pipeline that exceeded the configured
/// per-issue budget (`[agent].wall_clock_minutes`) — either short-
/// circuited before a phase started because the budget was already
/// spent, or had a container killed mid-run when the deadline fired.
///
/// `RateLimited` covers a non-zero phase exit whose stderr matches a
/// known Anthropic API rate-limit signature. Operator-distinguishable
/// from `Crash` because the appropriate response is "wait for the
/// rate-limit window to clear and re-run" rather than "investigate."
///
/// `Cancelled` covers a run where `bellows kill <N>` (slice 10) flipped
/// the issue's label out from under us during the pipeline. The
/// runner detects this BEFORE opening the PR (via a lightweight GET
/// on the issue's labels) and overrides the classification so the
/// PR opens draft + the log body says "Cancelled" rather than
/// whatever the pipeline-internal signals would have suggested
/// (commonly `Success` — phases that completed naturally between the
/// kill firing and the cancellation check would otherwise misclassify
/// as a successful run, producing a ready-for-review PR a reviewer
/// could plausibly merge).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    Success,
    AgentSelfReportedFailure,
    Crash,
    FinalTestsRed,
    WallClockExceeded,
    RateLimited,
    Cancelled,
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

/// Outcome of a generic analysis phase that reads a diff and writes a
/// findings file (slice X2: security-review). Same shape as
/// `ReviewOutcome` but kept as a distinct type so `PhaseOutcomes` carries
/// a clearly-named field for each phase — a glance at the struct shows
/// which phase produced which signal.
///
/// `findings_text` is `Some` when the phase produced a non-empty findings
/// file; `None` means the analysis found nothing to flag (clean diff) and
/// the runner skipped the corresponding fix phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisOutcome {
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
    /// Slice X2: outcome of the security-review phase. Sits between
    /// review-fix and the end-of-pipeline cargo gate; reads the
    /// post-review-fix diff and writes findings to
    /// `SECURITY_FINDINGS_FILE`. `None` when the runner halted before
    /// the security-review phase ran (e.g. implement crashed,
    /// post-implement gate failed, or review/review-fix crashed).
    pub security: Option<AnalysisOutcome>,
    /// Slice X2: outcome of the security-fix phase. Only present when
    /// the security-review phase produced findings AND the fix run was
    /// actually launched. `None` means either no findings to fix or the
    /// runner halted before the fix phase could run.
    pub security_fix: Option<FixOutcome>,
    pub end_pipeline_gate: Option<GateOutcome>,
    /// True when the runner short-circuited the pipeline because the
    /// per-issue wall-clock budget was exceeded — either the budget hit
    /// zero before a phase started, or a container was killed mid-run
    /// when its deadline fired. Orthogonal to per-phase exit codes since
    /// the run was killed, not exited cleanly.
    pub wall_clock_exceeded: bool,
    /// Slice 9.6: blocker/important findings that the parser-as-
    /// backstop detected as neither addressed-in-code nor explained
    /// via an `## Unaddressed finding:` section. Empty in the typical
    /// path (address-OR-explain contract met). When non-empty the
    /// runner appended synthetic agent-notes entries (which routes
    /// the run to AgentSelfReportedFailure) and the log comment
    /// includes the `### Address-or-explain contract violated`
    /// callout that names each offending finding.
    pub backstop_violations: Vec<ParsedFinding>,
    /// Issue #49: true when the runner synthesised an
    /// `agent-notes.md` entry to recover from an implement-phase
    /// crash that left the workspace with no commits. The synth's
    /// only purpose is to give the run something to commit so the
    /// branch can be pushed and a draft PR opened (otherwise the
    /// pipeline silently stalls at `agent-in-progress`). The note
    /// content is bellows-authored, NOT agent-authored — so the
    /// usual `has_agent_notes → AgentSelfReportedFailure` precedence
    /// in `classify_exit` is suppressed when this flag is set and
    /// implement actually exited non-zero, letting the run classify
    /// as `Crash` instead.
    pub implement_crash_synthesised: bool,
}

/// Decide how a finished agent run should be classified.
///
/// Precedence: an agent self-report (notes file present) wins over
/// everything else; the agent's voice always trumps tooling signals.
/// Then any non-zero implement exit is `Crash`. Then any failing cargo
/// gate (clippy or test, post-implement or end-pipeline) is
/// `FinalTestsRed`. Otherwise `Success`.
pub fn classify_exit(has_agent_notes: bool, outcomes: &PhaseOutcomes) -> ExitReason {
    // Issue #49: when the runner synthesised an agent-notes entry to
    // recover from an implement-phase crash with no commits, the file
    // exists on disk (and ships in the PR diff) but is bellows-authored,
    // not agent-authored. The agent did not self-report; bellows wrote
    // the note to give the run something to commit. Suppress the usual
    // `has_agent_notes` precedence so the run classifies on its actual
    // failure mode (Crash) rather than spuriously routing to
    // AgentSelfReportedFailure. The synth flag only suppresses when the
    // implement phase ACTUALLY crashed — a clean-exit run with the flag
    // somehow set (defensive corner) still respects notes-precedence.
    let synth_suppresses_notes =
        outcomes.implement_crash_synthesised && outcomes.implement.exit_code != 0;
    if has_agent_notes && !synth_suppresses_notes {
        return ExitReason::AgentSelfReportedFailure;
    }
    if outcomes.wall_clock_exceeded {
        return ExitReason::WallClockExceeded;
    }
    // Rate-limit detection runs BEFORE the generic Crash check so a
    // non-zero exit caused by an Anthropic rate-limit gets the more
    // specific operator signal. Signature alone is insufficient — the
    // run must have actually exited non-zero, otherwise a successful
    // run that happens to mention a rate-limit error string in benign
    // context would misclassify.
    if outcomes.implement.exit_code != 0
        && is_rate_limit_signature(&outcomes.implement.stderr_tail)
    {
        return ExitReason::RateLimited;
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

/// Whether the given text contains a known rate-limit signature. Used
/// by `classify_exit` to distinguish a rate-limit failure from a
/// generic crash so the operator gets the right follow-up signal
/// ("wait for the rate-limit window to clear and re-run" vs
/// "investigate").
///
/// Matches case-insensitively. The signature set covers:
///   - Anthropic / Claude Code: the underscore-style identifiers
///     Anthropic uses in API error responses (`rate_limit_error`,
///     `rate_limited`).
///   - Codex (issue #79 spike findings, sourced from
///     `codex-rs/codex-api/src/error.rs`): `quota exceeded`
///     (subscription users, primary path) and `rate limit:`
///     (Platform-API users, secondary path).
///
/// Bare HTTP `429` is deliberately NOT matched — too false-positive-
/// prone (port numbers, test fixtures, JSON byte counts, etc.).
pub fn is_rate_limit_signature(text: &str) -> bool {
    const SIGNATURES: [&str; 4] = [
        // Claude Code / Anthropic API signatures.
        "rate_limit_error",
        "rate_limited",
        // Codex signatures (issue #79 / ADR-0005 spike findings).
        "quota exceeded",
        "rate limit:",
    ];
    let lower = text.to_lowercase();
    SIGNATURES.iter().any(|sig| lower.contains(sig))
}

/// Whether the given text contains a known auth-error signature. Used
/// by the log-body builder to surface a clear "run `bellows
/// refresh-auth`" pointer when a non-zero phase exit was caused by an
/// expired OAuth refresh token rather than a generic crash. Mirrors
/// `is_rate_limit_signature` in shape.
///
/// Matches case-insensitively. Current entries:
///   - Claude Code / Anthropic API: the literal `"401 unauthorized"`
///     HTTP status line, the underscore-style `"refresh_token_expired"`
///     identifier Anthropic returns in API error payloads, and the
///     human-readable `"authentication failed"` phrase that appears in
///     Claude Code's stderr when its OAuth session is rejected.
///   - Codex (issue #79 / ADR-0005 spike findings): composite match
///     of `"401 unauthorized"` AND `"missing bearer or basic
///     authentication"` (a bare `401 Unauthorized` could be a false
///     positive from unrelated HTTP 401 in the agent's web-fetched
///     content; the composite avoids that, see
///     `is_codex_auth_error_signature` for the strict path).
///
/// Note: bellows uses the union of all engine signatures here for
/// the existing "auth error happened in this run" callout. The
/// engine-naming callout (issue #81 / ADR-0005 AC: "Auth-error callout
/// in the run-log comment names the engine to refresh") uses the
/// per-engine helpers below.
pub fn is_auth_error_signature(text: &str) -> bool {
    is_claude_auth_error_signature(text) || is_codex_auth_error_signature(text)
}

/// Claude-side auth-error signature subset. Returns true when the
/// stderr looks like the Claude Code CLI / Anthropic API auth
/// failure mode — used by the run-log builder to name the engine to
/// refresh (`bellows refresh-auth --engine claude`).
pub fn is_claude_auth_error_signature(text: &str) -> bool {
    const SIGNATURES: [&str; 3] = [
        "401 unauthorized",
        "refresh_token_expired",
        "authentication failed",
    ];
    let lower = text.to_lowercase();
    SIGNATURES.iter().any(|sig| lower.contains(sig))
}

/// Codex-side auth-error signature subset. Composite match of `401
/// Unauthorized` AND `Missing bearer or basic authentication` (issue
/// #79 spike findings) so a bare `401 Unauthorized` in unrelated
/// web-fetched content does not produce a false positive.
pub fn is_codex_auth_error_signature(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("401 unauthorized")
        && lower.contains("missing bearer or basic authentication")
}

/// Whether either of a gate's checks exited non-zero. Crate-public so the
/// runner can use it for orchestration decisions ("should we halt before
/// review?") with the same predicate `classify_exit` uses for routing —
/// keeping them in sync prevents a divergence bug.
pub(crate) fn gate_failed(gate: &GateOutcome) -> bool {
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

/// Workspace-relative path of the findings file the security-review
/// prompt writes (slice X2). Sibling of `REVIEW_FINDINGS_FILE`, but for
/// the security-review phase: the runner reads it after the
/// security-review run and posts the contents as a `## Security findings`
/// PR comment. The security-fix phase removes the file when all findings
/// are addressed; defensive cleanup at the end of the pipeline catches
/// any leftover so the file never lands in the PR diff.
pub const SECURITY_FINDINGS_FILE: &str = ".bellows-security-findings.md";

/// Workspace-relative path of the commit-log file the runner writes
/// before the review phase. Read-only input to the review prompt
/// alongside REVIEW_DIFF_FILE — the diff shows the squashed end-state,
/// the commit log shows ordering. The reviewer-claude reads it to
/// reason about test-first commit shape (one failing-test commit then
/// one make-it-pass commit, per acceptance criterion); mega-commits
/// and source-before-test orderings are visible from this file but
/// not from the squashed diff. The runner removes the file after the
/// review-fix phase completes so it never lands in the PR diff.
pub const REVIEW_COMMIT_LOG_FILE: &str = ".bellows-review-commit-log.txt";

/// Vendored review-phase prompt. Documents the input file path
/// (REVIEW_DIFF_FILE), the output file path (REVIEW_FINDINGS_FILE),
/// the findings markdown format with a closed `blocker | important |
/// nit` severity vocabulary, and the agent-notes append-not-overwrite
/// contract. Bellows-specific (operates on a local diff instead of
/// `gh pr diff`) so the container stays GitHub-credential-free.
pub const REVIEW_PROMPT: &str = r#"You are running as the **review phase** of a Bellows agent pipeline. The implement phase has already produced changes on this branch; your job is to review the diff for correctness, maintainability, project conventions, and test coverage.

## Inputs

- `/workspace/.bellows-review-diff.patch` contains `git diff <base>...HEAD` — the entire delta the implement phase produced. Read this file as the primary input. Do not browse the wider codebase except to disambiguate symbols referenced in the diff; the patch is the contract.
- `/workspace/.bellows-review-commit-log.txt` contains `git log --name-status <base>...HEAD` — the commit-by-commit history of the agent branch since it diverged from the base. Use this to reason about commit *ordering* (which the squashed diff cannot show): which files arrived in which commit, in what order. Required for the test-first check below.
- `/workspace/agent-notes.md` may exist (the implement phase appended to it if it could not complete some part of the brief). Read it for context on deliberate gaps or known limitations.

## Test-first commit-shape check

The implement-phase kickoff mandates a test-first commit shape: one failing-test commit, then one make-it-pass commit, per acceptance criterion. Use `.bellows-review-commit-log.txt` to verify that shape. Flag a finding tagged ` — important` when you see either of these test-first violations:

- **mega-commit**: a single commit on the agent branch touches BOTH test files (`tests/**`, files containing `#[test]` / `#[tokio::test]` attributes) AND non-trivial source files at the same time. The two should land in separate commits so the make-it-pass commit demonstrates the test transitioning from red to green.
- **source-before-test**: a source-file commit lands earlier in the agent-branch history than the corresponding test commit. Tests added after the implementation are not test-first — they post-hoc rationalise whatever the implementation happens to do.

If the entire branch is a single mega-commit, that one commit is the violation; if individual commits ordered source-before-test, name each offending pair. Use the existing finding format and the `important` severity tag so the run's per-finding review-fix loop can route the finding through unchanged: the agent will get one invocation to either rewrite history to be test-first OR append an `## Unaddressed finding: <verbatim title>` section to agent-notes.md.

A diff with no test files at all is out of scope for this check — the slice-8 weak-test guard handles "no tests added" separately. Briefs that the operator labelled with the skip-label are also out of scope here; the bellows runner will already have skipped the weak-test guard for those, but the test-first check is a stylistic recommendation rather than a hard gate, so it is acceptable to skip flagging where the brief makes test-first ordering impractical (e.g. pure-docs PRs).

## Output

Write your findings to `/workspace/.bellows-review-findings.md` in this markdown format. Each finding's title line MUST end with ` — ` followed by exactly one severity tag drawn from the closed vocabulary `blocker | important | nit` — use exactly one of these three values, never invent another tag (no "medium", "minor", "follow-up", etc.). The review-fix phase keys its address-OR-explain contract on these exact strings, so a missing or off-vocabulary tag silently demotes the finding.

Additional title-format constraints (load-bearing for the bellows parser-as-backstop — the runner extracts the title verbatim and matches it against `## Unaddressed finding: <title>` sections in agent-notes.md, so any drift breaks the cross-reference):

- The title MUST be on one line. No line breaks inside a title.
- The title line MUST end with ` — <tag>` (space, em-dash, space, then the severity tag).
- The title MUST NOT contain markdown links or backticks. Plain prose only — these characters break parser extraction and silently demote the finding.

Severity meanings:

- `blocker` — the change as written is wrong, unsafe, or breaks the brief's acceptance criteria. Must be fixed before merge.
- `important` — a real bug or design flaw that survives the test suite (logic gap, leaked resource, wrong invariant). Must be fixed or escalated; should not silently ship.
- `nit` — style, naming, micro-cleanup, optional polish. Operator-discretionary; safe to skip.

Example findings file:

```
## Findings

### 1. status file leaks busy state on Rust error returns — important

The `?` early-returns in `runner::run_one` skip the cleanup that resets the status file from "busy" back to "idle", so a single error leaves the slot permanently busy and blocks future dispatches.

**Suggestion:** wrap the body in a guard that resets the status on drop, or use a `defer`-style closure before each `?`.

### 2. unwrap on parsed config can panic on empty input — blocker

`Config::from_str("")` panics inside `serde_json::from_str` rather than returning the typed error, so an empty config file crashes startup before any logging is set up.

**Suggestion:** map the serde error into the existing `ConfigError::Parse` variant.

### 3. helper function name shadows std::cmp::min — nit

`fn min(a, b)` in `src/util.rs` reads fine locally but conflicts with the prelude when imported elsewhere.

**Suggestion:** rename to `min_nonzero` or inline the two call sites.
```

If you find no issues worth flagging, write the file with a single line: `(no findings)`. The file MUST exist either way — Bellows reads it after the run and treats it as the contract for the review-fix phase.

## What this phase does NOT do

You are read-only. Do NOT edit any files except `.bellows-review-findings.md` and (optionally) `agent-notes.md`. Do NOT create commits. Do NOT push. The review-fix phase that follows you will read your findings and address them.

## When you cannot complete

If the diff is malformed, missing, or you genuinely cannot review it, append a section to `/workspace/agent-notes.md` explaining what stopped you. APPEND — do not overwrite. The file may already contain notes from the implement phase that must remain visible to the human reviewer.
"#;

/// Vendored review-fix-phase prompt — slice 9.6 per-finding shape.
///
/// This is a TEMPLATE that `per_finding_kickoff` renders with a specific
/// finding interpolated. The prompt scopes the agent to a SINGLE finding
/// per invocation: there is no list to silently skip, only one finding
/// and two options (address in code OR write an `## Unaddressed finding:
/// <verbatim title>` section to agent-notes.md). The slice-9.5 prompt's
/// "every finding marked blocker or important" framing is gone — that
/// wording is exactly what enabled four consecutive silent-skip
/// regressions (#26, #28, #30, #33), so the per-finding shape removes
/// the discretion the agent kept exercising.
///
/// Placeholders rendered by `per_finding_kickoff`:
///
/// - `{title}` — the finding's verbatim title
/// - `{severity}` — `blocker` or `important`
/// - `{body}` — the finding's description + suggestion block
/// - `{urgency}` — severity-flavoured tone line
/// - `{diff_path}` — workspace-relative path to the review diff
/// - `{agent_notes_path}` — workspace-relative path to agent-notes.md
pub const REVIEW_FIX_PROMPT: &str = r#"You are running as a **single-finding review-fix invocation** of a Bellows agent pipeline. You have ONE finding to handle. That's the entire job.

## The finding

**Title:** {title}
**Severity:** {severity}

{body}

{urgency}

## Your two options

You MUST do exactly one of the following:

1. **Address the finding in code.** Make the change that resolves the finding's root cause, run `cargo check` (or equivalent), and commit it with a scoped commit message. One commit per finding so the operator can map your fix back to the review-findings PR comment.
2. **Append an `## Unaddressed finding: <title>` section to `/workspace/{agent_notes_path}`.** Use the EXACT VERBATIM title from this finding — the bellows parser-as-backstop matches title strings character-for-character. The exact header you must append is:

```
## Unaddressed finding: {title}
```

Then a paragraph describing (a) what would be required to address the finding and (b) why you cannot address it in this run (missing context, architectural decision needed, requires human judgement, etc.).

APPEND to `/workspace/{agent_notes_path}` — do not overwrite; the file may already contain notes from earlier phases.

## Silent skip is out-of-bounds

Exiting without either a code-fix commit OR an `## Unaddressed finding: {title}` section is prompt-out-of-bounds. The bellows parser-as-backstop will detect a silent skip after this phase ends and synthesize an `## Unaddressed finding:` entry on your behalf, forcing the run to agent-self-reported-failure anyway. It is strictly better to write the section yourself with the real reason than to let the synthetic entry replace it.

## What appending to agent-notes.md signals

The presence of `/workspace/{agent_notes_path}` at the end of the pipeline routes the run to **agent-self-reported-failure**: bellows opens the resulting PR as a draft with the `agent-failed` label, attaches your notes, and surfaces the partial commits to the operator for review. This is the intended escalation path for `blocker` / `important` work you cannot complete — the operator sees the draft PR plus your notes plus the partial commits and decides what to do.

Reach for the unaddressed-finding section deliberately. It is not a "didn't get to it" note; it is a structured handoff that says "I am self-reporting this as incomplete and want a human to look."

## What you must NOT do

- Do NOT broaden scope to address other findings; you have exactly one finding to handle. Other findings are handled by other invocations of this same prompt with different findings interpolated.
- Do NOT remove the findings file (`.bellows-review-findings.md`); other per-finding invocations may still need it as context.
- Do NOT use a paraphrased title in the `## Unaddressed finding:` header. Verbatim match required.

## Inputs for context

- `/workspace/{diff_path}` contains the diff this finding is about — read it if you need disambiguation.
- `/workspace/{agent_notes_path}` may exist with notes from earlier phases. Read it for context before appending.

## Stop conditions

Stop when EITHER (1) you committed a code fix AND `cargo check` is green, OR (2) you appended the `## Unaddressed finding: {title}` section to `/workspace/{agent_notes_path}`.
"#;

/// Vendored security-review-phase prompt (slice X2). Sibling of
/// `REVIEW_PROMPT`. Same input file (`REVIEW_DIFF_FILE`, regenerated from
/// the post-review-fix workspace state so it reflects review fixups) and
/// the same markdown findings format (so the existing finding-parser
/// machinery applies cleanly), but the analysis scope is the five
/// security focus categories: input validation, authentication, crypto,
/// injection, and data exposure.
pub const SECURITY_REVIEW_PROMPT: &str = r#"You are running as the **security-review phase** of a Bellows agent pipeline. The implement → review → review-fix phases have already run; your job is to review the resulting diff for security concerns.

## Focus categories (closed list)

Look for issues in exactly these five categories. Do not expand the scope:

1. **Input validation** — untrusted input flowing into parsers, file paths, command arguments, or deserialisation without bounds checks, sanitisation, or whitelisting.
2. **Authentication and authorisation** — missing auth checks, hard-coded credentials, weakened-on-error fallbacks, broken session handling, token leakage in logs.
3. **Cryptography** — broken or homegrown crypto, hard-coded keys, weak hash algorithms, missing integrity checks, predictable nonces or random sources.
4. **Injection** — command, SQL, shell, or template injection via string interpolation that mixes untrusted input with code paths.
5. **Data exposure** — secrets in logs, error messages, or commit content; sensitive data written to world-readable locations; PII or credentials traversing unintended boundaries.

A finding outside these five categories is out of scope for this phase — flag it as a `## Unaddressed finding` section in agent-notes.md only if it materially blocks the review, otherwise leave it for the standard review phase.

## Inputs

- `/workspace/.bellows-review-diff.patch` contains `git diff <base>...HEAD` regenerated from the POST-review-fix workspace state — the entire delta the implement + review-fix phases produced. Read this file as the primary input. Do not browse the wider codebase except to disambiguate symbols referenced in the diff.
- `/workspace/agent-notes.md` may exist (prior phases may have appended to it). Read it for context on deliberate gaps or known limitations.

## Output

Write your findings to `/workspace/.bellows-security-findings.md` in the SAME markdown format as the review phase. Each finding's title line MUST end with ` — ` followed by exactly one severity tag drawn from the closed vocabulary `blocker | important | nit` — use exactly one of these three values, never invent another tag (no "medium", "minor", "follow-up", etc.). The downstream security-fix phase keys on these exact strings.

Additional title-format constraints (same load-bearing rules as the review phase, so the same parser machinery applies):

- The title MUST be on one line. No line breaks inside a title.
- The title line MUST end with ` — <tag>` (space, em-dash, space, then the severity tag).
- The title MUST NOT contain markdown links or backticks. Plain prose only.

Severity meanings (same closed vocabulary as the review phase):

- `blocker` — the change as written introduces a security vulnerability that must be fixed before merge.
- `important` — a real security weakness that survives the test suite (missing validation, weak auth boundary, leaked secret). Must be fixed or escalated.
- `nit` — minor hardening opportunity (defence-in-depth, naming, comment). Operator-discretionary.

Example findings file:

```
## Findings

### 1. shell call interpolates untrusted input without escaping — blocker

`src/runner.rs` constructs `format!("git log {}", branch_name)` and passes it to `Command::new("sh").arg("-c").arg(...)`; an attacker-controlled branch name like `master; rm -rf /` would be executed verbatim. This is the canonical command-injection shape.

**Suggestion:** pass arguments as a `&[&str]` slice to `Command::new("git").args([...])` so the shell never sees the user-controlled value.

### 2. agent-notes.md may contain secrets and is committed to the PR diff — important

The implement-phase synth embeds a prefix of the agent's stderr tail in agent-notes.md. If the agent printed an API key or OAuth token to stderr before crashing, that secret would be committed to the PR's branch and visible in the diff.

**Suggestion:** scrub well-known secret shapes (Bearer tokens, AWS keys, OAuth refresh tokens) from the embedded tail before writing it to agent-notes.md.
```

If you find no issues worth flagging, write the file with a single line: `(no findings)`. The file MUST exist either way — Bellows reads it after the run and treats it as the contract for the security-fix phase.

## What this phase does NOT do

You are read-only. Do NOT edit any files except `.bellows-security-findings.md` and (optionally) `agent-notes.md`. Do NOT create commits. Do NOT push. The security-fix phase that follows you will read your findings and address them.

## When you cannot complete

If the diff is malformed, missing, or you genuinely cannot review it, append a section to `/workspace/agent-notes.md` explaining what stopped you. APPEND — do not overwrite. The file may already contain notes from earlier phases that must remain visible to the human reviewer.
"#;

/// Vendored security-fix-phase prompt (slice X2). Sibling of
/// `REVIEW_FIX_PROMPT` but in the batch shape (single invocation handling
/// all findings) — the security-fix phase reads the findings file
/// written by `SECURITY_REVIEW_PROMPT`, addresses each finding, commits
/// each fix, and removes the findings file. Appends to `agent-notes.md`
/// if any finding can't be addressed cleanly.
pub const SECURITY_FIX_PROMPT: &str = r#"You are running as the **security-fix phase** of a Bellows agent pipeline. The security-review phase produced findings; your job is to address each one and remove the findings file.

## Inputs

- `/workspace/.bellows-security-findings.md` contains the security findings produced by the security-review phase. Each finding has a title ending in ` — blocker | important | nit`, a description, and a suggested remediation.
- `/workspace/.bellows-review-diff.patch` contains the post-review-fix diff that the security review was performed against. Read it if you need disambiguation.
- `/workspace/agent-notes.md` may exist with notes from earlier phases. Read it for context; APPEND only, never overwrite.

## Your job

For each finding in `.bellows-security-findings.md`:

1. Read the title, description, and suggestion.
2. Make the change that resolves the finding's root cause.
3. Run `cargo check` (or equivalent) after each change to confirm you have not broken compilation.
4. Commit each fix with a clear, scoped commit message — one commit per finding is ideal so the operator can map fixes back to the security-findings PR comment.

When every finding has been addressed (or explicitly escalated to agent-notes.md), delete `/workspace/.bellows-security-findings.md`. The runner uses the absence of this file as the signal that the security-fix phase is complete; leaving it behind would cause a downstream readability problem (the file would ship in the PR diff).

## When a finding cannot be addressed

If you cannot address a finding in this run (requires architectural decision, missing context, etc.), APPEND an `## Unaddressed finding: <title>` section to `/workspace/agent-notes.md` using the EXACT VERBATIM title from the finding. Then move on to the next finding. The presence of an `## Unaddressed finding:` section at the end of the pipeline routes the run to **agent-self-reported-failure** (draft PR with the `agent-failed` label), surfacing the gap to a human reviewer.

Do NOT silently skip a finding — either address it in code or escalate it via the unaddressed-finding section.

## What you must NOT do

- Do NOT broaden scope outside the five security focus categories (input validation, auth, crypto, injection, data exposure).
- Do NOT introduce new functionality beyond what's needed to address the findings — security fixes only.
- Do NOT paraphrase the finding title when writing an `## Unaddressed finding:` header; verbatim match is required.

## Stop conditions

Stop when EITHER (1) every finding has been addressed in code AND `cargo check` is green AND `.bellows-security-findings.md` has been removed, OR (2) every finding has been routed (some to code commits, the remainder to `## Unaddressed finding:` sections in agent-notes.md) AND the findings file has been removed.
"#;

/// Closed severity vocabulary for review findings. The review prompt
/// instructs the agent to tag every finding with exactly one of these
/// three values; the parser refuses anything else (it lands in
/// `ParseFindingsResult::malformed_titles` instead). The per-finding
/// enact path is keyed on the top two severities — `Nit` findings go
/// through the batch path and are operator-discretionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Blocker,
    Important,
    Nit,
}

impl Severity {
    /// The exact lower-case string the prompt instructs the review
    /// agent to use as the tag at the end of each finding's title
    /// (`blocker`, `important`, `nit`). Round-trips with
    /// `Severity::from_tag`.
    pub fn as_tag(&self) -> &'static str {
        match self {
            Severity::Blocker => "blocker",
            Severity::Important => "important",
            Severity::Nit => "nit",
        }
    }

    /// Parse a severity tag string from the end of a finding's title
    /// line. Matches the closed vocabulary `blocker | important | nit`
    /// exactly (case-insensitive). Anything else returns `None`, which
    /// the parser treats as a malformed finding.
    pub fn from_tag(s: &str) -> Option<Severity> {
        match s.trim().to_ascii_lowercase().as_str() {
            "blocker" => Some(Severity::Blocker),
            "important" => Some(Severity::Important),
            "nit" => Some(Severity::Nit),
            _ => None,
        }
    }
}

/// One review finding extracted from the review-phase output file.
/// `title` is the verbatim text between `### N. ` and ` — <tag>` on
/// the title line — the per-finding kickoff and the agent-notes
/// `## Unaddressed finding: <title>` contract both key on this exact
/// string, so it must round-trip verbatim through the pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFinding {
    pub title: String,
    pub severity: Severity,
    pub body: String,
}

/// Outcome of `parse_findings`. Carries the well-formed findings AND
/// the title lines the parser rejected because they did not end in a
/// valid severity tag. The runner logs the rejected lines so an operator
/// can see "review produced a malformed finding" rather than the parser
/// silently dropping the line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParseFindingsResult {
    pub findings: Vec<ParsedFinding>,
    pub malformed_titles: Vec<String>,
}

/// Parse a review-findings markdown file into a list of structured
/// findings + the title lines that did not match the locked grammar.
///
/// The grammar (matching `REVIEW_PROMPT`'s instructions):
///
/// - Each finding's title line starts with `### ` and ends with
///   ` — <tag>` where `<tag>` is one of `blocker | important | nit`.
/// - The title is the text between `### ` (optionally followed by
///   `N. ` numbering) and the ` — ` separator.
/// - The body is every line between the title and the next `### `
///   header (or EOF).
///
/// A `### ` header whose trailing ` — <tag>` is missing or off-vocabulary
/// is rejected — the parser pushes the line into `malformed_titles` and
/// does not produce a `ParsedFinding`. Bare `(no findings)` markers and
/// lines outside any finding are ignored.
pub fn parse_findings(text: &str) -> ParseFindingsResult {
    let mut findings = Vec::new();
    let mut malformed_titles = Vec::new();
    let mut current: Option<(String, Severity, String)> = None;

    let push_current = |current: &mut Option<(String, Severity, String)>,
                        findings: &mut Vec<ParsedFinding>| {
        if let Some((title, severity, body)) = current.take() {
            findings.push(ParsedFinding {
                title,
                severity,
                body: body.trim().to_string(),
            });
        }
    };

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("### ") {
            push_current(&mut current, &mut findings);
            // Strip an optional leading `N. ` numbering so the parser
            // matches both numbered and unnumbered title lines.
            let after_number = strip_leading_numbering(rest);
            if let Some((title, tag)) = after_number.rsplit_once(" — ") {
                if let Some(severity) = Severity::from_tag(tag) {
                    current = Some((title.trim().to_string(), severity, String::new()));
                } else {
                    malformed_titles.push(line.to_string());
                }
            } else {
                malformed_titles.push(line.to_string());
            }
            continue;
        }

        if let Some((_, _, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    push_current(&mut current, &mut findings);

    ParseFindingsResult {
        findings,
        malformed_titles,
    }
}

/// Strip an optional leading `N. ` numbering from a title line. The
/// example findings in REVIEW_PROMPT are numbered; the parser accepts
/// either form so a future tweak to the prompt's example doesn't break
/// extraction.
///
/// PR #37 review finding #3 fix: anchor the strip to require a space
/// after the period (`N. `, not `N.X`), so a title like
/// `1.5 release notes — important` doesn't get silently rewritten to
/// `5 release notes — important`. Decimal-prefixed titles aren't in
/// the prompt example today but a future operator-authored brief
/// might use them.
fn strip_leading_numbering(s: &str) -> &str {
    let trimmed = s.trim_start();
    if let Some(rest) = trimmed
        .split_once('.')
        .filter(|(n, rest)| {
            !n.is_empty()
                && n.chars().all(|c| c.is_ascii_digit())
                && rest.starts_with(' ')
        })
        .map(|(_, rest)| rest)
    {
        rest.trim_start()
    } else {
        trimmed
    }
}

/// One `## Unaddressed finding: <title>` section parsed from an
/// `agent-notes.md` file. The per-finding enact agent appends one of
/// these per finding it deliberately chose not to address in code —
/// the parser-as-backstop reads them to confirm the agent met the
/// address-OR-explain contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentNoteSection {
    pub title: String,
    pub body: String,
}

/// Parse the `## Unaddressed finding: <title>` sections out of an
/// `agent-notes.md` file. Title comparison is verbatim — the agent
/// must use the exact title from the findings file for the section to
/// match its finding. Other `## ...` headings (general notes from
/// implement / review / earlier phases) are ignored.
pub fn parse_agent_notes_sections(text: &str) -> Vec<AgentNoteSection> {
    let mut sections = Vec::new();
    let mut current: Option<(String, String)> = None;
    const PREFIX: &str = "## Unaddressed finding: ";

    let push_current = |current: &mut Option<(String, String)>,
                        sections: &mut Vec<AgentNoteSection>| {
        if let Some((title, body)) = current.take() {
            sections.push(AgentNoteSection {
                title,
                body: body.trim().to_string(),
            });
        }
    };

    for line in text.lines() {
        if let Some(title) = line.strip_prefix(PREFIX) {
            push_current(&mut current, &mut sections);
            current = Some((title.trim().to_string(), String::new()));
            continue;
        }
        // Any other `## ` heading closes the current section (without
        // emitting a new one) — we only collect Unaddressed-finding
        // sections.
        if line.starts_with("## ") {
            push_current(&mut current, &mut sections);
            continue;
        }
        if let Some((_, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    push_current(&mut current, &mut sections);

    sections
}

/// Pairing of one review finding with the bellows-side signal "did
/// this finding's per-finding invocation produce a commit?". The
/// runner accumulates one of these per `blocker`/`important` finding
/// as it loops; `compute_coverage_violations` reads the list to
/// produce the parser-as-backstop's findings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingCoverage {
    pub finding: ParsedFinding,
    pub commit_landed: bool,
}

/// The parser-as-backstop. Returns the `blocker`/`important` findings
/// that have neither an associated commit nor a matching `##
/// Unaddressed finding: <title>` section in agent-notes.md.
///
/// `nit` findings are operator-discretionary and are never violations
/// (silent skip is explicitly permitted for the nit severity).
///
/// Title comparison is verbatim — agents that paraphrase the title in
/// their agent-notes section do NOT close the loop. This is intentional;
/// the verbatim contract is what makes the cross-reference deterministic.
pub fn compute_coverage_violations(
    coverage: &[FindingCoverage],
    sections: &[AgentNoteSection],
) -> Vec<ParsedFinding> {
    coverage
        .iter()
        .filter(|c| matches!(c.finding.severity, Severity::Blocker | Severity::Important))
        .filter(|c| !c.commit_landed)
        .filter(|c| !sections.iter().any(|s| s.title == c.finding.title))
        .map(|c| c.finding.clone())
        .collect()
}

/// Build the markdown bellows appends to agent-notes.md when the
/// parser-as-backstop finds blocker/important findings that the
/// per-finding agent silently skipped (no commit, no explanation
/// section). The synthesised entries trigger the existing
/// `has_agent_notes` → `AgentSelfReportedFailure` precedence in
/// `classify_exit`, ensuring the run opens as a draft PR with the
/// `agent-failed` label rather than shipping silently as Success.
///
/// Each entry uses the verbatim finding title so a reader can map it
/// back to the review-findings PR comment. The body identifies bellows
/// as the author so a human reviewing agent-notes.md doesn't mistake
/// the synthesised entry for one the agent wrote.
///
/// Returns an empty string when there are no violations. The runner
/// should only call this when violations are present, but the empty
/// path is defined so a zero-violation call cannot accidentally
/// produce a header-only stub that would itself route to
/// AgentSelfReportedFailure.
pub fn synthesize_unaddressed_entries(violations: &[ParsedFinding]) -> String {
    if violations.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str(
        "\n\n<!-- bellows parser-as-backstop appended these entries because the per-finding \
         review-fix invocations exited without addressing the findings in code and without \
         appending an Unaddressed finding section. The presence of these entries forces the \
         run to agent-self-reported-failure (draft PR + agent-failed label). -->\n",
    );
    for v in violations {
        out.push_str(&format!(
            "\n## Unaddressed finding: {title}\n\n\
             Bellows-synthesised entry. The per-finding review-fix invocation for this \
             {severity} finding exited without making a commit and without appending its \
             own `## Unaddressed finding:` section. The address-OR-explain contract requires \
             one of those; the parser-as-backstop synthesised this entry so the run routes \
             to agent-self-reported-failure and a human reviewer sees the gap.\n",
            title = v.title,
            severity = v.severity.as_tag(),
        ));
    }
    out
}

/// Build the `### Address-or-explain contract violated` callout that
/// the runner injects into the PR's run-log comment when the
/// parser-as-backstop fires. Names each offending finding (verbatim
/// title + severity) so the operator can see exactly which findings
/// the per-finding agent silently skipped — surfacing the violation
/// explicitly is the difference between a confused "why is this
/// agent-failed?" PR and an actionable "the agent silently skipped
/// finding X" PR.
pub fn build_violation_callout(violations: &[ParsedFinding]) -> String {
    let mut out = String::from("\n### Address-or-explain contract violated\n\n");
    out.push_str(
        "The parser-as-backstop detected blocker/important findings that the per-finding \
         review-fix invocations neither addressed in code nor explained via an `## \
         Unaddressed finding:` section in agent-notes.md. Bellows synthesised the missing \
         entries to force this run to agent-self-reported-failure. Offending findings:\n\n",
    );
    for v in violations {
        out.push_str(&format!(
            "- **{severity}** — {title}\n",
            severity = v.severity.as_tag(),
            title = v.title,
        ));
    }
    out
}

/// Vendored single-nit-batch prompt for the review-fix phase's batched
/// nit invocation. Permissive: silent skip is explicitly allowed for
/// nits because the operator already sees every finding in the
/// review-findings PR comment and can choose whether to follow up.
pub const BATCH_REVIEW_FIX_NIT_PROMPT: &str = r#"You are running as the **batched nit-fix invocation** of a Bellows agent pipeline. The review phase produced one or more `nit`-severity findings; your job is to address the easy / adjacent ones and skip the rest.

## The permissive contract

`nit` findings are operator-discretionary. You MAY skip a `nit` without explanation — the operator already sees every finding in the review-findings PR comment and can decide whether to follow up. Silent skip IS allowed for nits.

Apply the cheap, in-scope ones. Skip cosmetic findings that would burn time. Do NOT append to agent-notes.md for nits — appending routes the run to agent-self-reported-failure (draft PR + agent-failed label), which is too heavy for a nit you simply chose not to do.

## Inputs

- The list of nit findings is interpolated at the top of this kickoff (one per `### ` block).
- `/workspace/agent-notes.md` may exist with notes from earlier phases. Read it for context. APPEND only — do not overwrite.

## Process

For each nit finding you decide to address:

1. Read the title, description, and suggestion.
2. Make the change. Run `cargo check` (or equivalent) after each change to confirm you have not broken compilation.
3. Commit each fix with a clear, scoped commit message. One commit per finding is ideal so the operator can map fixes back to the review-findings PR comment.

For the nits you skip: do nothing. No note, no commit.

## Stop conditions

Stop when you have made the changes you intend to make and `cargo test` is green. The operator sees the review-findings comment regardless; nothing here is mandatory.
"#;

/// Build the per-finding `claude -p` kickoff body for a single
/// `blocker` or `important` finding. Pure function so it can be
/// unit-tested without spinning up a container.
///
/// Renders `REVIEW_FIX_PROMPT` as a template with the specific finding
/// interpolated. The agent sees exactly one finding — there is no list
/// to silently skip — and must either address it in code OR append a
/// `## Unaddressed finding: <verbatim title>` section to
/// `agent-notes.md`. Severity flavours the urgency line so a `blocker`
/// reads as more urgent than an `important`.
///
/// The `diff_path` and `agent_notes_path` arguments are interpolated
/// into the inputs section so the agent knows where to read the diff
/// and where to append the unaddressed-finding section. Passed as
/// arguments rather than hardcoded so the function stays pure and the
/// runner can re-use it across phase boundaries.
pub fn per_finding_kickoff(
    finding: &ParsedFinding,
    diff_path: &str,
    agent_notes_path: &str,
) -> String {
    let urgency = match finding.severity {
        Severity::Blocker => "This is a **blocker**: the change as written is wrong, unsafe, or breaks the brief's acceptance criteria. It MUST be fixed before merge — escalation via the unaddressed-finding section is reserved for genuinely impossible cases, not for cases that are merely hard.",
        Severity::Important => "This is an **important** finding: a real bug or design flaw that survives the test suite (logic gap, leaked resource, wrong invariant). It must be fixed or escalated via the unaddressed-finding section; it should not silently ship.",
        // PR #37 review finding #2 fix: nits flow through the batch
        // nit prompt, NOT this per-finding path. A nit reaching here
        // means the caller (the runner's per-finding loop) is buggy.
        // Previous "address-it-or-skip" fallback contradicted the
        // surrounding template's mandate ("silent skip is
        // prompt-out-of-bounds"), producing an incoherent kickoff;
        // unreachable! is the right reaction.
        Severity::Nit => unreachable!(
            "per_finding_kickoff received a Nit finding; nits must go through \
             BATCH_NIT_PROMPT, not the per-finding path. This is a runner bug."
        ),
    };

    REVIEW_FIX_PROMPT
        .replace("{title}", &finding.title)
        .replace("{severity}", finding.severity.as_tag())
        .replace("{body}", &finding.body)
        .replace("{urgency}", urgency)
        .replace("{diff_path}", diff_path)
        .replace("{agent_notes_path}", agent_notes_path)
}

/// Canonical title of the synthetic `## Unaddressed finding:` entry the
/// slice-8 weak-test guard appends to `agent-notes.md` when an implement
/// run produced changes but no new Rust test attributes. Verbatim per the
/// brief; the agent-notes parser (and any future cross-reference) keys
/// on this exact string.
pub const NO_NEW_TESTS_FINDING_TITLE: &str = "no new tests added";

/// Detect whether a unified diff adds at least one new Rust test
/// attribute. Used by the slice-8 weak-test guard: an agent that ships
/// implementation code with no new tests trips a green cargo gate but
/// is otherwise indistinguishable from a real Success — the post-hoc
/// diff scan is the only mechanical post-run check that catches it.
///
/// Recognises the common attribute shapes: `#[test]`, `#[tokio::test]`,
/// `#[async_std::test]`, `#[wasm_bindgen_test]`, `#[rstest]`,
/// `#[test_case(...)]`, `#[proptest]`. Each may optionally carry a
/// `(...)` argument list (e.g. `#[tokio::test(flavor = "multi_thread")]`)
/// so the patterns match prefixes rather than full bracketed forms.
///
/// Scan discipline (the heuristic that keeps this useful AND honest):
///
/// - Only lines starting with a single `+` are considered (added lines).
///   `+++ b/path` file-header lines and ` ` context lines are skipped.
/// - A `-` removed-only line is NOT a new test attribute even if it
///   names one — a refactor that deletes a test must not pass the guard.
/// - Lines whose first non-whitespace content is `//` are treated as
///   line comments and skipped. `// #[test]` in a doc string or example
///   is not a real test attribute; the brief explicitly calls this
///   false-positive case out.
///
/// Limitations (deliberately out of scope for the guard's presence
/// check — the triage gate + human review remain the primary defences
/// against weak tests): block-comment-style `/* #[test] */` and string
/// literals containing the substring are not filtered. Both are rare
/// enough in real test suites that the cost of false-positives is
/// preferable to the parser complexity needed to handle them.
pub fn has_new_tests(diff: &str) -> bool {
    const ATTR_PATTERNS: &[&str] = &[
        "#[test]",
        "#[tokio::test",
        "#[async_std::test",
        "#[wasm_bindgen_test",
        "#[rstest",
        "#[test_case",
        "#[proptest",
    ];
    for line in diff.lines() {
        // File-header marker (`+++ b/path`). Not an added content line.
        if line.starts_with("+++") {
            continue;
        }
        let Some(rest) = line.strip_prefix('+') else {
            continue;
        };
        let trimmed = rest.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        if ATTR_PATTERNS.iter().any(|p| trimmed.contains(p)) {
            return true;
        }
    }
    false
}

/// Detect whether a unified diff touches at least one `.rs` file
/// (added or modified). Issue #103: the slice-8 weak-test guard
/// previously fired on every implement-phase diff that lacked new
/// Rust test attributes, including doc-only briefs (ADRs, markdown
/// updates) whose diffs carry no Rust source at all. The runner uses
/// this helper to short-circuit the guard when there is nothing
/// Rust-shaped in the diff for `has_new_tests` to score against.
///
/// Scan discipline:
///
/// - Looks at `diff --git a/<path> b/<path>` headers and the
///   `+++ b/<path>` "new file" marker. Either is sufficient to
///   declare a `.rs` file touched.
/// - Keys on the `.rs` *extension* at the end of the path rather
///   than the substring `.rs` anywhere in the line. A path like
///   `docs/rs-notes.md` contains the substring but is not a Rust
///   source file; the helper must not be confused by it.
/// - Skips `+++ /dev/null` (the "file deleted" marker on the new
///   side of a deletion-only diff). A pure deletion of a `.rs`
///   file is still a Rust change for the guard's purpose because
///   the `diff --git` header on the same hunk names the path, so
///   the diff is correctly counted via the `diff --git` line.
/// - Empty input returns `false` — a no-op diff touches no files
///   of any kind, which is the right semantics for the runner's
///   short-circuit (an empty diff has no implementation either,
///   and the guard's outer gating handles that branch independently).
pub fn diff_contains_rs_files(diff: &str) -> bool {
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // `diff --git a/<path> b/<path>`. Splitting on whitespace
            // yields the two path tokens; either should end in `.rs`
            // for a real Rust-source change (git always renders both
            // sides even for added/deleted files).
            if rest
                .split_whitespace()
                .any(|tok| path_token_is_rust(tok))
            {
                return true;
            }
        } else if let Some(path) = line.strip_prefix("+++ b/") {
            if path_ends_with_rs(path) {
                return true;
            }
        } else if let Some(path) = line.strip_prefix("--- a/") {
            if path_ends_with_rs(path) {
                return true;
            }
        }
    }
    false
}

/// True when a `diff --git` path token (`a/foo.rs` or `b/foo.rs`)
/// names a Rust source file. Strips the `a/` or `b/` prefix git uses
/// to disambiguate the old and new sides before extension-matching so
/// a path like `a/src/lib.rs` is recognised but `a/docs/rs-notes.md`
/// is not.
fn path_token_is_rust(token: &str) -> bool {
    let path = token
        .strip_prefix("a/")
        .or_else(|| token.strip_prefix("b/"))
        .unwrap_or(token);
    path_ends_with_rs(path)
}

/// True when a path ends in the `.rs` extension. Anchored on the end
/// of the string so a substring match elsewhere in the path (e.g.
/// `docs/rs-notes.md`) does not register.
fn path_ends_with_rs(path: &str) -> bool {
    // Trim trailing whitespace defensively — diff headers should not
    // carry any, but a tabbed timestamp on the `+++` line (rare, but
    // some `git diff` configurations emit it) would otherwise mask
    // the extension.
    let path = path.trim_end();
    path.ends_with(".rs")
}

/// Build the markdown the slice-8 weak-test guard appends to
/// `agent-notes.md` when the post-implement diff contains no new Rust
/// test attributes (and the issue does not carry the skip-label). The
/// section's title is the canonical `NO_NEW_TESTS_FINDING_TITLE`
/// constant so a parser cross-reference matches verbatim; the body
/// identifies bellows as the author so a human reviewing
/// `agent-notes.md` later isn't confused about provenance.
///
/// Reuses the existing slice-9.6 mechanism rather than introducing a
/// new pipeline phase: the presence of an `## Unaddressed finding:`
/// section triggers `classify_exit`'s `has_agent_notes` precedence,
/// routing the run to `AgentSelfReportedFailure` and producing a draft
/// PR with the `agent-failed` label.
pub fn synthesize_no_new_tests_entry() -> String {
    format!(
        "\n\n<!-- bellows weak-test guard appended this entry because the implement phase \
         produced changes against the base branch with no new Rust test attributes \
         (#[test], #[tokio::test], etc.) and the issue did not carry the configurable \
         skip-label. The presence of this entry forces the run to agent-self-reported-failure \
         (draft PR + agent-failed label) so a human reviewer sees the gap. -->\n\
         \n\
         ## Unaddressed finding: {title}\n\
         \n\
         Bellows-synthesised entry. The implement phase produced a diff against the base \
         branch with no new Rust test attributes detected by the slice-8 weak-test guard. \
         A green cargo-checks gate over an unchanged test suite is a poor signal of \
         correctness; the brief's acceptance criteria typically require accompanying \
         tests. The weak-test guard synthesised this entry so the run routes to \
         agent-self-reported-failure for a human reviewer.\n",
        title = NO_NEW_TESTS_FINDING_TITLE,
    )
}

/// Maximum bytes of captured stderr/stdout tail that the implement-crash
/// synth embeds in `agent-notes.md`. The sandbox already caps the raw
/// `stderr_tail` at 64KB; for the synth note (which ships in the PR diff
/// AND the agent-notes commit body) a tighter bound keeps the entry
/// human-readable while still leaving plenty of room to fingerprint the
/// underlying failure. The trim is char-boundary-aware (`char_indices`)
/// so a multibyte glyph at the boundary cannot slice through UTF-8.
const IMPLEMENT_CRASH_TAIL_CAP_BYTES: usize = 4 * 1024;

/// Build the markdown bellows appends to `agent-notes.md` when the
/// implement phase exits non-zero AND produced no commits — typical of
/// an early-exit crash (sandbox setup failure, container start failure,
/// immediate Anthropic error, etc.) where the agent never wrote
/// anything to the workspace.
///
/// Without this synth, `workspace::commit_all` would return
/// `NoChangesToCommit` and the legacy commit/push path produced no
/// branch on origin — `open_pr` then either fails or opens a
/// no-content PR, leaving the source issue stuck at `agent-in-progress`
/// with no PR, no `agent-failed` label, and no log comment.
///
/// The synth gives the run a single, bellows-authored commit on the
/// `agent/<N>-...` branch so the rest of the pipeline (the existing
/// `halt_after_post_implement` → `classify_exit` → `finalise` path)
/// runs through to completion: draft PR opens against the default
/// branch, the issue's label transitions from `agent-in-progress` to
/// `agent-failed`, and the standard `<details>` log comment posts on
/// the PR.
///
/// The synth note uses an `## Implement phase crashed` heading
/// (deliberately NOT an `## Unaddressed finding:` heading) so it does
/// not collide with the slice-9.6 / slice-8 helpers — those produce
/// `## Unaddressed finding:` sections which `parse_agent_notes_sections`
/// keys on to drive the address-or-explain coverage check. The
/// implement-crash synth is a separate concern (different routing:
/// `Crash`, not `AgentSelfReportedFailure`) and must not pollute that
/// parser's view of the file.
///
/// The body identifies bellows as the author so a human reviewing
/// agent-notes.md later isn't confused about provenance, surfaces the
/// implement-phase exit code, and embeds a bounded prefix of the
/// captured stderr/stdout tail so the operator can diagnose the
/// underlying failure (CRLF shebang, missing image, OAuth expiry, ...)
/// without having to fetch container logs.
pub fn synthesize_implement_crash_entry(exit_code: i64, stderr_tail: &str) -> String {
    let truncated = if stderr_tail.len() <= IMPLEMENT_CRASH_TAIL_CAP_BYTES {
        stderr_tail.to_string()
    } else {
        let mut cut = IMPLEMENT_CRASH_TAIL_CAP_BYTES;
        while cut > 0 && !stderr_tail.is_char_boundary(cut) {
            cut -= 1;
        }
        format!(
            "{}\n... (truncated; full tail in the bellows.log)",
            &stderr_tail[..cut],
        )
    };
    let tail_block = if truncated.trim().is_empty() {
        "_(No agent output was captured before termination.)_".to_string()
    } else {
        format!("```\n{}\n```", truncated)
    };
    format!(
        "\n\n<!-- bellows implement-crash recovery appended this entry because the \
         implement-phase agent exited non-zero AND produced no commits in the workspace. \
         Without this entry the workspace would have no changes to commit, the agent \
         branch would never be pushed, and the source issue would silently stay at \
         agent-in-progress. The presence of this entry lets the rest of the pipeline \
         run through to a draft PR + agent-failed label. -->\n\
         \n\
         ## Implement phase crashed\n\
         \n\
         Bellows-synthesised entry. The implement-phase agent exited with code \
         `{exit_code}` and produced no commits in the workspace; no agent-authored \
         changes survived. A captured prefix of the agent's stderr/stdout tail \
         follows so the operator can diagnose the failure without fetching the \
         container's logs.\n\
         \n\
         {tail_block}\n",
    )
}

/// Render the kickoff prompt that gets fed into `claude -p` inside the
/// sandbox. Pure function so it can be unit-tested without spinning up
/// a container.
///
/// Engine-aware via `render_kickoff_for_engine` (issue #81 / ADR-0005):
/// the Claude path is unchanged (operating context auto-loads from
/// `CLAUDE.md` + on-demand skill reads); the Codex path inlines the
/// operating-context body + baked skill bodies directly into the
/// kickoff prompt itself, because codex does not have an equivalent
/// on-demand discovery mechanism. This wrapper preserves the v1
/// `render_kickoff(brief, repo, branch)` signature (one source of
/// truth for the failing-test commit-shape language), and delegates
/// to the engine-aware function with `Engine::Claude` so the existing
/// tests and call sites stay green.
pub fn render_kickoff(brief: &str, repo_url: &str, branch_name: &str) -> String {
    render_kickoff_for_engine(crate::config::Engine::Claude, brief, repo_url, branch_name)
}

/// Engine-aware kickoff renderer (issue #81 / ADR-0005). For
/// `Engine::Claude` produces the canonical body the v1 single-engine
/// path always produced. For `Engine::Codex` prepends the operating-
/// context body + the bodies of all baked skills (tdd, diagnose,
/// triage) so codex sees the same operating instructions claude would
/// auto-discover via `CLAUDE.md` + on-demand file reads.
pub fn render_kickoff_for_engine(
    engine: crate::config::Engine,
    brief: &str,
    repo_url: &str,
    branch_name: &str,
) -> String {
    let body = base_kickoff_body(brief, repo_url, branch_name);
    wrap_phase_prompt_for_engine(engine, &body)
}

/// Wrap a phase-specific prompt body in engine-aware operating context.
/// For `Engine::Claude` this is the identity function — Claude reads
/// `CLAUDE.md` + the skills directory from disk so the runner doesn't
/// need to repeat them in every kickoff. For `Engine::Codex` this
/// prepends the operating-context body + baked skill bodies inline,
/// because codex does not have an equivalent on-demand discovery
/// mechanism (per ADR-0005: "the codex path in `policy::render_kickoff`
/// inlines the operating-context body plus the bodies of all baked
/// skills directly into the kickoff prompt").
///
/// The same wrapper applies to all agent-invoking phases (implement,
/// review, review-fix's per-finding + nit-batch invocations, security-
/// review, security-fix) so codex sees the same operating instructions
/// at every phase boundary — there is no per-phase divergence in what
/// the operating context says.
pub fn wrap_phase_prompt_for_engine(
    engine: crate::config::Engine,
    body: &str,
) -> String {
    match engine {
        crate::config::Engine::Claude => body.to_string(),
        crate::config::Engine::Codex => {
            // Inline the operating-context body + baked skill bodies.
            // Claude-specific phrasing in those bodies ("Claude Code
            // running headless...", "your skills directory") is
            // neutralised via `neutralise_claude_phrasing_for_codex`
            // so the codex agent does not receive a kickoff that
            // calls it "Claude Code" or points it at a skills
            // directory it does not have. The phase-specific `body`
            // is *not* neutralised — it is written by bellows for
            // the agent currently in hand, so any "Claude Code"
            // reference there is intentional.
            let mut prepended = String::new();
            prepended.push_str("# Operating context\n\n");
            prepended.push_str(CODEX_INLINED_OPERATING_CONTEXT);
            prepended.push_str("\n\n# Baked skills\n\n");
            prepended.push_str(
                "The following skill bodies are inlined here because codex does \
                 not auto-load them from a skills directory. Reach for them \
                 whenever they apply.\n\n",
            );
            prepended.push_str("## Skill: tdd\n\n");
            prepended.push_str(CODEX_INLINED_SKILL_TDD);
            prepended.push_str("\n\n## Skill: diagnose\n\n");
            prepended.push_str(CODEX_INLINED_SKILL_DIAGNOSE);
            prepended.push_str("\n\n## Skill: triage\n\n");
            prepended.push_str(CODEX_INLINED_SKILL_TRIAGE);
            prepended.push_str("\n\n---\n\n");

            let mut out = neutralise_claude_phrasing_for_codex(&prepended);
            out.push_str(body);
            out
        }
    }
}

fn base_kickoff_body(brief: &str, repo_url: &str, branch_name: &str) -> String {
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
         ## Commit shape (test-first)\n\
         \n\
         The TDD skill is not just a guideline here — it is a load-bearing requirement on the *commit shape* of this branch, because the review phase reads the commit log and flags violations as `important` findings:\n\
         \n\
         - For each acceptance criterion in the brief, produce TWO commits in order: first a **failing-test commit** that adds the test(s) and would fail against the unchanged source, then a **make-it-pass commit** that changes the source so those tests pass.\n\
         - One failing-test commit then one make-it-pass commit, per acceptance criterion. Do NOT bundle tests and source into a single mega-commit. Do NOT land source-file changes before their corresponding tests.\n\
         - It is fine to add small refactors as separate follow-up commits after the make-it-pass commit. The constraint is on test-vs-source ordering, not on commit count overall.\n\
         - If an acceptance criterion is genuinely impossible to drive test-first (e.g. a pure-prompt-text change with no observable behaviour), call that out in `agent-notes.md` rather than silently bundling tests and source.\n\
         \n\
         ## Stop conditions\n\
         \n\
         Stop only when `cargo test` is green and your changes satisfy every acceptance criterion in the brief above.\n\
         Do NOT write a `.bellows-stub-marker` (or any other marker) file — the slice-2 stub agent is gone; only your real changes should appear in the resulting commit.\n\
         \n\
         When you are done, write a PR description body to `/workspace/.bellows-pr-description.md` summarising what you built, mapping each new test to the brief's acceptance criteria.\n"
    )
}

/// Codex operating-context body. Bellows's policy image bakes the
/// `CLAUDE.md` operating context for claude (auto-discovered from
/// `/home/bellows/.claude/CLAUDE.md`); codex does not have an
/// equivalent auto-discovery mechanism, so the body is inlined into
/// every codex kickoff. This is the raw `CLAUDE.md` content;
/// `wrap_phase_prompt_for_engine` runs it (together with the inlined
/// skill bodies) through `neutralise_claude_phrasing_for_codex`
/// before pushing it into the codex prompt, so claude-specific
/// phrasing ("Claude Code", "your skills directory") does not leak
/// through.
pub const CODEX_INLINED_OPERATING_CONTEXT: &str = include_str!(
    "../policy-image/CLAUDE.md"
);

/// Strip claude-specific phrasing from policy-image content before
/// inlining it into a codex kickoff. The codex container has no
/// skills directory (skill bodies are inlined into the prompt
/// instead, per ADR-0005), and the identity claim "Claude Code
/// running headless" is wrong for a codex agent. Both must be
/// rewritten so the codex agent gets a coherent kickoff. Applied to
/// the operating-context body *and* the baked-skill bodies, since
/// any of those may have been authored in claude's voice.
fn neutralise_claude_phrasing_for_codex(claude_flavored: &str) -> String {
    claude_flavored
        .replace("Claude Code agent", "the agent")
        .replace("Claude Code", "the agent")
        .replace(
            "that lives in your skills directory",
            "(its body is inlined in the baked-skills section above)",
        )
        .replace(
            "look for it under your skills directory",
            "look for its body in the baked-skills section above",
        )
}

/// Inlined body of the `tdd` baked skill — per ADR-0005, codex's
/// kickoff carries each baked skill's body verbatim because codex has
/// no on-demand skill discovery (claude reads
/// `~/.claude/skills/tdd/SKILL.md` lazily when the kickoff names it).
pub const CODEX_INLINED_SKILL_TDD: &str = include_str!(
    "../policy-image/skills/tdd/SKILL.md"
);

/// Inlined body of the `diagnose` baked skill. Same rationale as the
/// `tdd` skill above.
pub const CODEX_INLINED_SKILL_DIAGNOSE: &str = include_str!(
    "../policy-image/skills/diagnose/SKILL.md"
);

/// Inlined body of the `triage` baked skill. Same rationale as above.
pub const CODEX_INLINED_SKILL_TRIAGE: &str = include_str!(
    "../policy-image/skills/triage/SKILL.md"
);
