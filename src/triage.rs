//! Slice T2 (#22): backlog drain for `bellows triage`. Iterates the
//! open `needs-triage` issues serially and tallies the per-issue
//! verdicts into an end-of-run summary. The per-issue triage call
//! itself (T1 / issue #21) is injected as an async closure so the
//! drain logic is testable independently of a live container.
//!
//! Serial-by-design: workspace state flows between issues. If issue N
//! produces a `wontfix` verdict that commits a new
//! `.out-of-scope/` precedent and pushes it, issue N+1's container
//! picks up the updated workspace state because bellows's working
//! copy on the host reflects the commit. Parallelising the drain
//! would break that property (T1's per-issue isolation is on the
//! sandbox-container axis, not the workspace axis).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::tracker::{Issue, IssueBundle};

/// Workspace-relative path of the bundle file bellows writes BEFORE
/// the triage container starts. The container reads this file and
/// produces a verdict.
pub const TRIAGE_INPUT_FILE: &str = ".bellows-triage-input.md";

/// Workspace-relative path of the verdict JSON file the triage
/// container writes. Bellows reads + validates this after the
/// container exits.
pub const TRIAGE_VERDICT_FILE: &str = ".bellows-triage-verdict.json";

/// Thin bellows-specific triage shim. Defers to the baked `/triage`
/// skill at `~/.claude/skills/triage/` (copied into the bellows user's
/// `.claude/skills/` at container start by `entrypoint-user` from
/// `/opt/bellows-policy/skills/triage/`) for the canonical role
/// taxonomy, brief templates, AI-disclaimer wording, and decision
/// heuristics. The shim's job is narrow: (a) cross-reference the
/// verdict JSON schema bellows validates on the host, and (b) apply a
/// headless-override layer (no `gh`, no human-wait, output is a file
/// not GitHub mutations).
///
/// The verbose role-taxonomy / brief-template reimplementation that
/// shipped with slice T1 has moved into the baked skill's `.md` files.
/// Editing the skill propagates into the bellows triage flow on the
/// next policy-image rebuild — no kickoff edit required.
pub const TRIAGE_PROMPT: &str = r#"You are the **triage agent** for one GitHub issue inside a Bellows sandbox.

**Read the canonical `/triage` skill at `~/.claude/skills/triage/` before deciding anything.** It is the source of truth for the role taxonomy, the `## Agent Brief` / `## Human Brief` templates, the AI-disclaimer wording, the `needs-info` vs grill heuristics, and the `wontfix` + `enhancement` precedent flow.

## Headless override

You are not running the skill end-to-end. The skill assumes `gh` and a human; neither is true here.

- No `gh` CLI is available in this container and no GitHub credentials are present — any `gh` invocation will fail. Do not try to mutate labels, post comments, or close the issue yourself; bellows owns those host-side once it reads your verdict.
- This is headless: no human will respond. If the skill suggests asking the reporter, capture the questions in `comment_body` under the `needs-info` state instead of waiting.
- Output is a single JSON verdict file. Bellows parses + validates it, then applies the verdict (label transitions, comments, close-issue, `.out-of-scope/` writes) on the host.

## Inputs

- `/workspace/.bellows-triage-input.md` — the issue bundle (number, title, body, labels, ordered comments). Read first.
- `~/.claude/skills/triage/` — canonical skill (taxonomy, templates, heuristics).
- The wider `/workspace` is a clone of the repo. You MAY read `CONTEXT.md`, `docs/adr/`, `.out-of-scope/`, sources to inform the verdict; the only file you write is the verdict.

## Verdict JSON schema (bellows host-side contract)

Write `/workspace/.bellows-triage-verdict.json`:

