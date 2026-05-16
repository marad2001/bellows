//! AC12 of issue #120: the run-log / PR body for `ExitReason::AuthError`
//! names the specific engine to refresh, not a generic `<engine>`
//! placeholder. When the implement phase ran on opencode and surfaced
//! an auth signature, the callout must read
//! `bellows refresh-auth --engine opencode`. When the implement
//! phase ran on claude or codex, the callout names that engine
//! instead. The operator can copy-paste the suggested command
//! straight from the PR body.

use bellows::config::Engine;
use bellows::policy::{ImplementOutcome, PhaseOutcomes};
use bellows::runner::pr_body_for_auth_error;

#[test]
fn pr_body_for_auth_error_names_opencode_when_implement_engine_was_opencode() {
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: 0,
            stderr_tail: r#"{"name":"AI_APICallError","statusCode":401}"#.to_string(),
            engine: Some(Engine::Opencode),
        },
        ..PhaseOutcomes::default()
    };
    let body = pr_body_for_auth_error(&outcomes);
    assert!(
        body.contains("bellows refresh-auth --engine opencode"),
        "auth-error callout must name opencode by name: {body}",
    );
}

#[test]
fn pr_body_for_auth_error_names_claude_when_implement_engine_was_claude() {
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: 1,
            stderr_tail: "credit balance is too low".to_string(),
            engine: Some(Engine::Claude),
        },
        ..PhaseOutcomes::default()
    };
    let body = pr_body_for_auth_error(&outcomes);
    assert!(
        body.contains("bellows refresh-auth --engine claude"),
        "auth-error callout must name claude when implement engine was claude: {body}",
    );
}

#[test]
fn pr_body_for_auth_error_names_codex_when_implement_engine_was_codex() {
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: 1,
            stderr_tail: "401 Unauthorized".to_string(),
            engine: Some(Engine::Codex),
        },
        ..PhaseOutcomes::default()
    };
    let body = pr_body_for_auth_error(&outcomes);
    assert!(
        body.contains("bellows refresh-auth --engine codex"),
        "auth-error callout must name codex when implement engine was codex: {body}",
    );
}

#[test]
fn pr_body_for_auth_error_falls_back_to_engine_placeholder_when_engine_unknown() {
    // Legacy outcomes (engine: None) keep the generic `<engine>`
    // placeholder so the v1 / pre-AC12 callouts behave unchanged.
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: 1,
            stderr_tail: "401 Unauthorized".to_string(),
            engine: None,
        },
        ..PhaseOutcomes::default()
    };
    let body = pr_body_for_auth_error(&outcomes);
    assert!(
        body.contains("bellows refresh-auth --engine <engine>"),
        "engine-unknown auth-error callout must use the generic placeholder: {body}",
    );
}
