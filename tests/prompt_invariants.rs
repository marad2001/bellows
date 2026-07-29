//! Structural contracts for versioned agent prompts.
//!
//! These assertions deliberately pin only parser-facing vocabularies and
//! instructions recorded as load-bearing in the cited ADRs. They avoid full
//! sentences so prose can be rephrased without weakening the contract.

use bellows::policy::{
    Severity, BATCH_REVIEW_FIX_NIT_PROMPT, REVIEW_FINDINGS_FILE, REVIEW_FIX_PROMPT,
    REVIEW_PROMPT, SECURITY_FINDINGS_FILE, SECURITY_REVIEW_PROMPT,
};
use bellows::triage::{VerdictState, TRIAGE_PROMPT, TRIAGE_VERDICT_FILE};

fn assert_prompt_names_every_severity(prompt_name: &str, prompt: &str) {
    // parse_findings's closed vocabulary: use the parser's own rendering
    // rather than duplicating the three accepted strings in this test.
    for severity in [Severity::Blocker, Severity::Important, Severity::Nit] {
        let tag = severity.as_tag();
        assert!(
            prompt.contains(tag),
            "{prompt_name} must name parser severity `{tag}`: {prompt}",
        );
    }
}

#[test]
fn review_prompt_uses_the_findings_parser_severity_vocabulary() {
    // Parser contract: parse_findings rejects titles whose tag is not one of
    // the Severity variants.
    assert_prompt_names_every_severity("REVIEW_PROMPT", REVIEW_PROMPT);
}

#[test]
fn review_prompt_writes_findings_to_the_runner_handoff_file() {
    // ADR-0009: review findings are an upstream handoff, not an input for the
    // merger to reinterpret.
    assert!(
        REVIEW_PROMPT.contains(REVIEW_FINDINGS_FILE),
        "REVIEW_PROMPT must direct findings to `{REVIEW_FINDINGS_FILE}`: {REVIEW_PROMPT}",
    );
}

#[test]
fn review_prompt_preserves_the_address_or_explain_contract() {
    // ADR-0006 and ADR-0009: important findings must be fixed or explicitly
    // escalated; silently dropping one triggers the parser-as-backstop.
    assert!(
        REVIEW_PROMPT.to_lowercase().contains("fixed or escalated"),
        "REVIEW_PROMPT must say important findings are fixed or escalated: {REVIEW_PROMPT}",
    );
}

#[test]
fn review_prompt_leaves_code_changes_to_the_review_fix_phase() {
    // ADR-0011: review is an analysis phase; review-fix is the phase that
    // changes the implementation.
    let lower = REVIEW_PROMPT.to_lowercase();
    assert!(
        lower.contains("read-only")
            && lower.contains("do not edit")
            && lower.contains("review-fix phase"),
        "REVIEW_PROMPT must write findings rather than act on them: {REVIEW_PROMPT}",
    );
}

#[test]
fn security_review_prompt_uses_the_findings_parser_severity_vocabulary() {
    // Parser contract: security findings use the same parse_findings grammar
    // and Severity variants as standard review findings.
    assert_prompt_names_every_severity("SECURITY_REVIEW_PROMPT", SECURITY_REVIEW_PROMPT);
}

#[test]
fn security_review_prompt_writes_findings_to_the_runner_handoff_file() {
    // ADR-0009: findings files belong to their producer/fix handoff and are
    // not later reconstructed from review prose.
    assert!(
        SECURITY_REVIEW_PROMPT.contains(SECURITY_FINDINGS_FILE),
        "SECURITY_REVIEW_PROMPT must direct findings to `{SECURITY_FINDINGS_FILE}`: \
         {SECURITY_REVIEW_PROMPT}",
    );
}

#[test]
fn security_review_prompt_is_read_only_over_the_diff() {
    // ADR-0011: security-review is an analysis phase in the pipeline; fixes
    // are performed by the following security-fix phase.
    let lower = SECURITY_REVIEW_PROMPT.to_lowercase();
    assert!(
        lower.contains("read-only")
            && lower.contains("do not edit")
            && lower.contains("security-fix phase"),
        "SECURITY_REVIEW_PROMPT must remain read-only: {SECURITY_REVIEW_PROMPT}",
    );
}

#[test]
fn review_fix_prompt_handles_exactly_one_finding() {
    // ADR-0009's coverage backstop depends on the per-finding invocation
    // having no list from which an agent can silently skip an item.
    let lower = REVIEW_FIX_PROMPT.to_lowercase();
    assert!(
        lower.contains("one finding")
            && lower.contains("exactly one finding")
            && lower.contains("do not broaden scope"),
        "REVIEW_FIX_PROMPT must scope each invocation to one finding: {REVIEW_FIX_PROMPT}",
    );
}

#[test]
fn review_fix_prompt_requires_address_or_explain() {
    // ADR-0006 and ADR-0009: every non-nit finding must become either a code
    // fix or a structured escalation visible to the parser-as-backstop.
    assert!(
        REVIEW_FIX_PROMPT.contains("Address the finding in code")
            && REVIEW_FIX_PROMPT.contains("## Unaddressed finding: {title}")
            && REVIEW_FIX_PROMPT.contains("MUST do exactly one"),
        "REVIEW_FIX_PROMPT must retain both address-or-explain outcomes: \
         {REVIEW_FIX_PROMPT}",
    );
}