```json
{
  "category": "bug" | "enhancement",
  "state": "needs-info" | "ready-for-agent" | "ready-for-human" | "wontfix",
  "reasoning": "short prose: why this verdict",
  "comment_body": "1-2 sentence routing note posted on the issue (bellows prepends the AI-disclaimer); point to the questions, brief, or precedent",
  "agent_brief": "REQUIRED iff state=ready-for-agent; `## Agent Brief` per the skill template",
  "human_brief": "REQUIRED iff state=ready-for-human; `## Human Brief` per the skill template",
  "out_of_scope_filename": "REQUIRED iff state=wontfix AND category=enhancement; slug filename, e.g. \"auto-rerun.md\"",
  "out_of_scope_content": "REQUIRED iff state=wontfix AND category=enhancement; markdown body for the precedent",
  "close_issue": "REQUIRED true iff state=wontfix; absent or false otherwise"
}
```

Fields not relevant to the chosen state MUST be absent (not present with `null` or empty strings). Bellows rejects mismatches, leaving the issue untouched.

`comment_body` is only the top-level issue comment. Keep it to a 1-2 sentence pointer to the follow-up questions, `agent_brief`, `human_brief`, or out-of-scope precedent. Do not duplicate `agent_brief` or `human_brief` content there.

Good comment_body: "Routing this to an agent. See `agent_brief` for the implementation contract."
Bad comment_body: ## Agent Brief ...full brief pasted here...

When unsure, default to `needs-info` — the skill explains why.

## Stop condition

Stop when a valid verdict JSON is written to `/workspace/.bellows-triage-verdict.json`. A missing or malformed verdict file is the halt-on-failure signal for bellows.
"#;

/// Render the bundle as the markdown file the triage container reads.
/// Pure function — `bellows triage <N>` calls `fetch_issue_with_comments`
/// on the host to build the `IssueBundle`, then writes the output of
/// this function into the workspace's `.bellows-triage-input.md`
/// BEFORE starting the container.
pub fn render_triage_input(bundle: &IssueBundle) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Issue #{} — {}\n\n", bundle.number, bundle.title));

    out.push_str("## Current labels\n\n");
    if bundle.labels.is_empty() {
        out.push_str("_(no labels)_\n\n");
    } else {
        for l in &bundle.labels {
            out.push_str(&format!("- `{}`\n", l));
        }
        out.push('\n');
    }

    out.push_str("## Body\n\n");
    match &bundle.body {
        Some(b) if !b.trim().is_empty() => {
            out.push_str(b.trim_end());
            out.push_str("\n\n");
        }
        _ => out.push_str("_(no body)_\n\n"),
    }

    out.push_str("## Comments\n\n");
    if bundle.comments.is_empty() {
        out.push_str("_(no comments)_\n");
    } else {
        for (i, c) in bundle.comments.iter().enumerate() {
            out.push_str(&format!("### Comment {}\n\n{}\n\n", i + 1, c.trim_end()));
        }
    }
    out
}

/// Render the kickoff prompt the triage container reads (the contents
/// of `.bellows-kickoff.md`). A thin shim built on top of
/// `TRIAGE_PROMPT`; the canonical role taxonomy and brief templates
/// live in the baked skill at `~/.claude/skills/triage/` (copied by
/// `entrypoint-user` from `/opt/bellows-policy/skills/triage/`).
pub fn render_triage_kickoff() -> String {
    format!(
        "Read the issue bundle at `/workspace/{TRIAGE_INPUT_FILE}` and write a verdict to `/workspace/{TRIAGE_VERDICT_FILE}`.\n\n{TRIAGE_PROMPT}"
    )
}

/// Render a human-readable preview of the verdict for `--dry-run`
/// mode. Surfaces the state, a preview of `comment_body`, the
/// brief (when relevant), and the wontfix-enhancement file-write
/// preview. No GitHub or git mutations are implied — this is what
/// the operator sees on stdout when they say "show me what the
/// agent decided but don't apply it".
pub fn render_dry_run_report(verdict: &TriageVerdict) -> String {
    let mut out = String::new();
    out.push_str("== bellows triage dry-run ==\n");
    out.push_str(&format!(
        "state:    {} ({:?})\n",
        verdict.state.label(),
        verdict.category,
    ));
    out.push_str(&format!("reasoning: {}\n", verdict.reasoning));
    out.push_str("\n-- comment_body --\n");
    out.push_str(&verdict.comment_body);
    out.push('\n');

    if let Some(brief) = &verdict.agent_brief {
        out.push_str("\n-- agent_brief --\n");
        out.push_str(brief);
        out.push('\n');
    }
    if let Some(brief) = &verdict.human_brief {
        out.push_str("\n-- human_brief --\n");
        out.push_str(brief);
        out.push('\n');
    }
    if verdict.is_wontfix_enhancement() {
        let path = verdict
            .out_of_scope_filename
            .as_deref()
            .unwrap_or("<missing>");
        let content = verdict
            .out_of_scope_content
            .as_deref()
            .unwrap_or("<missing>");
        out.push_str(&format!(
            "\n-- would write file --\n.out-of-scope/{path}\n\n{content}\n"
        ));
    }
    if verdict.state == VerdictState::Wontfix {
        out.push_str("\n-- would close issue --\n");
    }
    out
}

/// The four canonical triage roles in docs/agents/triage-labels.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    ReadyForAgent,
    NeedsInfo,
    ReadyForHuman,
    Wontfix,
}

impl Verdict {
    /// The canonical label string for this verdict. Used by the apply
    /// step (to set the GitHub label) and by the summary printer.
    pub fn label(self) -> &'static str {
        match self {
            Verdict::ReadyForAgent => "ready-for-agent",
            Verdict::NeedsInfo => "needs-info",
            Verdict::ReadyForHuman => "ready-for-human",
            Verdict::Wontfix => "wontfix",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Per-verdict tally of how the backlog drain went, plus a separate
/// `failed` bucket for issues whose per-issue triage call returned
/// `Err(_)` (crash, malformed verdict, apply error). Failures stay in
/// their own bucket — they MUST NOT silently roll into a verdict
/// count, because the operator scanning the summary needs to know
/// whether any per-issue call crashed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BacklogSummary {
    pub ready_for_agent: u32,
    pub needs_info: u32,
    pub wontfix: u32,
    pub ready_for_human: u32,
    pub failed: u32,
}

impl BacklogSummary {
    pub fn total(&self) -> u32 {
        self.ready_for_agent
            + self.needs_info
            + self.wontfix
            + self.ready_for_human
            + self.failed
    }

    pub fn record_verdict(&mut self, v: Verdict) {
        match v {
            Verdict::ReadyForAgent => self.ready_for_agent += 1,
            Verdict::NeedsInfo => self.needs_info += 1,
            Verdict::ReadyForHuman => self.ready_for_human += 1,
            Verdict::Wontfix => self.wontfix += 1,
        }
    }

    pub fn record_failure(&mut self) {
        self.failed += 1;
    }
}

impl fmt::Display for BacklogSummary {
    /// End-of-run report the operator sees on stdout. Format is one
    /// header line (total processed) plus one indented line per
    /// verdict bucket. The `failed` line is included only when any
    /// failures occurred — a clean run keeps the report uncluttered.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "bellows triage: processed {} issue(s)", self.total())?;
        writeln!(f, "  {} -> ready-for-agent", self.ready_for_agent)?;
        writeln!(f, "  {} -> needs-info", self.needs_info)?;
        writeln!(f, "  {} -> wontfix", self.wontfix)?;
        writeln!(f, "  {} -> ready-for-human", self.ready_for_human)?;
        if self.failed > 0 {
            writeln!(f, "  {} failed", self.failed)?;
        }
        Ok(())
    }
}

/// Closed category vocabulary for triage verdicts. Distinguishes a
/// `bug` (something is broken and the issue describes the breakage)
/// from an `enhancement` (something is missing or could be better).
/// The category gates `wontfix` behaviour: wontfix + enhancement
/// writes `.out-of-scope/<filename>.md` directly to master so future
/// triage runs see the precedent; wontfix + bug just closes the
/// issue.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VerdictCategory {
    Bug,
    Enhancement,
}

/// Closed triage-state vocabulary. Mirrors docs/agents/triage-labels.md
/// minus the `needs-triage` entry (which is the input state, not an
/// output state). The `label()` method renders the canonical label
/// string the apply step uses when transitioning labels on the
/// GitHub issue.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VerdictState {
    NeedsInfo,
    ReadyForAgent,
    ReadyForHuman,
    Wontfix,
}

impl VerdictState {
    pub fn label(self) -> &'static str {
        match self {
            VerdictState::NeedsInfo => "needs-info",
            VerdictState::ReadyForAgent => "ready-for-agent",
            VerdictState::ReadyForHuman => "ready-for-human",
            VerdictState::Wontfix => "wontfix",
        }
    }
}

impl fmt::Display for VerdictState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// The full triage verdict the in-container claude agent writes to
/// `/workspace/.bellows-triage-verdict.json`. Bellows reads this file
/// after the container exits, validates conditional fields per state,
/// then applies via the tracker (label swap, comments, optionally
/// closing the issue and committing an `.out-of-scope/` precedent).
///
/// The optional fields are conditionally required per state — the
/// [`TriageVerdict::parse`] helper runs the per-state validation rules
/// AFTER deserialisation. A missing required field surfaces as
/// [`VerdictParseError::MissingField`]; an unexpected field for the
/// state (e.g. `out_of_scope_filename` on a `needs-info` verdict)
/// surfaces as [`VerdictParseError::UnexpectedField`].
///
/// Rules:
///   - `state == ready-for-agent` requires `agent_brief` to be
///     non-empty, and forbids `human_brief`, `out_of_scope_*`,
///     `close_issue=true`.
///   - `state == ready-for-human` requires `human_brief` to be
///     non-empty, and forbids the symmetric set on the agent side.
///   - `state == needs-info` forbids all conditional fields.
///   - `state == wontfix` requires `close_issue == true`. Additionally
///     `state == wontfix AND category == enhancement` requires both
///     `out_of_scope_filename` and `out_of_scope_content` to be
///     non-empty (this is the wontfix-enhancement precedent path).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TriageVerdict {
    pub category: VerdictCategory,
    pub state: VerdictState,
    /// Plain-prose explanation of why the agent reached this verdict.
    /// Surfaced in dry-run output and (operator-discretionary) may be
    /// quoted into the closing comment.
    pub reasoning: String,
    /// The main verdict comment body posted on the issue. Bellows
    /// prefixes the canonical AI-disclaimer line before posting so a
    /// human reading the issue knows the verdict came from triage.
    pub comment_body: String,
    /// The `## Agent Brief` payload (only for state=ready-for-agent).
    /// Posted as a separate comment so the downstream
    /// `tracker::fetch_agent_brief` (which scans for the literal
    /// `## Agent Brief` header) picks it up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_brief: Option<String>,
    /// The `## Human Brief` payload (only for state=ready-for-human).
    /// Posted as a separate comment for the human implementer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_brief: Option<String>,
    /// Filename under `.out-of-scope/` to write when applying a
    /// `wontfix-enhancement` verdict (only for state=wontfix AND
    /// category=enhancement).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_of_scope_filename: Option<String>,
    /// File content for the `wontfix-enhancement` precedent file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_of_scope_content: Option<String>,
    /// Required to be `Some(true)` when `state == wontfix`; required
    /// to be `None`/`Some(false)` otherwise. Explicit so the agent
    /// and bellows agree that the issue is being closed (rather than
    /// implicit on `state == wontfix`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_issue: Option<bool>,
}

