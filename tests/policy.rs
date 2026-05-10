use bellows::policy::{
    classify_exit, render_kickoff, CheckResult, ExitReason, GateOutcome, ImplementOutcome,
    PhaseOutcomes, ReviewOutcome,
};

fn check(exit: i64) -> CheckResult {
    CheckResult { exit_code: exit, output: String::new() }
}

#[test]
fn rendered_kickoff_includes_the_agent_brief_body() {
    let brief = "## Agent Brief\n\n**Summary:** Do the thing.";
    let prompt = render_kickoff(brief, "https://github.com/owner/repo", "agent/42-do-thing");
    assert!(prompt.contains(brief), "brief missing from prompt: {prompt}");
}

#[test]
fn rendered_kickoff_includes_branch_name_and_repo_url() {
    let prompt = render_kickoff(
        "any brief",
        "https://github.com/owner/repo",
        "agent/42-do-thing",
    );
    assert!(
        prompt.contains("agent/42-do-thing"),
        "branch name missing: {prompt}"
    );
    assert!(
        prompt.contains("https://github.com/owner/repo"),
        "repo url missing: {prompt}"
    );
}

#[test]
fn rendered_kickoff_includes_stop_conditions_and_tooling_hints() {
    let prompt = render_kickoff("any brief", "https://github.com/owner/repo", "agent/42-x");
    assert!(prompt.contains("tdd"), "tdd skill mention missing: {prompt}");
    assert!(prompt.contains("cargo test"), "cargo test mention missing: {prompt}");
    assert!(prompt.contains("marker"), "marker file mention missing: {prompt}");
}

#[test]
fn classify_exit_returns_success_when_all_phases_clean() {
    // Tracer bullet for slice X1: every phase produced a clean exit and
    // every cargo gate's clippy + test passed. No findings, so review-fix
    // didn't run. Both gates ran (Cargo.toml is at the workspace root).
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: 0,
            stderr_tail: String::new(),
        },
        post_implement_gate: GateOutcome {
            cargo_clippy: Some(check(0)),
            cargo_test: Some(check(0)),
        },
        review: Some(ReviewOutcome {
            findings_text: None,
            exit_code: 0,
        }),
        review_fix: None,
        end_pipeline_gate: Some(GateOutcome {
            cargo_clippy: Some(check(0)),
            cargo_test: Some(check(0)),
        }),
    };
    assert_eq!(classify_exit(false, &outcomes), ExitReason::Success);
}

/// Helper for migrated tests: an `Outcomes` shape representing the
/// slice-5 path (only the post-implement gate populated, no review,
/// no end gate). Each test tweaks one field to express its scenario.
fn slice5_shaped(implement_exit: i64, cargo_test: Option<i64>) -> PhaseOutcomes {
    PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: implement_exit,
            stderr_tail: String::new(),
        },
        post_implement_gate: GateOutcome {
            cargo_clippy: None,
            cargo_test: cargo_test.map(check),
        },
        review: None,
        review_fix: None,
        end_pipeline_gate: None,
    }
}

#[test]
fn classify_exit_returns_success_for_clean_run_with_tests_green() {
    assert_eq!(
        classify_exit(false, &slice5_shaped(0, Some(0))),
        ExitReason::Success
    );
}

#[test]
fn classify_exit_returns_success_when_cargo_test_gate_was_skipped() {
    // None means the workspace had no Cargo.toml at root; the runner
    // skipped the cargo test gate. Non-Rust briefs are a valid use case.
    assert_eq!(
        classify_exit(false, &slice5_shaped(0, None)),
        ExitReason::Success
    );
}

#[test]
fn classify_exit_returns_self_reported_failure_when_agent_notes_present() {
    // agent-notes.md presence wins over exit code 0 AND green tests —
    // the agent's voice trumps everything.
    assert_eq!(
        classify_exit(true, &slice5_shaped(0, Some(0))),
        ExitReason::AgentSelfReportedFailure
    );
}

#[test]
fn classify_exit_returns_crash_when_agent_exits_non_zero_without_notes() {
    // Agent process died (claude itself errored, OOM, etc.). No notes
    // file means the agent didn't get to write a structured report.
    assert_eq!(
        classify_exit(false, &slice5_shaped(1, None)),
        ExitReason::Crash
    );
    assert_eq!(
        classify_exit(false, &slice5_shaped(137, Some(0))),
        ExitReason::Crash
    );
}

#[test]
fn classify_exit_returns_final_tests_red_when_post_implement_gate_clippy_failed() {
    // Implement run was clean (exit 0, no notes) and cargo test passed,
    // but clippy flagged something — gate fails on clippy alone.
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new() },
        post_implement_gate: GateOutcome {
            cargo_clippy: Some(check(101)),
            cargo_test: Some(check(0)),
        },
        review: None,
        review_fix: None,
        end_pipeline_gate: None,
    };
    assert_eq!(classify_exit(false, &outcomes), ExitReason::FinalTestsRed);
}

#[test]
fn classify_exit_returns_final_tests_red_when_end_pipeline_gate_failed() {
    // Post-implement gate was clean. Review ran and produced findings,
    // review-fix addressed them, but the fixups broke a test — caught
    // by the end-of-pipeline gate.
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new() },
        post_implement_gate: GateOutcome {
            cargo_clippy: Some(check(0)),
            cargo_test: Some(check(0)),
        },
        review: Some(ReviewOutcome { findings_text: Some("found stuff".to_string()), exit_code: 0 }),
        review_fix: Some(bellows::policy::FixOutcome { exit_code: 0 }),
        end_pipeline_gate: Some(GateOutcome {
            cargo_clippy: Some(check(0)),
            cargo_test: Some(check(101)),
        }),
    };
    assert_eq!(classify_exit(false, &outcomes), ExitReason::FinalTestsRed);
}

#[test]
fn classify_exit_returns_final_tests_red_when_cargo_test_failed() {
    // Agent thought it was done (exit 0, no notes), but the cargo test
    // gate caught failing tests.
    assert_eq!(
        classify_exit(false, &slice5_shaped(0, Some(1))),
        ExitReason::FinalTestsRed
    );
    assert_eq!(
        classify_exit(false, &slice5_shaped(0, Some(101))),
        ExitReason::FinalTestsRed
    );
}
