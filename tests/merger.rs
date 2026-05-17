//! Phase 8 merger acceptance criteria (issue #123 / ADR-0009 slice 1).
//!
//! These tests pin the brief's verdict-parser ACs to a checkable surface
//! before the source-side change lands, so the TDD shape is visible in
//! the commit log.

use std::str::FromStr;

use bellows::config::{Config, Engine};
use bellows::policy::{
    parse_merger_verdict, render_merger_prompt, MergerVerdict, PhaseOutcomes,
};

// -----------------------------------------------------------------
// AC: parse_merger_verdict for the three valid tokens, garbage,
// missing-line, non-trailing / duplicate verdict lines, and whitespace /
// CRLF edge cases.
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
fn parse_merger_verdict_returns_none_when_verdict_line_is_not_trailing() {
    let agent_output =
        "Initial judgement.\n\nVERDICT: MERGE\n\nLater prose means this was not the final line.\n";
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
fn parse_merger_verdict_returns_none_for_duplicate_verdict_lines_with_same_token() {
    // The merger prompt requires a single trailing verdict line.
    // Earlier standalone verdict lines are stale / off-contract even
    // when they carry the same token as the final line.
    let agent_output = "I'll say it twice.\n\nVERDICT: MERGE\n\nAgain:\n\nVERDICT: MERGE\n";
    assert_eq!(parse_merger_verdict(agent_output), None);
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

#[test]
fn render_merger_prompt_instructs_not_to_vote_merge_when_synth_provenance_markers_present() {
    // AC8 of issue #124 / ADR-0009 slice 2: the merger reads
    // agent-notes.md but the in-band content may contain
    // Bellows-authored synth-provenance markers
    // (`<!-- bellows parser-as-backstop ... -->`,
    //  `<!-- bellows weak-test guard ... -->`,
    //  `<!-- bellows implement-crash recovery ... -->`).
    //
    // The policy-side (β) hard override in classify_exit blocks a
    // Merge verdict over these markers, but the merger should
    // recognise them and not vote MERGE in the first place — the
    // override is a defence-in-depth, not the primary instruction.
    // Pin the prompt's instruction so a future edit can't quietly
    // drop the synth-provenance guidance.
    let prompt = render_merger_prompt();
    let lowered = prompt.to_lowercase();
    assert!(
        lowered.contains("synth")
            || lowered.contains("bellows-authored")
            || lowered.contains("<!-- bellows"),
        "merger prompt must mention bellows-authored synth-provenance markers \
         so the merger knows to look for them: {prompt}",
    );
    // The instruction must specifically tell the agent NOT to vote
    // MERGE in their presence — `HOLD-DRAFT` (or any non-MERGE
    // verdict) is the correct shape when the synth markers indicate
    // Bellows itself decided the run was not mergeable.
    assert!(
        lowered.contains("do not vote merge")
            || lowered.contains("do not vote `merge`")
            || lowered.contains("must not vote merge")
            || lowered.contains("never vote merge")
            || lowered.contains("never vote `merge`")
            || lowered.contains("not vote `merge`"),
        "merger prompt must instruct the agent NOT to vote MERGE when \
         synth-provenance markers are present in agent-notes.md: {prompt}",
    );
}

// -----------------------------------------------------------------
// AC: `[phases.merge]` schema entry parses with a `cli_chain` field;
// default `["claude:claude-opus-4-7"]` when omitted.
// -----------------------------------------------------------------

#[test]
fn config_phases_merge_defaults_to_claude_opus_when_section_omitted() {
    // Brief: 'Default `claude:claude-opus-4-7` per ADR-0009 (opus is
    // the first-look-judgement role; cross-family independence from
    // codex which originates most agent-authored unaddressed-finding
    // headings in phase 4).' A minimal config (with no `[phases.merge]`
    // table) must produce a single-entry chain with that engine + model.
    let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"
"#;
    let config = Config::from_str(config_text).expect("minimal config must parse");
    let merge = &config.phases.merge.cli_chain;
    assert_eq!(merge.len(), 1, "merger default chain must be one entry");
    assert_eq!(merge[0].engine, Engine::Claude);
    assert_eq!(
        merge[0].model.as_deref(),
        Some("claude-opus-4-7"),
        "merger default must pin claude-opus-4-7 per ADR-0009"
    );
}

#[test]
fn config_phases_merge_accepts_operator_supplied_cli_chain() {
    // Brief: 'Engine selection is per-phase configurable via
    // `[phases.merge] cli_chain = [...]` in `orchestrator.toml`,
    // matching the existing per-phase pattern.' Operators can swap
    // the engine but cannot disable the phase via config — the
    // empty-chain rejection lives in the per-phase normaliser and is
    // already shared with the other phases.
    let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"

[phases.merge]
cli_chain = ["codex:gpt-5.5", "claude:claude-opus-4-7"]
"#;
    let config = Config::from_str(config_text).expect("merge override config must parse");
    let merge = &config.phases.merge.cli_chain;
    assert_eq!(merge.len(), 2);
    assert_eq!(merge[0].engine, Engine::Codex);
    assert_eq!(merge[0].model.as_deref(), Some("gpt-5.5"));
    assert_eq!(merge[1].engine, Engine::Claude);
    assert_eq!(merge[1].model.as_deref(), Some("claude-opus-4-7"));
}

// -----------------------------------------------------------------
// AC: Runner phase-8 dispatch reads the configured engine from
// phases.merge.cli_chain (mirroring the existing phase-dispatch
// test shape — config-driven, not container-side).
// -----------------------------------------------------------------

#[test]
fn config_phases_merge_first_entry_drives_phase_8_dispatch() {
    // Mirrors `config_phases_implement_first_entry_drives_default_engine_for_setup_auth`
    // in tests/engine_dispatch.rs. The first chain entry of
    // phases.merge is what the phase-8 runner will dispatch with.
    let minimal = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"
"#;
    let config = Config::from_str(minimal).unwrap();
    let entry = config.phases.merge.first_entry();
    assert_eq!(entry.engine, Engine::Claude);
    assert_eq!(entry.model.as_deref(), Some("claude-opus-4-7"));

    let codex_first = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"

[phases.merge]
cli_chain = ["codex:gpt-5.5", "claude:claude-opus-4-7"]
"#;
    let config = Config::from_str(codex_first).unwrap();
    let entry = config.phases.merge.first_entry();
    assert_eq!(entry.engine, Engine::Codex);
    assert_eq!(entry.model.as_deref(), Some("gpt-5.5"));
}

// -----------------------------------------------------------------
// AC: 'Merger verdict is logged and stored in the run state but does
// NOT yet feed `classify_exit`.' The parsed verdict has to land on a
// `PhaseOutcomes` field so the runner can carry it across the gap
// from phase-8 dispatch to the PR/log build sites. Slice 2 (#124)
// wires the verdict into routing; in this slice it must be stored.
// -----------------------------------------------------------------

#[test]
fn phase_outcomes_carry_optional_merger_verdict_defaulting_to_none() {
    // Default `PhaseOutcomes` represents an unrun pipeline; the
    // merger verdict must be `None` (no run yet → no parseable
    // verdict). This is the slot the runner writes into when phase
    // 8 produces a recognised verdict line.
    let outcomes = PhaseOutcomes::default();
    assert_eq!(
        outcomes.merger_verdict, None,
        "default PhaseOutcomes.merger_verdict must be None",
    );
}

#[test]
fn phase_outcomes_merger_verdict_round_trips_all_three_variants() {
    // Pin every variant separately so a future enum extension can't
    // silently lose a value. Each verdict has to land on the slot
    // cleanly.
    for verdict in [
        MergerVerdict::Merge,
        MergerVerdict::HoldNoted,
        MergerVerdict::HoldDraft,
    ] {
        let outcomes = PhaseOutcomes {
            merger_verdict: Some(verdict),
            ..PhaseOutcomes::default()
        };
        assert_eq!(outcomes.merger_verdict, Some(verdict));
    }
}

// -----------------------------------------------------------------
// Note: the slice-1 `classify_exit_is_invariant_under_merger_verdict_in_slice_1`
// test that previously lived here is gone. Slice 2 (issue #124 /
// ADR-0009) wires the merger verdict into routing, so the slice-1
// invariance no longer holds: `Some(Merge)` is now a Success
// signal, `Some(HoldNoted)` is `SuccessWithNotes`, and
// `Some(HoldDraft)` is `AgentSelfReportedFailure`. The slice-2
// precedence-table tests live in `tests/policy_merger_routing.rs`.
// -----------------------------------------------------------------

#[test]
fn config_phases_merge_rejects_empty_cli_chain() {
    // Same shape as the other phases: explicit `cli_chain = []` is
    // rejected at config-load time so an operator can't silently end
    // up with a phase that has nothing to dispatch.
    let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"

[phases.merge]
cli_chain = []
"#;
    assert!(
        Config::from_str(config_text).is_err(),
        "empty merge cli_chain must be rejected at config-load time"
    );
}