#[derive(Debug, thiserror::Error)]
pub enum VerdictParseError {
    #[error("verdict JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("verdict for state={state} is missing required field `{field}`")]
    MissingField {
        field: &'static str,
        state: VerdictState,
    },
    #[error("verdict for state={state} carries unexpected field `{field}` (only valid for a different state)")]
    UnexpectedField {
        field: &'static str,
        state: VerdictState,
    },
}

impl TriageVerdict {
    /// Parse + validate a verdict JSON blob. Deserialisation errors
    /// (malformed JSON, unknown state/category strings, unknown
    /// fields) surface as [`VerdictParseError::Json`]; conditional
    /// per-state validation errors surface as
    /// [`VerdictParseError::MissingField`] or
    /// [`VerdictParseError::UnexpectedField`]. The apply step
    /// short-circuits on any parse error so the issue is left
    /// untouched (no partial label-swap with a malformed brief).
    pub fn parse(s: &str) -> Result<TriageVerdict, VerdictParseError> {
        let raw: TriageVerdict = serde_json::from_str(s)?;
        raw.validate()?;
        Ok(raw)
    }

    fn validate(&self) -> Result<(), VerdictParseError> {
        let state = self.state;
        let non_empty = |opt: &Option<String>| opt.as_ref().is_some_and(|s| !s.trim().is_empty());
        let present = |opt: &Option<String>| opt.is_some();

        match state {
            VerdictState::ReadyForAgent => {
                if !non_empty(&self.agent_brief) {
                    return Err(VerdictParseError::MissingField {
                        field: "agent_brief",
                        state,
                    });
                }
                if present(&self.human_brief) {
                    return Err(VerdictParseError::UnexpectedField {
                        field: "human_brief",
                        state,
                    });
                }
                if present(&self.out_of_scope_filename) {
                    return Err(VerdictParseError::UnexpectedField {
                        field: "out_of_scope_filename",
                        state,
                    });
                }
                if present(&self.out_of_scope_content) {
                    return Err(VerdictParseError::UnexpectedField {
                        field: "out_of_scope_content",
                        state,
                    });
                }
                if matches!(self.close_issue, Some(true)) {
                    return Err(VerdictParseError::UnexpectedField {
                        field: "close_issue",
                        state,
                    });
                }
            }
            VerdictState::ReadyForHuman => {
                if !non_empty(&self.human_brief) {
                    return Err(VerdictParseError::MissingField {
                        field: "human_brief",
                        state,
                    });
                }
                if present(&self.agent_brief) {
                    return Err(VerdictParseError::UnexpectedField {
                        field: "agent_brief",
                        state,
                    });
                }
                if present(&self.out_of_scope_filename) {
                    return Err(VerdictParseError::UnexpectedField {
                        field: "out_of_scope_filename",
                        state,
                    });
                }
                if present(&self.out_of_scope_content) {
                    return Err(VerdictParseError::UnexpectedField {
                        field: "out_of_scope_content",
                        state,
                    });
                }
                if matches!(self.close_issue, Some(true)) {
                    return Err(VerdictParseError::UnexpectedField {
                        field: "close_issue",
                        state,
                    });
                }
            }
            VerdictState::NeedsInfo => {
                if present(&self.agent_brief) {
                    return Err(VerdictParseError::UnexpectedField {
                        field: "agent_brief",
                        state,
                    });
                }
                if present(&self.human_brief) {
                    return Err(VerdictParseError::UnexpectedField {
                        field: "human_brief",
                        state,
                    });
                }
                if present(&self.out_of_scope_filename) {
                    return Err(VerdictParseError::UnexpectedField {
                        field: "out_of_scope_filename",
                        state,
                    });
                }
                if present(&self.out_of_scope_content) {
                    return Err(VerdictParseError::UnexpectedField {
                        field: "out_of_scope_content",
                        state,
                    });
                }
                if matches!(self.close_issue, Some(true)) {
                    return Err(VerdictParseError::UnexpectedField {
                        field: "close_issue",
                        state,
                    });
                }
            }
            VerdictState::Wontfix => {
                if !matches!(self.close_issue, Some(true)) {
                    return Err(VerdictParseError::MissingField {
                        field: "close_issue",
                        state,
                    });
                }
                if present(&self.agent_brief) {
                    return Err(VerdictParseError::UnexpectedField {
                        field: "agent_brief",
                        state,
                    });
                }
                if present(&self.human_brief) {
                    return Err(VerdictParseError::UnexpectedField {
                        field: "human_brief",
                        state,
                    });
                }
                match self.category {
                    VerdictCategory::Enhancement => {
                        if !non_empty(&self.out_of_scope_filename) {
                            return Err(VerdictParseError::MissingField {
                                field: "out_of_scope_filename",
                                state,
                            });
                        }
                        if !non_empty(&self.out_of_scope_content) {
                            return Err(VerdictParseError::MissingField {
                                field: "out_of_scope_content",
                                state,
                            });
                        }
                    }
                    VerdictCategory::Bug => {
                        if present(&self.out_of_scope_filename) {
                            return Err(VerdictParseError::UnexpectedField {
                                field: "out_of_scope_filename",
                                state,
                            });
                        }
                        if present(&self.out_of_scope_content) {
                            return Err(VerdictParseError::UnexpectedField {
                                field: "out_of_scope_content",
                                state,
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether the wontfix-enhancement workspace-side path (write
    /// `.out-of-scope/<filename>.md` directly to master) applies to
    /// this verdict. Equivalent to `state == wontfix AND category ==
    /// enhancement`, but the named predicate keeps the runner
    /// readable.
    pub fn is_wontfix_enhancement(&self) -> bool {
        self.state == VerdictState::Wontfix && self.category == VerdictCategory::Enhancement
    }
}

/// Walk `issues` serially, calling `triage_one` against each one.
/// Returns the aggregated tally.
///
/// `triage_one` is an `FnMut(issue_number, dry_run) -> Future<Result<Verdict, String>>`
/// — async so it can talk to Docker / GitHub, and returning a
/// `Result` so per-issue failures can be caught and tallied. A
/// returned `Err(_)` does NOT abort the drain: the failure is
/// recorded in `summary.failed` and the loop proceeds to the next
/// issue. (Different from slice X1's halt-on-phase-failure: there,
/// halting protects one PR; here, halting would block the whole
/// backlog drain.)
///
/// The serial `for` loop is the workspace-state-flows-between-issues
/// contract. Do not change to `join_all` or similar without first
/// understanding the contract in the module docstring.
pub async fn drain_backlog<F, Fut>(
    issues: Vec<Issue>,
    dry_run: bool,
    mut triage_one: F,
) -> BacklogSummary
where
    F: FnMut(u64, bool) -> Fut,
    Fut: std::future::Future<Output = Result<Verdict, String>>,
{
    let mut summary = BacklogSummary::default();
    for issue in issues {
        match triage_one(issue.number, dry_run).await {
            Ok(v) => summary.record_verdict(v),
            Err(_) => summary.record_failure(),
        }
    }
    summary
}
