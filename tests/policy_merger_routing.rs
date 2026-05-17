//! Phase-8 merger-verdict routing precedence table (issue #124 / ADR-0009
//! slice 2). These tests pin the new behaviour of `classify_exit`:
//!
//! 1. The signature gains an `Option<MergerVerdict>` parameter.
//! 2. When the verdict is `Some` and no (β) synth-provenance or (γ)
//!    coverage-backstop hard override fires, the verdict drives the
//!    agent-authored routing branch.
//! 3. When the verdict is `None`, behaviour is exactly the pre-slice
//!    classifier (Q4-Option-A fallback per ADR-0009 — merger failure
//!    is strictly additive on throughput, never regressive).
//! 4. (β) synth-provenance and (γ) coverage-backstop synths cannot be
//!    overridden by a `Merge` vote.
//! 5. The wall-clock-exceeded, rate-limit, non-zero implement exit,
//!    and gate-failed precedences still beat the merger verdict.

use bellows::policy::{
    classify_exit, BellowsSynthCause, CheckResult, ExitReason, GateOutcome, ImplementOutcome,
    MergerVerdict, NotesShape, PhaseOutcomes,
};

fn check(exit: i64) -> CheckResult {
    CheckResult {
        exit_code: exit,
        output: String::new(),
    }
}

/// Baseline `PhaseOutcomes` for the "everything green, agent-authored
/// `## Unaddressed finding:` heading" scenario: implement exit 0, both
/// gates passing, no synth spans recorded. This is the scenario PR-#121
/// crystallised — the substantive code is mergeable; only the heading
/// shape was holding the run as a draft.
fn clean_outcomes_with_agent_authored_heading() -> PhaseOutcomes {
    PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: 0,
            stderr_tail: String::new(),
            engine: None,
        },
        post_implement_gate: GateOutcome {
            cargo_clippy: Some(check(0)),
            cargo_test: Some(check(0)),
        },
        review: None,
        review_fix: None,
        end_pipeline_gate: Some(GateOutcome {
            cargo_clippy: Some(check(0)),
            cargo_test: Some(check(0)),
        }),
        wall_clock_exceeded: false,
        backstop_violations: Vec::new(),
        implement_crash_synthesised: false,
        merger_verdict: None,
        security: None,
        security_fix: None,
    }
}

/// Same baseline as above but framed for the informational-notes
/// scenario (ADR-0006's `<!-- bellows: informational -->` channel).
/// The `PhaseOutcomes` value itself is identical — `classify_exit`
/// takes `NotesShape` as a separate argument, so the helper just
/// documents which test case the fixture is supporting.
fn clean_outcomes_with_informational_notes() -> PhaseOutcomes {
    clean_outcomes_with_agent_authored_heading()
}

// -----------------------------------------------------------------
// AC1 + AC2-Merge tracer bullet: the signature gains
// `Option<MergerVerdict>`, and `Some(Merge)` over an agent-authored
// `HasUnaddressedFinding` (and otherwise-clean phases) routes to
// Success — the merger replaced the (α) auto-fatal heading branch.
// -----------------------------------------------------------------

#[test]
fn classify_exit_routes_merge_verdict_over_agent_authored_heading_to_success() {
    let outcomes = clean_outcomes_with_agent_authored_heading();
    assert_eq!(
        classify_exit(
            NotesShape::HasUnaddressedFinding,
            &outcomes,
            Some(MergerVerdict::Merge),
        ),
        ExitReason::Success,
        "merger Merge over agent-authored heading + clean phases must \
         route to Success — the merger replaces the (α) auto-fatal \
         heading branch (ADR-0009 / issue #124)",
    );
}

// -----------------------------------------------------------------
// AC2 — pin the brief-mandated mapping for each non-Merge verdict.
// -----------------------------------------------------------------

