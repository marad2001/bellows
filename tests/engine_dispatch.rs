//! Engine-dispatch acceptance criteria for issue #81 / ADR-0005.
//!
//! Each test pins one acceptance criterion to a checkable surface:
//! - codex stderr signature matching (rate-limit + auth-error)
//! - engine-aware render_kickoff (Codex variant inlines context+skills)
//! - BELLOWS_ENGINE / BELLOWS_MODEL env vars set per-phase
//! - --engine flag default derived from first
//!   `phases.implement.cli_chain` entry
//! - auth-error callout in run-log names the engine to refresh

use std::str::FromStr;

use bellows::auth::Auth;
use bellows::config::{Config, Engine};
use bellows::policy::{
    is_auth_error_signature, is_claude_auth_error_signature, is_codex_auth_error_signature,
    is_rate_limit_signature, render_kickoff_for_engine, wrap_phase_prompt_for_engine,
};

// -----------------------------------------------------------------
// AC: Codex rate-limit + auth-error stderr signatures (issue #79's
// spike findings, sourced from codex-rs/codex-api/src/error.rs).
// -----------------------------------------------------------------

#[test]
fn rate_limit_matches_codex_quota_exceeded_substring() {
    // Subscription users (primary path) — codex emits a `quota
    // exceeded` line on stderr when the ChatGPT subscription throttles.
    let codex_quota = "codex: error: quota exceeded for this billing window\n";
    assert!(
        is_rate_limit_signature(codex_quota),
        "codex `quota exceeded` must match: {codex_quota:?}",
    );
}

#[test]
fn rate_limit_matches_codex_rate_limit_colon_substring() {
    // Platform-API users (secondary path) — codex emits a `rate
    // limit:` prefix when the OpenAI platform throttles.
    let codex_rate = "codex: rate limit: 60 requests/minute exceeded\n";
    assert!(
        is_rate_limit_signature(codex_rate),
        "codex `rate limit:` must match: {codex_rate:?}",
    );
}

#[test]
fn auth_error_matches_codex_composite_401_plus_missing_bearer() {
    // Composite match (issue #79 spike): `401 Unauthorized` AND
    // `Missing bearer or basic authentication`. A bare `401
    // Unauthorized` could be a false positive from unrelated HTTP 401
    // in agent-fetched content, so the composite is required.
    let codex_auth =
        "codex: error: 401 Unauthorized. Missing bearer or basic authentication credentials.";
    assert!(
        is_auth_error_signature(codex_auth),
        "codex composite must match: {codex_auth:?}",
    );
    assert!(
        is_codex_auth_error_signature(codex_auth),
        "codex-specific helper must match the composite",
    );
}

#[test]
fn auth_error_does_not_attribute_bare_401_to_codex() {
    // A bare `401 Unauthorized` without the codex-specific
    // `Missing bearer or basic authentication` companion substring
    // must NOT match the codex-specific helper. This is the false-
    // positive case the spike's composite design protects against
    // (agent-fetched web content can contain HTTP 401 status lines).
    let bare_401 = "404 Not Found ... 401 Unauthorized from upstream";
    assert!(
        !is_codex_auth_error_signature(bare_401),
        "bare 401 must not match codex composite",
    );
}

#[test]
fn auth_error_attributes_claude_signature_correctly() {
    // Per-engine attribution helpers (used by the run-log callout to
    // name the engine to refresh).
    let claude_signature = "Error: refresh_token_expired (request_id=abc)";
    assert!(
        is_claude_auth_error_signature(claude_signature),
        "claude `refresh_token_expired` must match the per-engine helper",
    );
    assert!(
        !is_codex_auth_error_signature(claude_signature),
        "the codex helper must NOT match claude-only signatures",
    );
}

// -----------------------------------------------------------------
// AC: engine-aware `render_kickoff`. Claude path unchanged; Codex
// path inlines operating-context body + baked skill bodies into the
// kickoff prompt.
// -----------------------------------------------------------------

