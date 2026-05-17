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
    append_bellows_synth_entry, classify_agent_notes_with_synth_spans, classify_exit,
    synthesize_implement_crash_entry, synthesize_no_new_tests_entry,
    synthesize_unaddressed_entries, BellowsSynthCause, CheckResult, ExitReason, GateOutcome,
    ImplementOutcome, MergerVerdict, NotesShape, ParsedFinding, PhaseOutcomes, Severity,
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