#[test]
fn classify_exit_routes_hold_noted_verdict_to_success_with_notes() {
    // Brief: 'MergerVerdict::HoldNoted → ExitReason::SuccessWithNotes
    // (non-draft + agent-noted label)'. The diff broadly satisfies
    // the ACs but a gap flagged in agent-notes.md should be visible
    // to a human reviewer — non-draft PR labelled agent-noted.
    let outcomes = clean_outcomes_with_agent_authored_heading();
    assert_eq!(
        classify_exit(
            NotesShape::HasUnaddressedFinding,
            &outcomes,
            Some(MergerVerdict::HoldNoted),
        ),
        ExitReason::SuccessWithNotes,
        "merger HoldNoted must route to SuccessWithNotes regardless of \
         the agent-authored heading shape (ADR-0009 / issue #124)",
    );
}

#[test]
fn classify_exit_routes_hold_draft_verdict_to_agent_self_reported_failure() {
    // Brief: 'MergerVerdict::HoldDraft → ExitReason::AgentSelfReportedFailure
    // (draft + agent-failed label)'. The merger judged the diff does
    // NOT satisfy the brief; open a draft PR so a human can take over.
    let outcomes = clean_outcomes_with_agent_authored_heading();
    assert_eq!(
        classify_exit(
            NotesShape::HasUnaddressedFinding,
            &outcomes,
            Some(MergerVerdict::HoldDraft),
        ),
        ExitReason::AgentSelfReportedFailure,
        "merger HoldDraft must route to AgentSelfReportedFailure so the \
         runner opens a draft PR labelled agent-failed (ADR-0009 / issue #124)",
    );
}

#[test]
fn classify_exit_routes_merge_verdict_over_informational_notes_to_success() {
    // A merger Merge over the informational channel routes to plain
    // Success, replacing the pre-slice-2 SuccessWithNotes shape that
    // InformationalOnly notes alone would have produced.
    let outcomes = clean_outcomes_with_informational_notes();
    assert_eq!(
        classify_exit(
            NotesShape::InformationalOnly,
            &outcomes,
            Some(MergerVerdict::Merge),
        ),
        ExitReason::Success,
        "merger Merge over informational notes must route to Success — \
         the merger explicitly judged the run mergeable",
    );
}

#[test]
fn classify_exit_routes_hold_noted_verdict_over_informational_notes_to_success_with_notes() {
    // HoldNoted is the merger's own assessment of "diff broadly OK
    // but flag for human review" and is independent of the
    // NotesShape-derived classification.
    let outcomes = clean_outcomes_with_informational_notes();
    assert_eq!(
        classify_exit(
            NotesShape::InformationalOnly,
            &outcomes,
            Some(MergerVerdict::HoldNoted),
        ),
        ExitReason::SuccessWithNotes,
    );
}

// -----------------------------------------------------------------
// AC3 — `None` verdict falls back to the exact pre-slice classifier
// (Q4-Option-A neutrality). A merger that didn't run, was skipped
// because the agent output couldn't be parsed, or hit a rate-limit
// must not regress routing. The three NotesShape variants below
// pin the pre-slice mapping byte-for-byte.
// -----------------------------------------------------------------

#[test]
fn classify_exit_none_verdict_falls_back_has_unaddressed_finding_to_agent_self_reported_failure() {
    // Pre-slice: agent-authored `## Unaddressed finding:` heading
    // alone routed to AgentSelfReportedFailure. Without a merger
    // verdict the slice-2 classifier must reproduce that exactly.
    let outcomes = clean_outcomes_with_agent_authored_heading();
    assert_eq!(
        classify_exit(NotesShape::HasUnaddressedFinding, &outcomes, None),
        ExitReason::AgentSelfReportedFailure,
        "None verdict + agent-authored heading must fall through to \
         the pre-slice AgentSelfReportedFailure routing — merger \
         failure is strictly additive on throughput (Q4-Option-A)",
    );
}

