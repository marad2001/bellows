//! Phase 8 merger acceptance criteria (issue #123 / ADR-0009 slice 1).
//!
//! These tests pin the brief's verdict-parser ACs to a checkable surface
//! before the source-side change lands, so the TDD shape is visible in
//! the commit log.

use bellows::policy::{parse_merger_verdict, render_merger_prompt, MergerVerdict};

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

// -----------------------------------------------------------------
// AC: render_merger_prompt produces a prompt with the diff, brief
// verbatim ACs, agent-notes with synth-provenance markers, and CI
// status as input; anchors on diff and ACs; emits prose ending with
// the verdict line.
// -----------------------------------------------------------------

#[test]
fn render_merger_prompt_names_each_input_source() {
    // The merger reads four inputs: diff vs master, the brief's
    // verbatim ACs, the final agent-notes.md (with synth-provenance
    // markers), and CI / cargo-checks status. The prompt must
    // explicitly name each so the agent reads from the right place.
    let prompt = render_merger_prompt();
    assert!(
        prompt.contains(".bellows-review-diff.patch")
            || prompt.contains("diff vs master")
            || prompt.contains("diff"),
        "merger prompt must reference the diff input: {prompt}",
    );
    assert!(
        prompt.contains("acceptance criteria") || prompt.contains("Acceptance criteria"),
        "merger prompt must reference the brief's ACs: {prompt}",
    );
    assert!(
        prompt.contains("agent-notes.md"),
        "merger prompt must reference agent-notes.md: {prompt}",
    );
    assert!(
        prompt.contains("CI") || prompt.contains("cargo-checks") || prompt.contains("cargo checks"),
        "merger prompt must reference CI / cargo-checks status: {prompt}",
    );
}

#[test]
fn render_merger_prompt_anchors_judgement_on_diff_and_acs_not_notes() {
    // Brief: "Anchors judgement on the diff and ACs; treats notes as
    // agent-stated reasoning, not evidence the code is correct." Pin
    // the load-bearing phrasing so a future edit can't accidentally
    // flip the framing.
    let prompt = render_merger_prompt();
    assert!(
        prompt.to_lowercase().contains("not evidence")
            || prompt.to_lowercase().contains("agent-stated reasoning")
            || prompt.to_lowercase().contains("reasoning, not evidence"),
        "merger prompt must frame notes as reasoning, not evidence: {prompt}",
    );
}

#[test]
fn render_merger_prompt_demands_trailing_verdict_line_with_closed_vocabulary() {
    // The prompt must instruct the agent to end with a verdict line
    // carrying exactly one of MERGE / HOLD-NOTED / HOLD-DRAFT. This
    // is what parse_merger_verdict keys on; the prompt is the
    // contract between the agent and the parser.
    let prompt = render_merger_prompt();
    assert!(
        prompt.contains("VERDICT:"),
        "merger prompt must instruct the agent to emit a VERDICT: line: {prompt}",
    );
    for token in ["MERGE", "HOLD-NOTED", "HOLD-DRAFT"] {
        assert!(
            prompt.contains(token),
            "merger prompt must name the closed-vocabulary token `{token}`: {prompt}",
        );
    }
}

#[test]
fn render_merger_prompt_identifies_phase_as_merger() {
    // Sibling-of-review/security shape: the prompt opens by telling
    // the agent which phase of the pipeline it is running as. Pin
    // it loosely (allow prose to flex) by matching "merger" in
    // the first 200 characters.
    let prompt = render_merger_prompt();
    let head: String = prompt.chars().take(200).collect();
    assert!(
        head.to_lowercase().contains("merger"),
        "merger prompt must identify the phase: {head}",
    );
}
