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

/// Whether the given text contains a known Anthropic API rate-limit
/// signature. Used by `classify_exit` to distinguish a rate-limit
/// failure from a generic crash so the operator gets the right
/// follow-up signal ("wait for the rate-limit window to clear and
/// re-run" vs "investigate").
///
/// Matches case-insensitively against the underscore-style identifiers
/// Anthropic uses in API error responses (`rate_limit_error`,
/// `rate_limited`). Bare HTTP `429` is deliberately NOT matched — too
/// false-positive-prone (port numbers, test fixtures, JSON byte
/// counts, etc.).
pub fn is_rate_limit_signature(text: &str) -> bool {
    const SIGNATURES: [&str; 2] = ["rate_limit_error", "rate_limited"];
    let lower = text.to_lowercase();
    SIGNATURES.iter().any(|sig| lower.contains(sig))
}

/// Whether the given text contains a known Anthropic / Claude Code
/// auth-error signature. Used by the log-body builder to surface a
/// clear "run `bellows refresh-auth`" pointer when a non-zero phase
/// exit was caused by an expired OAuth refresh token rather than a
/// generic crash. Mirrors `is_rate_limit_signature` in shape.
///
/// Matches case-insensitively. The signature set starts small and is
/// extended over time as real-world failure modes surface — current
/// entries cover the literal `"401 unauthorized"` HTTP status line,
/// the underscore-style `"refresh_token_expired"` identifier
/// Anthropic returns in API error payloads, and the human-readable
/// `"authentication failed"` phrase that appears in Claude Code's
/// stderr when its OAuth session is rejected.
pub fn is_auth_error_signature(text: &str) -> bool {
    const SIGNATURES: [&str; 3] = [
        "401 unauthorized",
        "refresh_token_expired",
        "authentication failed",
    ];
    let lower = text.to_lowercase();
    SIGNATURES.iter().any(|sig| lower.contains(sig))
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

/// Vendored review-phase prompt. Documents the input file path
/// (REVIEW_DIFF_FILE), the output file path (REVIEW_FINDINGS_FILE),
/// the findings markdown format with a closed `blocker | important |
/// nit` severity vocabulary, and the agent-notes append-not-overwrite
/// contract. Bellows-specific (operates on a local diff instead of
/// `gh pr diff`) so the container stays GitHub-credential-free.
pub const REVIEW_PROMPT: &str = r#"You are running as the **review phase** of a Bellows agent pipeline. The implement phase has already produced changes on this branch; your job is to review the diff for correctness, maintainability, project conventions, and test coverage.

## Inputs

- `/workspace/.bellows-review-diff.patch` contains `git diff <base>...HEAD` — the entire delta the implement phase produced. Read this file as the primary input. Do not browse the wider codebase except to disambiguate symbols referenced in the diff; the patch is the contract.
- `/workspace/agent-notes.md` may exist (the implement phase appended to it if it could not complete some part of the brief). Read it for context on deliberate gaps or known limitations.

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