#[test]
fn classify_exit_none_verdict_falls_back_informational_only_to_success_with_notes() {
    // Pre-slice: ADR-0006's informational channel routed to
    // SuccessWithNotes (non-draft + agent-noted label).
    let outcomes = clean_outcomes_with_informational_notes();
    assert_eq!(
        classify_exit(NotesShape::InformationalOnly, &outcomes, None),
        ExitReason::SuccessWithNotes,
        "None verdict + informational notes must fall through to \
         the pre-slice SuccessWithNotes routing",
    );
}

// -----------------------------------------------------------------
// AC4 — (β) synth-provenance hard override. A recorded
// `BellowsSynthCause` of `WeakTestGuard` / `ParserBackstop` /
// `ImplementCrash` is out-of-band evidence that Bellows itself
// authored an `## Unaddressed finding:` span. The merger cannot
// vote past that: a `Merge` verdict over any of these synth
// causes must route to `AgentSelfReportedFailure`.
// -----------------------------------------------------------------

#[test]
fn classify_exit_synth_weak_test_guard_overrides_merge_verdict() {
    let mut outcomes = clean_outcomes_with_agent_authored_heading();
    outcomes.synth_causes = vec![BellowsSynthCause::WeakTestGuard];
    assert_eq!(
        classify_exit(
            NotesShape::HasUnaddressedFinding,
            &outcomes,
            Some(MergerVerdict::Merge),
        ),
        ExitReason::AgentSelfReportedFailure,
        "WeakTestGuard synth-provenance must override merger Merge — \
         the guard's purpose is to fail runs where tests are weak, \
         and the merger cannot vote past that",
    );
}

#[test]
fn classify_exit_synth_parser_backstop_overrides_merge_verdict() {
    let mut outcomes = clean_outcomes_with_agent_authored_heading();
    outcomes.synth_causes = vec![BellowsSynthCause::ParserBackstop];
    assert_eq!(
        classify_exit(
            NotesShape::HasUnaddressedFinding,
            &outcomes,
            Some(MergerVerdict::Merge),
        ),
        ExitReason::AgentSelfReportedFailure,
        "ParserBackstop synth-provenance must override merger Merge — \
         the backstop detected an unaddressed/unexplained finding the \
         agent silently skipped",
    );
}

#[test]
fn classify_exit_synth_implement_crash_overrides_merge_verdict() {
    let mut outcomes = clean_outcomes_with_agent_authored_heading();
    outcomes.synth_causes = vec![BellowsSynthCause::ImplementCrash];
    assert_eq!(
        classify_exit(
            NotesShape::HasUnaddressedFinding,
            &outcomes,
            Some(MergerVerdict::Merge),
        ),
        ExitReason::AgentSelfReportedFailure,
        "ImplementCrash synth-provenance must override merger Merge — \
         the implement phase crashed and the synth is recovery scaffolding, \
         not an agent-authored mergeable diff",
    );
}

#[test]
fn classify_exit_no_synth_causes_lets_merge_verdict_win() {
    // Control: empty synth_causes leaves the Merge verdict intact.
    let outcomes = clean_outcomes_with_agent_authored_heading();
    assert!(
        outcomes.synth_causes.is_empty(),
        "default PhaseOutcomes must have no recorded synth causes",
    );
    assert_eq!(
        classify_exit(
            NotesShape::HasUnaddressedFinding,
            &outcomes,
            Some(MergerVerdict::Merge),
        ),
        ExitReason::Success,
        "with no synth-provenance recorded, the merger Merge verdict \
         drives routing per AC1",
    );
}

#[test]
fn classify_exit_none_verdict_falls_back_absent_notes_to_success() {
    // Pre-slice: no agent-notes.md at all (or fully empty) routed
    // to plain Success.
    let outcomes = clean_outcomes_with_agent_authored_heading();
    assert_eq!(
        classify_exit(NotesShape::Absent, &outcomes, None),
        ExitReason::Success,
        "None verdict + absent notes must fall through to the \
         pre-slice Success routing",
    );
}

