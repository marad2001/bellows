//! ADR-0011: merge gating is mechanical-only; the phase-8 merger verdict
//! is advisory. These tests pin the new `classify_exit` contract, which
//! supersedes the ADR-0009 / issue #124 merger-routing precedence table
//! this file used to encode:
//!
//! 1. `classify_exit(&outcomes)` reads only the objective phase signals.
//! 2. The merger verdict (Merge / HoldNoted / HoldDraft) does NOT change
//!    routing — a clean run auto-merges regardless. The verdict surfaces
//!    as an advisory `## Merge verdict` PR comment, not a gate.
//! 3. The former (β) synth-provenance and (γ) coverage-backstop hard
//!    overrides no longer gate — an otherwise-clean run carrying a
//!    WeakTestGuard / ParserBackstop synth or a non-empty
//!    `backstop_violations` still routes to Success.
//! 4. The mechanical precedences still hold: wall-clock, rate-limit,
//!    non-zero implement exit, and a failing cargo gate each route to
//!    their objective `ExitReason`, beating any advisory signal.

use bellows::policy::{
    classify_exit, AnalysisOutcome, BaseHealth, BellowsSynthCause, CheckResult, ExitReason,
    FixOutcome,
    GateOutcome, ImplementOutcome, MergerVerdict, ParsedFinding, PhaseOutcomes, ReviewOutcome,
    Severity,
};

fn check(exit: i64) -> CheckResult {
    CheckResult {
        exit_code: exit,
        output: String::new(),
    }
}

/// Everything-green baseline: implement exit 0, both gates passing, no
/// synth spans, no verdict. Under ADR-0011 this auto-merges.
fn clean_outcomes() -> PhaseOutcomes {
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
        base_health: BaseHealth::NotEstablished,
        wall_clock_exceeded: false,
        backstop_violations: Vec::new(),
        implement_crash_synthesised: false,
        merger_verdict: None,
        merger_prose: None,
        synth_causes: Vec::new(),
        security: None,
        security_fix: None,
    }
}

// -----------------------------------------------------------------
// The merger verdict is advisory: it never gates.
// -----------------------------------------------------------------

#[test]
fn merge_verdict_is_advisory_clean_run_auto_merges() {
    let outcomes = PhaseOutcomes {
        merger_verdict: Some(MergerVerdict::Merge),
        ..clean_outcomes()
    };
    assert_eq!(classify_exit(&outcomes), ExitReason::Success);
}

#[test]
fn hold_noted_verdict_does_not_gate() {
    let outcomes = PhaseOutcomes {
        merger_verdict: Some(MergerVerdict::HoldNoted),
        ..clean_outcomes()
    };
    assert_eq!(
        classify_exit(&outcomes),
        ExitReason::Success,
        "ADR-0011: HoldNoted is advisory — it surfaces as a PR comment, not a gate",
    );
}

#[test]
fn hold_draft_verdict_does_not_gate() {
    let outcomes = PhaseOutcomes {
        merger_verdict: Some(MergerVerdict::HoldDraft),
        ..clean_outcomes()
    };
    assert_eq!(
        classify_exit(&outcomes),
        ExitReason::Success,
        "ADR-0011: HoldDraft is advisory — a clean pipeline auto-merges regardless",
    );
}

// -----------------------------------------------------------------
// The former (β)/(γ) hard overrides no longer gate.
// -----------------------------------------------------------------

#[test]
fn weak_test_guard_synth_no_longer_gates() {
    let mut outcomes = clean_outcomes();
    outcomes.synth_causes = vec![BellowsSynthCause::WeakTestGuard];
    assert_eq!(
        classify_exit(&outcomes),
        ExitReason::Success,
        "ADR-0011: the weak-test guard is advisory now — 'no new tests' no longer drafts",
    );
}

#[test]
fn parser_backstop_synth_no_longer_gates() {
    let mut outcomes = clean_outcomes();
    outcomes.synth_causes = vec![BellowsSynthCause::ParserBackstop];
    assert_eq!(classify_exit(&outcomes), ExitReason::Success);
}

#[test]
fn backstop_violations_no_longer_gate() {
    let mut outcomes = clean_outcomes();
    outcomes.backstop_violations = vec![ParsedFinding {
        title: "unhandled error path".to_string(),
        severity: Severity::Blocker,
        body: String::new(),
    }];
    assert_eq!(
        classify_exit(&outcomes),
        ExitReason::Success,
        "ADR-0011 (A-strict): an unaddressed blocker finding surfaces as a comment, not a gate",
    );
}

