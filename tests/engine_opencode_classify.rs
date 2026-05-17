//! AC6 of issue #120: `classify_exit` returns `RateLimited` /
//! `AuthError` for an opencode run with **exit 0** plus matching
//! stderr signatures. Opencode v1.15.3 exits 0 on its 429 / 401
//! responses (the CLI reports the error to stderr and returns
//! cleanly), so the exit-code-only gate the Claude / Codex
//! signatures sit behind would otherwise miss the rate-limit /
//! auth-error.
//!
//! Adds `ImplementOutcome.engine: Option<Engine>` (so classify_exit
//! can route the opencode-specific signature precedence) and
//! `ExitReason::AuthError` (new ADR-0008 variant — distinct routing
//! from generic Crash so the run-log builder can name the engine to
//! refresh).

use bellows::config::Engine;
use bellows::policy::{classify_exit, ExitReason, ImplementOutcome, NotesShape, PhaseOutcomes};

#[test]
fn classify_exit_opencode_exit_zero_rate_limit_stderr_is_rate_limited() {
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: 0,
            stderr_tail: r#"{"name":"AI_APICallError","statusCode":429}"#.to_string(),
            engine: Some(Engine::Opencode),
        },
        ..PhaseOutcomes::default()
    };
    let r = classify_exit(NotesShape::Absent, &outcomes, None);
    assert_eq!(
        r,
        ExitReason::RateLimited,
        "opencode exit 0 + 429 signature must classify as RateLimited",
    );
}

#[test]
fn classify_exit_opencode_exit_zero_auth_error_stderr_is_auth_error() {
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: 0,
            stderr_tail: r#"{"name":"AI_APICallError","statusCode":401}"#.to_string(),
            engine: Some(Engine::Opencode),
        },
        ..PhaseOutcomes::default()
    };
    let r = classify_exit(NotesShape::Absent, &outcomes, None);
    assert_eq!(
        r,
        ExitReason::AuthError,
        "opencode exit 0 + 401 signature must classify as AuthError",
    );
}

#[test]
fn classify_exit_opencode_clean_exit_zero_is_success() {
    // No signature → success path is unchanged.
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: 0,
            stderr_tail: "ordinary opencode output, nothing wrong".to_string(),
            engine: Some(Engine::Opencode),
        },
        ..PhaseOutcomes::default()
    };
    let r = classify_exit(NotesShape::Absent, &outcomes, None);
    assert_eq!(r, ExitReason::Success);
}

#[test]
fn classify_exit_claude_exit_zero_with_rate_limit_signature_still_success() {
    // Claude does NOT participate in the "signature authoritative on
    // exit 0" rule — only opencode. The pre-existing behaviour
    // (signature only matters when exit_code != 0) must be preserved
    // for the claude path. None engine = the legacy / not-set case,
    // and must behave exactly like the pre-AC6 code path.
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: 0,
            stderr_tail: "rate_limited mentioned but exit 0".to_string(),
            engine: Some(Engine::Claude),
        },
        ..PhaseOutcomes::default()
    };
    let r = classify_exit(NotesShape::Absent, &outcomes, None);
    assert_eq!(
        r,
        ExitReason::Success,
        "claude exit 0 + rate-limit string still classifies as Success",
    );
}
