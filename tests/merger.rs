//! Phase 8 merger acceptance criteria (issue #123 / ADR-0009 slice 1).
//!
//! These tests pin the brief's verdict-parser ACs to a checkable surface
//! before the source-side change lands, so the TDD shape is visible in
//! the commit log.

use bellows::policy::{parse_merger_verdict, MergerVerdict};

// -----------------------------------------------------------------
// AC: parse_merger_verdict for the three valid tokens, garbage,
// missing-line, ambiguous, and whitespace / CRLF edge cases.
// -----------------------------------------------------------------

#[test]
fn parse_merger_verdict_recognises_merge_token() {
    let agent_output = "Prose review goes here.\nMore prose.\n\nVERDICT: MERGE\n";
    assert_eq!(parse_merger_verdict(agent_output), Some(MergerVerdict::Merge));
}

#[test]
fn parse_merger_verdict_recognises_hold_noted_token() {
    let agent_output = "Diff broadly matches ACs but agent-notes flag a gap.\n\nVERDICT: HOLD-NOTED\n";
    assert_eq!(
        parse_merger_verdict(agent_output),
        Some(MergerVerdict::HoldNoted),
    );
}

#[test]
fn parse_merger_verdict_recognises_hold_draft_token() {
    let agent_output = "Diff fails AC2 — not enough test coverage.\n\nVERDICT: HOLD-DRAFT\n";
    assert_eq!(
        parse_merger_verdict(agent_output),
        Some(MergerVerdict::HoldDraft),
    );
}

#[test]
fn parse_merger_verdict_returns_none_when_verdict_line_missing() {
    let agent_output = "Prose review with no trailing verdict line at all.\n";
    assert_eq!(parse_merger_verdict(agent_output), None);
}

#[test]
fn parse_merger_verdict_returns_none_for_garbage_token() {
    // Off-vocabulary tokens (e.g. `LGTM`, `OK`, `merge`, lowercase
    // variants) must NOT be coerced into a valid verdict.
    let agent_output = "Prose.\n\nVERDICT: LGTM\n";
    assert_eq!(parse_merger_verdict(agent_output), None);
    let agent_output_lc = "Prose.\n\nVERDICT: merge\n";
    assert_eq!(parse_merger_verdict(agent_output_lc), None);
}

#[test]
fn parse_merger_verdict_returns_none_when_ambiguous() {
    // Two verdict lines with DIFFERENT tokens — refuse to pick one
    // arbitrarily. Logged as None, classifier behaviour unchanged.
    let agent_output =
        "First pass said MERGE.\n\nVERDICT: MERGE\n\nOn reflection:\n\nVERDICT: HOLD-DRAFT\n";
    assert_eq!(parse_merger_verdict(agent_output), None);
}

#[test]
fn parse_merger_verdict_accepts_duplicate_verdict_lines_with_same_token() {
    // Two verdict lines with the SAME token is not ambiguous — the
    // agent's verdict is still unambiguous, and a strict "any
    // duplicate" check would over-reject (an agent that quoted
    // itself in the prose would trip it). Brief says "ambiguous"
    // explicitly, which the doc-comment clarifies to mean
    // "different tokens".
    let agent_output = "I'll say it twice.\n\nVERDICT: MERGE\n\nAgain:\n\nVERDICT: MERGE\n";
    assert_eq!(parse_merger_verdict(agent_output), Some(MergerVerdict::Merge));
}

#[test]
fn parse_merger_verdict_tolerates_trailing_whitespace_on_verdict_line() {
    let agent_output = "Prose.\n\nVERDICT: MERGE   \n";
    assert_eq!(parse_merger_verdict(agent_output), Some(MergerVerdict::Merge));
    let with_tabs = "Prose.\n\nVERDICT: HOLD-NOTED\t\t\n";
    assert_eq!(parse_merger_verdict(with_tabs), Some(MergerVerdict::HoldNoted));
}

#[test]
fn parse_merger_verdict_tolerates_crlf_line_endings() {
    let agent_output = "Prose review.\r\n\r\nVERDICT: HOLD-DRAFT\r\n";
    assert_eq!(
        parse_merger_verdict(agent_output),
        Some(MergerVerdict::HoldDraft),
    );
}

#[test]
fn parse_merger_verdict_returns_none_for_empty_input() {
    assert_eq!(parse_merger_verdict(""), None);
}

#[test]
fn parse_merger_verdict_returns_none_when_verdict_keyword_appears_inside_prose() {
    // A bare `VERDICT:` line that doesn't actually carry one of the
    // three canonical tokens is off-vocabulary and must not match.
    let agent_output = "I considered the VERDICT: pending more thought.\n";
    assert_eq!(parse_merger_verdict(agent_output), None);
}
