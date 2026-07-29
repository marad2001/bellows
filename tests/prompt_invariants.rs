//! Structural contracts for versioned agent prompts.
//!
//! These assertions deliberately pin only parser-facing vocabularies and
//! instructions recorded as load-bearing in the cited ADRs. They avoid full
//! sentences so prose can be rephrased without weakening the contract.
//!
//! Each contract is pinned in exactly ONE place. Contracts already pinned by
//! `tests/policy.rs` (the severity vocabulary, the security-review handoff
//! paths and read-only rule, the per-finding scope / address-or-explain /
//! verbatim-title rules, the nit skip-and-do-not-escalate rules) and by
//! `tests/triage.rs` (the verdict JSON schema fields, the four verdict
//! states, the headless `gh`/no-human override, the verdict file path) are
//! NOT restated here. Add an assertion to this file only when no existing
//! test covers it; otherwise a single prompt reword fails twice with two
//! differently-worded messages and a maintainer editing one copy gets no
//! signal that the other exists.

use bellows::policy::{Severity, BATCH_REVIEW_FIX_NIT_PROMPT, REVIEW_FINDINGS_FILE, REVIEW_PROMPT};
use bellows::triage::{VerdictState, TRIAGE_PROMPT};

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
fn batch_nit_prompt_is_scoped_to_the_parser_nit_severity() {
    // Severity/parser contract: only Severity::Nit takes the permissive batch
    // path; blocker and important findings use per-finding enforcement. The
    // exclusion is the load-bearing half — `tests/policy.rs` pins what the nit
    // prompt permits, but nothing else pins that the stricter severities never
    // reach it.
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
fn triage_prompt_defaults_uncertainty_to_needs_info() {
    // ADR-0012: vague issues belong in triage and must route to needs-info
    // before an implementation brief is written.
    let needs_info = VerdictState::NeedsInfo.label();
    assert!(
        TRIAGE_PROMPT.contains(&format!("default to `{needs_info}`")),
        "TRIAGE_PROMPT must default uncertainty to `{needs_info}`: {TRIAGE_PROMPT}",
    );
}