#[test]
fn review_fix_prompt_requires_the_verbatim_title_in_escalations() {
    // ADR-0009 parser contract: coverage matching is character-for-character
    // between ParsedFinding.title and `## Unaddressed finding: <title>`.
    let lower = REVIEW_FIX_PROMPT.to_lowercase();
    assert!(
        lower.contains("exact verbatim title") && lower.contains("character-for-character"),
        "REVIEW_FIX_PROMPT must preserve verbatim finding-title matching: \
         {REVIEW_FIX_PROMPT}",
    );
}

#[test]
fn batch_nit_prompt_is_scoped_to_the_parser_nit_severity() {
    // Severity/parser contract: only Severity::Nit takes the permissive batch
    // path; blocker and important findings use per-finding enforcement.
    let nit = Severity::Nit.as_tag();
    assert!(
        BATCH_REVIEW_FIX_NIT_PROMPT.contains(&format!("`{nit}`-severity"))
            && [Severity::Blocker, Severity::Important]
                .into_iter()
                .all(|severity| !BATCH_REVIEW_FIX_NIT_PROMPT.contains(severity.as_tag())),
        "BATCH_REVIEW_FIX_NIT_PROMPT must be exclusive to `{nit}` findings: \
         {BATCH_REVIEW_FIX_NIT_PROMPT}",
    );
}

#[test]
fn batch_nit_prompt_permits_skipping_findings() {
    // ADR-0009: nits are operator-discretionary and deliberately do not
    // participate in the blocker/important coverage backstop.
    let lower = BATCH_REVIEW_FIX_NIT_PROMPT.to_lowercase();
    assert!(
        lower.contains("may skip") && lower.contains("silent skip is allowed"),
        "BATCH_REVIEW_FIX_NIT_PROMPT must explicitly permit skipping nits: \
         {BATCH_REVIEW_FIX_NIT_PROMPT}",
    );
}

#[test]
fn batch_nit_prompt_does_not_escalate_skipped_nits() {
    // ADR-0006: `## Unaddressed finding:` is structured failure, which is too
    // strong for an operator-discretionary nit.
    let lower = BATCH_REVIEW_FIX_NIT_PROMPT.to_lowercase();
    assert!(
        lower.contains("do not append to bellows-agent-notes.md")
            && lower.contains("agent-self-reported-failure"),
        "BATCH_REVIEW_FIX_NIT_PROMPT must not escalate skipped nits: \
         {BATCH_REVIEW_FIX_NIT_PROMPT}",
    );
}

#[test]
fn triage_prompt_names_every_parser_schema_field() {
    // TriageVerdict serde/parser contract: unknown, missing, and
    // state-inapplicable fields are rejected by the host-side parser.
    for field in [
        "category",
        "state",
        "reasoning",
        "comment_body",
        "agent_brief",
        "human_brief",
        "out_of_scope_filename",
        "out_of_scope_content",
        "close_issue",
    ] {
        assert!(
            TRIAGE_PROMPT.contains(&format!("\"{field}\"")),
            "TRIAGE_PROMPT must name verdict field `{field}`: {TRIAGE_PROMPT}",
        );
    }
}

#[test]
fn triage_prompt_uses_the_parser_state_vocabulary() {
    // VerdictState serde/parser contract: derive the four accepted state
    // strings from the enum used by host-side validation.
    for state in [
        VerdictState::NeedsInfo,
        VerdictState::ReadyForAgent,
        VerdictState::ReadyForHuman,
        VerdictState::Wontfix,
    ] {
        let label = state.label();
        assert!(
            TRIAGE_PROMPT.contains(label),
            "TRIAGE_PROMPT must name parser state `{label}`: {TRIAGE_PROMPT}",
        );
    }
}

#[test]
fn triage_prompt_overrides_gh_and_interactive_skill_assumptions() {
    // ADR-0005: the triage shim carries the load-bearing headless/no-user
    // constraint because the canonical skill assumes gh and a human.
    let lower = TRIAGE_PROMPT.to_lowercase();
    assert!(
        lower.contains("no `gh` cli")
            && lower.contains("no human will respond")
            && lower.contains("instead of waiting"),
        "TRIAGE_PROMPT must override gh and interactive skill behavior: {TRIAGE_PROMPT}",
    );
}

#[test]
fn triage_prompt_defaults_uncertainty_to_needs_info() {
    // ADR-0012: vague issues belong in triage and must route to needs-info
    // before an implementation brief is written.
    let needs_info = VerdictState::NeedsInfo.label();
    assert!(
        TRIAGE_PROMPT.contains(&format!("default to `{needs_info}`")),
        "TRIAGE_PROMPT must default uncertainty to `{needs_info}`: {TRIAGE_PROMPT}",
    );
}

#[test]
fn triage_prompt_writes_the_verdict_to_the_host_parser_input() {
    // Triage parser contract: the headless agent communicates solely through
    // the verdict JSON path consumed by bellows on the host.
    assert!(
        TRIAGE_PROMPT.contains(TRIAGE_VERDICT_FILE),
        "TRIAGE_PROMPT must write `{TRIAGE_VERDICT_FILE}`: {TRIAGE_PROMPT}",
    );
}
