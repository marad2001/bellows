use bellows::policy::{classify_exit, render_kickoff, ExitReason};

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
fn classify_exit_returns_success_for_clean_run_with_tests_green() {
    assert_eq!(
        classify_exit(0, false, Some(0)),
        ExitReason::Success
    );
}

#[test]
fn classify_exit_returns_success_when_cargo_test_gate_was_skipped() {
    // None means the workspace had no Cargo.toml at root; the runner
    // skipped the cargo test gate. Non-Rust briefs are a valid use case.
    assert_eq!(classify_exit(0, false, None), ExitReason::Success);
}

#[test]
fn classify_exit_returns_self_reported_failure_when_agent_notes_present() {
    // agent-notes.md presence wins over exit code 0 AND green tests —
    // the agent's voice trumps everything.
    assert_eq!(
        classify_exit(0, true, Some(0)),
        ExitReason::AgentSelfReportedFailure
    );
}

#[test]
fn classify_exit_returns_crash_when_agent_exits_non_zero_without_notes() {
    // Agent process died (claude itself errored, OOM, etc.). No notes
    // file means the agent didn't get to write a structured report.
    assert_eq!(classify_exit(1, false, None), ExitReason::Crash);
    assert_eq!(classify_exit(137, false, Some(0)), ExitReason::Crash);
}

#[test]
fn classify_exit_returns_final_tests_red_when_cargo_test_failed() {
    // Agent thought it was done (exit 0, no notes), but the cargo test
    // gate caught failing tests.
    assert_eq!(
        classify_exit(0, false, Some(1)),
        ExitReason::FinalTestsRed
    );
    assert_eq!(
        classify_exit(0, false, Some(101)),
        ExitReason::FinalTestsRed
    );
}
