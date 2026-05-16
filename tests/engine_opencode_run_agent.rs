//! AC15 of issue #120: the policy image's `run-agent` script branches
//! on BELLOWS_ENGINE=opencode and invokes the opencode CLI against the
//! DeepSeek backend.
//!
//! Tests are file-content greps rather than full container smoke
//! tests so they run inside the workspace without docker, matching
//! the existing `policy_image_engine_dispatch.rs` style.

fn read_run_agent() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("policy-image")
        .join("run-agent");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e))
}

#[test]
fn run_agent_has_an_opencode_case_arm() {
    // The case statement that dispatches on BELLOWS_ENGINE must have
    // an opencode arm alongside the existing claude and codex arms.
    let run_agent = read_run_agent();
    assert!(
        run_agent.contains("opencode)"),
        "run-agent must have an `opencode)` case arm: {run_agent}",
    );
}

#[test]
fn run_agent_opencode_arm_invokes_opencode_run() {
    // opencode v1.15+ uses `opencode run` for non-interactive
    // invocations (the analogue of `claude -p` / `codex exec`).
    let run_agent = read_run_agent();
    assert!(
        run_agent.contains("opencode run"),
        "opencode arm must invoke `opencode run`: {run_agent}",
    );
}

#[test]
fn run_agent_opencode_arm_closes_stdin_with_dev_null() {
    // Defence-in-depth against headless hangs: the codex arm needs
    // </dev/null to avoid hanging on EOF, and opencode (also
    // npm-shipped Node CLI built on similar primitives) is at risk
    // of the same failure mode. Close stdin defensively.
    let run_agent = read_run_agent();
    // The opencode arm should also redirect </dev/null (mirroring
    // codex's defence) — grep for at least two occurrences of
    // `</dev/null` so the opencode arm picks up the same hardening
    // as the codex arm without requiring a separate keyword.
    let occurrences = run_agent.matches("</dev/null").count();
    assert!(
        occurrences >= 2,
        "opencode arm must close stdin defensively with `</dev/null` (current occurrences: {occurrences}): {run_agent}",
    );
}

#[test]
fn run_agent_opencode_arm_threads_bellows_model_when_set() {
    // When the chain entry pins a model (e.g.
    // `"opencode:deepseek/deepseek-v4-pro"`), BELLOWS_MODEL is set
    // and the opencode invocation must pass it through via
    // `-m "$BELLOWS_MODEL"` (opencode accepts both `-m` and
    // `--model` per its CLI docs; pinning `-m` here matches the
    // codex arm's convention).
    let run_agent = read_run_agent();
    // The whole script must mention BELLOWS_MODEL (already true
    // for claude/codex), and the opencode block must contain a
    // pinned-model exec branch. We grep for the canonical pinned
    // invocation pattern.
    assert!(
        run_agent.contains(r#"opencode run -m "$BELLOWS_MODEL""#)
            || run_agent.contains(r#"opencode run --model "$BELLOWS_MODEL""#),
        "opencode arm must thread BELLOWS_MODEL through to opencode: {run_agent}",
    );
}

#[test]
fn run_agent_opencode_arm_does_not_require_dangerously_skip_permissions() {
    // opencode does not have a `--dangerously-skip-permissions` flag
    // (that's claude's invention). Pin that the opencode arm is NOT
    // accidentally carrying that flag forward via a copy-paste from
    // the claude arm.
    let run_agent = read_run_agent();
    // Walk line-by-line, find the opencode arm body, and assert it
    // does NOT mention --dangerously-skip-permissions.
    let mut in_opencode = false;
    let mut opencode_arm = String::new();
    for line in run_agent.lines() {
        if line.trim_start().starts_with("opencode)") {
            in_opencode = true;
            continue;
        }
        if in_opencode {
            if line.trim_start().starts_with(";;") {
                break;
            }
            opencode_arm.push_str(line);
            opencode_arm.push('\n');
        }
    }
    assert!(
        !opencode_arm.contains("--dangerously-skip-permissions"),
        "opencode arm must not carry --dangerously-skip-permissions (claude-only flag): {opencode_arm}",
    );
}

#[test]
fn run_agent_unknown_engine_message_now_mentions_opencode() {
    // The error message for an unrecognised BELLOWS_ENGINE must
    // name opencode as one of the accepted values now that it's a
    // first-class engine arm.
    let run_agent = read_run_agent();
    assert!(
        run_agent.contains("opencode"),
        "run-agent must mention opencode somewhere (case arm + error message): {run_agent}",
    );
    // Specifically check the error message references all three.
    assert!(
        run_agent.contains("'claude', 'codex', or 'opencode'")
            || run_agent.contains("claude, codex, or opencode")
            || run_agent.contains("claude/codex/opencode"),
        "unknown-engine error message must list opencode alongside claude and codex: {run_agent}",
    );
}
