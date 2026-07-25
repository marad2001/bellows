//! Pins the load-bearing phrases of the stop-signal bullet in the
//! `## Hard constraints` section of `policy-image/CLAUDE.md`. The stop
//! signal must name BOTH `cargo test` AND `cargo clippy` green — but the
//! clippy scope is NOT hardcoded: the agent is directed to match the
//! target repo's own CI clippy invocation, which bellows' cargo-checks
//! gate mirrors (ADR-0004). Hardcoding a verbatim `-D warnings` here
//! (the pre-fix contract from issue #138) made the agent chase a
//! baseline of tolerated pedantic warnings on repos that scope clippy
//! more narrowly (e.g. `-D clippy::correctness -D clippy::suspicious`).
//!
//! The existing "don't stop earlier and don't keep going after that
//! signal is met" framing must be preserved, and the wording must
//! make plain that the same exemptions that apply to the test gate
//! apply to the clippy gate (doc-only / test-enforcement-exempt
//! briefs are not tightened by this change).
//!
//! A drive-by edit that drops any of the load-bearing phrases must
//! flip these tests red.

fn read_policy_image_claude_md() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("policy-image")
        .join("CLAUDE.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e))
}

fn hard_constraints_section(body: &str) -> &str {
    let (_, after_heading) = body
        .split_once("## Hard constraints")
        .expect("policy-image/CLAUDE.md must contain `## Hard constraints`");
    // The section runs until the next `## ` heading.
    match after_heading.split_once("\n## ") {
        Some((section, _)) => section,
        None => after_heading,
    }
}

#[test]
fn stop_signal_names_cargo_test_green() {
    // AC: stop signal still names `cargo test` green. The clippy
    // addition rides alongside `cargo test` — it does not replace
    // it.
    let body = read_policy_image_claude_md();
    let section = hard_constraints_section(&body);
    assert!(
        section.contains("`cargo test`"),
        "Stop-signal bullet must still name `cargo test` (in backticks) as a gate: {section}",
    );
}

#[test]
fn stop_signal_directs_agent_to_match_target_ci_clippy_scope() {
    // AC (harness fix): the clippy stop-signal must NOT hardcode a fixed
    // flag string. Repos scope clippy differently (e.g.
    // `-D clippy::correctness -D clippy::suspicious`, tolerating a
    // baseline of pedantic warnings); the agent must match the target
    // repo's own CI invocation, which bellows' cargo-checks gate mirrors
    // (ADR-0004). Pinning a verbatim `-D warnings` here made the agent
    // chase warnings on repos that tolerate them.
    let body = read_policy_image_claude_md();
    let section = hard_constraints_section(&body);
    let lower = section.to_lowercase();
    assert!(
        lower.contains("cargo clippy"),
        "Stop-signal bullet must still name `cargo clippy` as a gate: {section}",
    );
    assert!(
        lower.contains(".github/workflows") && lower.contains("target repo's ci"),
        "Stop-signal bullet must direct the agent to match the TARGET repo's CI clippy \
         scope (naming `.github/workflows`), not a hardcoded flag string: {section}",
    );
}

#[test]
fn stop_signal_requires_both_gates_green() {
    // AC: `policy-image/CLAUDE.md` states the stop signal is BOTH
    // `cargo test` AND `cargo clippy ...` green. The wording must
    // make plain it is a conjunction, not an either-or.
    let body = read_policy_image_claude_md();
    let section = hard_constraints_section(&body);
    let lower = section.to_lowercase();
    assert!(
        lower.contains("both") && lower.contains("and"),
        "Stop-signal bullet must state that BOTH gates green is the stop signal (conjunction, \
         not either-or): {section}",
    );
}

#[test]
fn stop_signal_preserves_dont_stop_earlier_framing() {
    // AC: "The 'don't stop earlier and don't keep going after that
    // signal is met' guidance is preserved." This is the framing
    // that prevents the agent from declaring done before the gates
    // are green AND from over-engineering after they are green.
    let body = read_policy_image_claude_md();
    let section = hard_constraints_section(&body);
    assert!(
        section.contains("Don't stop earlier and don't keep going after that signal is met"),
        "Stop-signal bullet must preserve the verbatim 'Don't stop earlier and don't keep going \
         after that signal is met' framing: {section}",
    );
}

#[test]
fn stop_signal_scopes_exemptions_to_match_test_gate() {
    // AC: "Doc-only / test-enforcement-exempt briefs are not
    // tightened. The CLAUDE.md prose makes the scoping obvious
    // (e.g. 'the same exemptions that apply to the test gate apply
    // to the clippy gate')." The prose must say so without naming
    // specific labels (the labels live in config and may rename).
    let body = read_policy_image_claude_md();
    let section = hard_constraints_section(&body);
    let lower = section.to_lowercase();
    assert!(
        lower.contains("same exemptions") || lower.contains("exemptions that apply to the test gate"),
        "Stop-signal bullet must scope clippy-gate exemptions to match the test gate, so doc-only \
         / test-enforcement-exempt briefs are not tightened: {section}",
    );
}
