//! Policy-image-side multi-engine acceptance criteria (issue #81 /
//! ADR-0005). The bellows runner sets `BELLOWS_ENGINE` (and optionally
//! `BELLOWS_MODEL`) per-phase before each container starts; the policy
//! image's `run-agent` script branches on those env vars to invoke
//! either claude or codex with the right flags. The Dockerfile bakes
//! both CLIs (claude pinned via npm @<version>, codex pinned to the
//! rust-v0.130.0 release binary per issue #79's spike).
//!
//! Tests are file-content greps rather than full container smoke
//! tests so they run inside the workspace without docker.

fn read_policy_image_file(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("policy-image")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e))
}

// -----------------------------------------------------------------
// AC: Dockerfile bakes both claude-code and codex, both pinned.
// -----------------------------------------------------------------

#[test]
fn dockerfile_bakes_claude_code_at_a_pinned_version() {
    // Resolves the existing TODO at policy-image/Dockerfile lines
    // 11-15. The brief: "Policy image: bakes both
    // @anthropic-ai/claude-code and the Codex CLI, both pinned."
    // We pin "pinned" by requiring `@anthropic-ai/claude-code@<X>`
    // rather than `@anthropic-ai/claude-code@latest`.
    let dockerfile = read_policy_image_file("Dockerfile");
    assert!(
        dockerfile.contains("@anthropic-ai/claude-code@"),
        "Dockerfile must install @anthropic-ai/claude-code: {dockerfile}",
    );
    assert!(
        !dockerfile.contains("@anthropic-ai/claude-code@latest"),
        "claude-code must be pinned to a specific version (not @latest): {dockerfile}",
    );
}

#[test]
fn dockerfile_installs_codex_pinned_to_rust_v0_130_0() {
    // Issue #79 spike findings: codex is pinned at `rust-v0.130.0`
    // and installed from the Linux musl release binary (not npm).
    // The version string must appear somewhere in the Dockerfile.
    let dockerfile = read_policy_image_file("Dockerfile");
    assert!(
        dockerfile.contains("rust-v0.130.0"),
        "Dockerfile must pin codex at rust-v0.130.0 per the #79 spike: {dockerfile}",
    );
    // The brief calls out "rust-v0.130.0 Linux musl binary rather
    // than `npm install -g @openai/codex`" — pin both halves.
    assert!(
        dockerfile.contains("codex"),
        "Dockerfile must install codex",
    );
}

#[test]
fn dockerfile_removes_the_pin_todo() {
    // The brief says the policy image change resolves the existing
    // TODO at lines 11-15. The TODO comment block names slice 3.5
    // and the @latest situation; the resolution removes both lines.
    let dockerfile = read_policy_image_file("Dockerfile");
    assert!(
        !dockerfile.contains("TODO(slice 3.5)"),
        "the slice-3.5 pinning TODO must be resolved in this slice",
    );
}

// -----------------------------------------------------------------
// AC: run-agent branches on BELLOWS_ENGINE; codex branch passes the
// load-bearing flag set surfaced by issue #79's spike.
// -----------------------------------------------------------------

#[test]
fn run_agent_script_branches_on_bellows_engine_env_var() {
    let run_agent = read_policy_image_file("run-agent");
    assert!(
        run_agent.contains("BELLOWS_ENGINE"),
        "run-agent must read BELLOWS_ENGINE to dispatch per-phase: {run_agent}",
    );
}

#[test]
fn run_agent_script_invokes_claude_for_engine_claude() {
    let run_agent = read_policy_image_file("run-agent");
    // The claude branch keeps today's `claude -p
    // --dangerously-skip-permissions` invocation unchanged.
    assert!(
        run_agent.contains("claude --dangerously-skip-permissions -p")
            || run_agent.contains("claude -p --dangerously-skip-permissions"),
        "run-agent must invoke claude with -p and --dangerously-skip-permissions for the claude branch: {run_agent}",
    );
}

#[test]
fn run_agent_script_invokes_codex_with_load_bearing_flags_for_engine_codex() {
    let run_agent = read_policy_image_file("run-agent");
    // The codex branch passes the spike-confirmed flag set:
    //   codex exec --dangerously-bypass-approvals-and-sandbox
    //              --skip-git-repo-check "$PROMPT" </dev/null
    // The `</dev/null` stdin closure is load-bearing — without it
    // codex hangs forever waiting for stdin EOF.
    assert!(
        run_agent.contains("codex exec"),
        "run-agent's codex branch must invoke `codex exec`: {run_agent}",
    );
    assert!(
        run_agent.contains("--dangerously-bypass-approvals-and-sandbox"),
        "codex branch must pass --dangerously-bypass-approvals-and-sandbox per the #79 spike: {run_agent}",
    );
    assert!(
        run_agent.contains("--skip-git-repo-check"),
        "codex branch must pass --skip-git-repo-check per the #79 spike: {run_agent}",
    );
    assert!(
        run_agent.contains("</dev/null"),
        "codex branch must close stdin with </dev/null (load-bearing per the #79 spike): {run_agent}",
    );
}

#[test]
fn run_agent_script_passes_dash_m_when_bellows_model_is_set() {
    let run_agent = read_policy_image_file("run-agent");
    // Issue #81 / ADR-0005 AC: "When the chain entry pins a model,
    // the runner appends `-m <model>`; when no model pin, the flag
    // is omitted." Implemented in the policy image by reading
    // BELLOWS_MODEL and conditionally appending `-m "$BELLOWS_MODEL"`.
    assert!(
        run_agent.contains("BELLOWS_MODEL"),
        "run-agent must read BELLOWS_MODEL to pass -m to the CLI when set: {run_agent}",
    );
    assert!(
        run_agent.contains("-m"),
        "run-agent must pass -m <model> to the CLI: {run_agent}",
    );
}

#[test]
fn run_agent_script_fails_on_unset_or_unrecognised_engine() {
    let run_agent = read_policy_image_file("run-agent");
    // ADR-0005: "A missing or unrecognised BELLOWS_ENGINE is a hard
    // error from the entrypoint — the bellows-side dispatcher always
    // sets it, so an unset value indicates a regression rather than a
    // degraded mode worth running." Pin that a failure path exists
    // for the unrecognised-engine case (sh exit, no fallthrough).
    assert!(
        run_agent.contains("unknown") || run_agent.contains("unrecognised") || run_agent.contains("unrecognized") || run_agent.contains("BELLOWS_ENGINE is not set") || run_agent.contains("exit "),
        "run-agent must hard-fail on unset or unrecognised BELLOWS_ENGINE: {run_agent}",
    );
}
