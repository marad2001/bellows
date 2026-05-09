use bellows::policy::render_kickoff;

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
