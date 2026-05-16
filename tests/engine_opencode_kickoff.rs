//! AC7 of issue #120: `render_kickoff_for_engine(Engine::Opencode, ...)`
//! must produce exactly the same kickoff body as
//! `render_kickoff_for_engine(Engine::Claude, ...)`. Opencode
//! auto-discovers `AGENTS.md` (and per-skill markdown) from
//! `~/.config/opencode/` inside the container — the same shape as
//! claude reading `CLAUDE.md` + the skills directory from disk — so
//! the runner does not inline the operating context into the kickoff
//! prompt. The wrapper is the identity function for opencode for the
//! same reason it is for claude.
//!
//! This also pins the parallel `wrap_phase_prompt_for_engine`
//! identity contract for opencode at the phase boundary, so the same
//! parity applies to every agent-invoking phase (implement, review,
//! review-fix, security-review, security-fix).

use bellows::config::Engine;
use bellows::policy::{render_kickoff_for_engine, wrap_phase_prompt_for_engine};

#[test]
fn render_kickoff_for_engine_opencode_matches_claude_body() {
    let brief = "Stub brief body for the kickoff parity test.";
    let repo_url = "https://github.com/example/repo";
    let branch_name = "agent/120-opencode";
    let claude = render_kickoff_for_engine(Engine::Claude, brief, repo_url, branch_name);
    let opencode = render_kickoff_for_engine(Engine::Opencode, brief, repo_url, branch_name);
    assert_eq!(
        opencode, claude,
        "opencode kickoff body must equal claude kickoff body (identity-wrap parity)",
    );
}

#[test]
fn wrap_phase_prompt_for_engine_opencode_is_identity() {
    let phase_body =
        "## Phase prompt\n\nDo the thing. Then do the next thing. Then stop.\n";
    let wrapped = wrap_phase_prompt_for_engine(Engine::Opencode, phase_body);
    assert_eq!(
        wrapped, phase_body,
        "wrap_phase_prompt_for_engine(Opencode, body) must be the identity function \
         (opencode auto-discovers AGENTS.md from disk, same as claude with CLAUDE.md)",
    );
}