#[test]
fn render_kickoff_for_engine_claude_is_unchanged_from_v1_shape() {
    // The v1 single-engine `render_kickoff` shape is the source of
    // truth for the failing-test commit-shape language; the engine-
    // aware variant must preserve it for Claude. We pin a couple of
    // load-bearing substrings rather than the whole body so prose
    // can flex without breaking the test.
    let prompt = render_kickoff_for_engine(
        Engine::Claude,
        "## Agent Brief\n\n**Summary:** Do the thing.",
        "https://github.com/owner/repo",
        "agent/42-do-thing",
    );
    assert!(prompt.contains("agent/42-do-thing"));
    assert!(prompt.contains("## Agent Brief"));
    assert!(prompt.contains("failing-test commit"));
    // Claude reads CLAUDE.md and skills on-demand — they should NOT
    // be inlined into the kickoff.
    assert!(
        !prompt.contains("# Operating context"),
        "claude kickoff must not inline the operating context: {prompt}",
    );
    assert!(
        !prompt.contains("# Baked skills"),
        "claude kickoff must not inline skill bodies",
    );
}

#[test]
fn render_kickoff_for_engine_codex_inlines_operating_context_and_skills() {
    let prompt = render_kickoff_for_engine(
        Engine::Codex,
        "## Agent Brief\n\n**Summary:** Do the thing.",
        "https://github.com/owner/repo",
        "agent/42-do-thing",
    );
    // The brief still appears.
    assert!(prompt.contains("## Agent Brief"));
    assert!(prompt.contains("agent/42-do-thing"));
    // The operating context + baked-skill bodies are inlined for
    // codex (per ADR-0005: "the codex path in `policy::render_kickoff`
    // inlines the operating-context body plus the bodies of all baked
    // skills directly into the kickoff prompt").
    assert!(
        prompt.contains("# Operating context"),
        "codex kickoff must inline the operating context",
    );
    assert!(
        prompt.contains("# Baked skills"),
        "codex kickoff must inline the baked skills",
    );
    // At least one identifying phrase from each baked skill body —
    // pinned loosely so the skill prose can flex.
    assert!(
        prompt.contains("## Skill: tdd"),
        "codex kickoff must inline the tdd skill body",
    );
    assert!(
        prompt.contains("## Skill: diagnose"),
        "codex kickoff must inline the diagnose skill body",
    );
    assert!(
        prompt.contains("## Skill: triage"),
        "codex kickoff must inline the triage skill body",
    );
}

#[test]
fn wrap_phase_prompt_for_engine_claude_is_identity() {
    // Phase-specific prompts (review, security-review, per-finding
    // fix, nit batch) go through `wrap_phase_prompt_for_engine`. The
    // claude path must be the identity function — claude already
    // reads operating context on-demand.
    let phase_body = "## Inputs\n\nfoo bar baz";
    let wrapped = wrap_phase_prompt_for_engine(Engine::Claude, phase_body);
    assert_eq!(wrapped, phase_body);
}

#[test]
fn wrap_phase_prompt_for_engine_codex_prepends_operating_context() {
    let phase_body = "## Inputs\n\nfoo bar baz";
    let wrapped = wrap_phase_prompt_for_engine(Engine::Codex, phase_body);
    assert!(wrapped.ends_with(phase_body) || wrapped.contains(phase_body));
    assert!(wrapped.contains("# Operating context"));
    assert!(wrapped.contains("# Baked skills"));
}