// -----------------------------------------------------------------
// The mechanical precedences still gate, and beat any advisory signal.
// -----------------------------------------------------------------

#[test]
fn wall_clock_exceeded_still_gates_over_merge_verdict() {
    let mut outcomes = clean_outcomes();
    outcomes.wall_clock_exceeded = true;
    outcomes.merger_verdict = Some(MergerVerdict::Merge);
    assert_eq!(classify_exit(&outcomes), ExitReason::WallClockExceeded);
}

#[test]
fn rate_limit_still_gates() {
    let mut outcomes = clean_outcomes();
    outcomes.implement = ImplementOutcome {
        exit_code: 1,
        stderr_tail: "rate_limit_error: anthropic API throttled".to_string(),
        engine: None,
    };
    assert_eq!(classify_exit(&outcomes), ExitReason::RateLimited);
}

#[test]
fn non_zero_implement_exit_still_gates() {
    let mut outcomes = clean_outcomes();
    outcomes.implement = ImplementOutcome {
        exit_code: 137,
        stderr_tail: "agent process killed".to_string(),
        engine: None,
    };
    assert_eq!(classify_exit(&outcomes), ExitReason::Crash);
}

#[test]
fn failing_post_implement_gate_still_gates() {
    let mut outcomes = clean_outcomes();
    outcomes.post_implement_gate = GateOutcome {
        cargo_clippy: Some(check(0)),
        cargo_test: Some(check(101)),
    };
    assert_eq!(classify_exit(&outcomes), ExitReason::FinalTestsRed);
}

#[test]
fn failing_end_pipeline_gate_still_gates() {
    let mut outcomes = clean_outcomes();
    outcomes.end_pipeline_gate = Some(GateOutcome {
        cargo_clippy: Some(check(1)),
        cargo_test: Some(check(0)),
    });
    assert_eq!(classify_exit(&outcomes), ExitReason::FinalTestsRed);
}

// -----------------------------------------------------------------
// ADR-0011 amendment: a non-rate-limit CRASH in a review / fix /
// security phase is a mechanical failure and drafts the PR — it must
// NOT silently auto-merge with that phase's work skipped (e.g. a
// mis-typed codex model pin crashes the review agent). The advisory
// phase-8 merger is excluded (its exit is not carried on
// PhaseOutcomes).
// -----------------------------------------------------------------

#[test]
fn review_phase_crash_gates_to_draft() {
    let mut outcomes = clean_outcomes();
    outcomes.review = Some(ReviewOutcome {
        findings_text: None,
        exit_code: 1,
    });
    assert_eq!(
        classify_exit(&outcomes),
        ExitReason::Crash,
        "a crashed review agent must draft, not auto-merge with review skipped",
    );
}

#[test]
fn review_fix_phase_crash_gates_to_draft() {
    let mut outcomes = clean_outcomes();
    outcomes.review_fix = Some(FixOutcome { exit_code: 137 });
    assert_eq!(classify_exit(&outcomes), ExitReason::Crash);
}

#[test]
fn security_review_phase_crash_gates_to_draft() {
    let mut outcomes = clean_outcomes();
    outcomes.security = Some(AnalysisOutcome {
        findings_text: None,
        exit_code: 1,
    });
    assert_eq!(classify_exit(&outcomes), ExitReason::Crash);
}

#[test]
fn security_fix_phase_crash_gates_to_draft() {
    let mut outcomes = clean_outcomes();
    outcomes.security_fix = Some(FixOutcome { exit_code: 1 });
    assert_eq!(classify_exit(&outcomes), ExitReason::Crash);
}

#[test]
fn phases_that_ran_and_exited_zero_do_not_gate() {
    // Regression: phases that completed (exit 0) never gate, even with
    // findings present — only a non-zero exit (crash) does. This is the
    // normal happy path with a reviewer that flagged nits.
    let mut outcomes = clean_outcomes();
    outcomes.review = Some(ReviewOutcome {
        findings_text: Some("### 1. minor naming — nit".to_string()),
        exit_code: 0,
    });
    outcomes.review_fix = Some(FixOutcome { exit_code: 0 });
    outcomes.security = Some(AnalysisOutcome {
        findings_text: None,
        exit_code: 0,
    });
    outcomes.security_fix = Some(FixOutcome { exit_code: 0 });
    assert_eq!(classify_exit(&outcomes), ExitReason::Success);
}
