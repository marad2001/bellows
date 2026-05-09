/// Classification of how an agent run ended. `policy::classify_exit`
/// produces this from the post-run signals; the runner uses it to choose
/// PR draft state, label, and log-comment shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    Success,
    AgentSelfReportedFailure,
    Crash,
    FinalTestsRed,
}

/// Decide how a finished agent run should be classified.
///
/// `cargo_test_result` is `None` when the workspace had no `Cargo.toml`
/// at the root and the gate was skipped — that case counts as success.
///
/// Precedence: an agent self-report (notes file present) wins over
/// everything else; the agent's voice always trumps tooling signals.
pub fn classify_exit(
    agent_exit_code: i64,
    has_agent_notes: bool,
    cargo_test_result: Option<i64>,
) -> ExitReason {
    if has_agent_notes {
        return ExitReason::AgentSelfReportedFailure;
    }
    if agent_exit_code != 0 {
        return ExitReason::Crash;
    }
    if matches!(cargo_test_result, Some(code) if code != 0) {
        return ExitReason::FinalTestsRed;
    }
    ExitReason::Success
}

/// Render the kickoff prompt that gets fed into `claude -p` inside the
/// sandbox. Pure function so it can be unit-tested without spinning up
/// a container.
pub fn render_kickoff(brief: &str, repo_url: &str, branch_name: &str) -> String {
    format!(
        "You are working on {repo_url} on branch `{branch_name}`.\n\
         \n\
         {brief}\n\
         \n\
         ## How to work\n\
         \n\
         Use the `tdd` skill: write failing tests first, then implement to green, then refactor.\n\
         The skill is available in your skills directory; invoke it before doing implementation work.\n\
         \n\
         ## Stop conditions\n\
         \n\
         Stop only when `cargo test` is green and your changes satisfy every acceptance criterion in the brief above.\n\
         Do NOT write a `.bellows-stub-marker` (or any other marker) file — the slice-2 stub agent is gone; only your real changes should appear in the resulting commit.\n\
         \n\
         When you are done, write a PR description body to `/workspace/.bellows-pr-description.md` summarising what you built, mapping each new test to the brief's acceptance criteria.\n"
    )
}