#[test]
fn wrap_phase_prompt_for_engine_codex_neutralises_claude_specific_phrasing() {
    // The codex kickoff inlines the operating-context body (whose
    // canonical copy is `policy-image/CLAUDE.md`) so codex sees the
    // same operating instructions claude does. But the canonical
    // copy is written in claude's voice: it identifies the agent as
    // "Claude Code" and tells it to read skills from "your skills
    // directory". Neither is true for the codex container — it has
    // no skills directory (skill bodies are inlined into the prompt
    // by `wrap_phase_prompt_for_engine` itself), and calling the
    // codex agent "Claude Code" is a misidentification that confuses
    // the model's self-context. Pin the neutralisation here so the
    // doc comment's "Claude-specific phrasing neutralised" promise
    // matches reality.
    let phase_body = "## Phase body\n\nirrelevant";
    let wrapped = wrap_phase_prompt_for_engine(Engine::Codex, phase_body);
    assert!(
        !wrapped.contains("Claude Code"),
        "codex kickoff must not call the agent \"Claude Code\": {wrapped}",
    );
    assert!(
        !wrapped.contains("your skills directory"),
        "codex kickoff must not point the agent at a non-existent skills \
         directory: {wrapped}",
    );
}

// -----------------------------------------------------------------
// AC: per-phase BELLOWS_ENGINE / BELLOWS_MODEL dispatch via Auth.
// -----------------------------------------------------------------

#[test]
fn auth_subscription_sets_engine_env_var_per_phase() {
    // The runner constructs one Auth per phase from the resolved
    // chain entry; the env-var dispatch is in
    // `auth.extra_env()`. Pin the contract here.
    let claude_auth = Auth::Subscription {
        engine: Engine::Claude,
        model: None,
        credentials_volume_name: "_".to_string(),
    };
    assert!(claude_auth.extra_env().iter().any(|e| e == "BELLOWS_ENGINE=claude"));
    let codex_auth = Auth::Subscription {
        engine: Engine::Codex,
        model: None,
        credentials_volume_name: "_".to_string(),
    };
    assert!(codex_auth.extra_env().iter().any(|e| e == "BELLOWS_ENGINE=codex"));
}

#[test]
fn auth_subscription_with_model_pin_exports_bellows_model_env_var() {
    // Chain entry with a model pin (`claude:opus-4-7`) → runner
    // exports BELLOWS_MODEL=opus-4-7 → the policy image's run-agent
    // script appends `-m opus-4-7` to the CLI invocation. Chain entry
    // without a model pin → no BELLOWS_MODEL env var → CLI uses its
    // default model.
    let pinned = Auth::Subscription {
        engine: Engine::Claude,
        model: Some("opus-4-7".to_string()),
        credentials_volume_name: "_".to_string(),
    };
    assert!(
        pinned.extra_env().iter().any(|e| e == "BELLOWS_MODEL=opus-4-7"),
        "model pin must produce a BELLOWS_MODEL env var",
    );
    let unpinned = Auth::Subscription {
        engine: Engine::Claude,
        model: None,
        credentials_volume_name: "_".to_string(),
    };
    assert!(
        !unpinned.extra_env().iter().any(|e| e.starts_with("BELLOWS_MODEL=")),
        "no model pin must produce no BELLOWS_MODEL env var",
    );
}

// -----------------------------------------------------------------
// AC: --engine flag default = first entry of
// phases.implement.cli_chain. Tested by reading config.phases since
// the actual flag resolution is in main.rs; the contract is that
// the config exposes the default source.
// -----------------------------------------------------------------

#[test]
fn config_phases_implement_first_entry_drives_default_engine_for_setup_auth() {
    // With no per-phase config the default chain is ["claude"], so
    // the setup-auth default is claude.
    let minimal = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"
"#;
    let config = Config::from_str(minimal).unwrap();
    assert_eq!(config.phases.implement.first_entry().engine, Engine::Claude);

    // With `phases.implement.cli_chain = ["codex:gpt-5.5", "claude"]`
    // the setup-auth default is codex (the engine of the first chain
    // entry; the model pin is ignored — login is per-subscription).
    let codex_first = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"

[phases.implement]
cli_chain = ["codex:gpt-5.5", "claude"]
"#;
    let config = Config::from_str(codex_first).unwrap();
    assert_eq!(config.phases.implement.first_entry().engine, Engine::Codex);
}
