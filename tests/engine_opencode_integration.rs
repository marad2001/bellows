//! AC17 of issue #120: end-to-end integration test for the opencode
//! dispatch path. Stitches together the slice's per-AC surfaces so a
//! drive-by edit that breaks one of them flips a single named test
//! red, surfacing the regression at the dispatch level rather than
//! deep inside a per-AC unit test.
//!
//! Covers the Bellows-side dispatch flow up to (but not including)
//! the actual docker container run: chain parsing → engine selection
//! → Auth construction → extra_env composition → kickoff rendering →
//! signature classification. The container-side arms (run-agent
//! invocation, Dockerfile install) are pinned by AC14/AC15's
//! file-content tests; this test pins the in-process Rust glue that
//! reaches the container boundary.
//!
//! Why an integration test rather than a unit test per AC: the bugs
//! the per-AC tests can't catch are the ones that come from two
//! surfaces drifting (e.g. auth.engine() returns Opencode but
//! kickoff routes through Codex; signature classifier matches an
//! opencode 401 but exit_code threading drops the engine on the
//! floor). This test exercises the whole chain so a drift between
//! any two parts flips it red.

use std::os::unix::fs::PermissionsExt;

use bellows::auth::Auth;
use bellows::config::{ChainEntry, Engine};
use bellows::main_helpers::write_opencode_env_file;
use bellows::policy::{
    classify_exit, is_auth_error_signature, is_opencode_auth_error_signature,
    is_opencode_rate_limit_signature, is_rate_limit_signature, render_kickoff_for_engine,
    ExitReason, ImplementOutcome, NotesShape, PhaseOutcomes,
};
use bellows::runner::pr_body_for_auth_error;

/// Build the canonical opencode `Auth::EnvFile` an operator would
/// get after running `bellows setup-auth --engine opencode` and
/// configuring a chain entry of `"opencode:deepseek/deepseek-v4-pro"`.
fn build_opencode_auth_with_model(api_key: &str, model: &str) -> (tempfile::TempDir, Auth) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("opencode.env");
    write_opencode_env_file(&path, api_key).expect("write env-file");
    let auth = Auth::EnvFile {
        engine: Engine::Opencode,
        model: Some(model.to_string()),
        env_file_path: path,
    };
    (dir, auth)
}

#[test]
fn opencode_dispatch_end_to_end_chain_entry_to_extra_env() {
    // 1. Chain entry parses (AC1).
    let entry: ChainEntry = "opencode:deepseek/deepseek-v4-pro"
        .parse()
        .expect("opencode chain entry parses");
    assert_eq!(entry.engine, Engine::Opencode);
    assert_eq!(entry.model.as_deref(), Some("deepseek/deepseek-v4-pro"));

    // 2. Auth construction (AC9/AC11).
    let (_dir, auth) = build_opencode_auth_with_model("sk-integration", &entry.model.unwrap());
    assert_eq!(auth.engine(), Engine::Opencode);

    // 3. extra_env composes BELLOWS_ENGINE + BELLOWS_MODEL + the
    //    parsed env-file lines (AC11). The dispatcher passes the
    //    resulting Vec<String> as the container's env array.
    let env = auth.extra_env();
    assert!(
        env.iter().any(|e| e == "BELLOWS_ENGINE=opencode"),
        "extra_env must export BELLOWS_ENGINE=opencode: {env:?}",
    );
    assert!(
        env.iter().any(|e| e == "BELLOWS_MODEL=deepseek/deepseek-v4-pro"),
        "extra_env must export the pinned model: {env:?}",
    );
    assert!(
        env.iter().any(|e| e == "DEEPSEEK_API_KEY=sk-integration"),
        "extra_env must inject the parsed env-file lines: {env:?}",
    );
}

#[test]
fn opencode_dispatch_kickoff_is_identity_wrap_parity_with_claude() {
    // AC7: render_kickoff_for_engine for opencode is identity-wrap
    // (parity with claude), because opencode auto-discovers AGENTS.md
    // the same way claude auto-discovers CLAUDE.md.
    let body = "kickoff body for issue #120";
    let repo_url = "https://github.com/example/repo";
    let branch_name = "agent/120-opencode";
    let opencode_kickoff =
        render_kickoff_for_engine(Engine::Opencode, body, repo_url, branch_name);
    let claude_kickoff =
        render_kickoff_for_engine(Engine::Claude, body, repo_url, branch_name);
    assert_eq!(
        opencode_kickoff, claude_kickoff,
        "opencode kickoff must match claude (identity-wrap): \
         opencode={opencode_kickoff:?} claude={claude_kickoff:?}",
    );
}

#[test]
fn opencode_dispatch_classifies_401_as_auth_error_and_names_opencode_in_pr_body() {
    // AC5+AC6+AC12 stitched: an opencode auth-error stderr matches
    // the signature, classify_exit routes it to ExitReason::AuthError,
    // and pr_body_for_auth_error names opencode (not the generic
    // <engine> placeholder).
    let stderr_tail = r#"{"name":"AI_APICallError","statusCode":401,"message":"Unauthorized"}"#;
    assert!(
        is_opencode_auth_error_signature(stderr_tail),
        "opencode-specific helper must match the AI_APICallError 401",
    );
    assert!(
        is_auth_error_signature(stderr_tail),
        "the generic auth-error helper must also match",
    );

    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome {
            // opencode v1.15.3 exits 0 on 401 (per ADR-0008's
            // research findings), so the signature must be
            // authoritative — classify_exit cannot rely on the
            // exit code alone.
            exit_code: 0,
            stderr_tail: stderr_tail.to_string(),
            engine: Some(Engine::Opencode),
        },
        ..PhaseOutcomes::default()
    };
    let reason = classify_exit(NotesShape::Absent, &outcomes);
    assert!(
        matches!(reason, ExitReason::AuthError),
        "opencode 401 must classify as AuthError even on exit 0: {reason:?}",
    );

    let body = pr_body_for_auth_error(&outcomes);
    assert!(
        body.contains("bellows refresh-auth --engine opencode"),
        "PR-body callout must name opencode by name: {body}",
    );
}

#[test]
fn opencode_dispatch_classifies_429_as_rate_limited() {
    // AC4: opencode rate-limit signature matches and routes the run
    // to ExitReason::RateLimited (leave the PR open for re-run when
    // the cooling window clears).
    let stderr_tail = r#"{"name":"AI_APICallError","statusCode":429}"#;
    assert!(
        is_opencode_rate_limit_signature(stderr_tail),
        "opencode-specific rate-limit helper must match the 429",
    );
    assert!(
        is_rate_limit_signature(stderr_tail),
        "the generic rate-limit helper must also match",
    );

    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: 0,
            stderr_tail: stderr_tail.to_string(),
            engine: Some(Engine::Opencode),
        },
        ..PhaseOutcomes::default()
    };
    let reason = classify_exit(NotesShape::Absent, &outcomes);
    assert!(
        matches!(reason, ExitReason::RateLimited),
        "opencode 429 must classify as RateLimited even on exit 0: {reason:?}",
    );
}

#[test]
fn opencode_dispatch_env_file_permissions_are_0600_after_setup() {
    // AC9 / AC11 stitched: after setup-auth writes the env-file,
    // its mode is 0600 — and the dispatcher's permission check
    // (extra_env path) returns Ok for that mode and Err if the
    // mode is loosened to 0644.
    let (dir, _auth_pinned) =
        build_opencode_auth_with_model("sk-perm-check", "deepseek/deepseek-v4-pro");
    let path = dir.path().join("opencode.env");
    let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "setup-auth must produce a 0600 env-file");

    // try_extra_env must succeed at 0600...
    let auth_ok = Auth::EnvFile {
        engine: Engine::Opencode,
        model: None,
        env_file_path: path.clone(),
    };
    assert!(
        auth_ok.try_extra_env().is_ok(),
        "0600 env-file must pass the dispatcher's permission check",
    );

    // ...and fail when the operator (or backup-restore) loosens it.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("loosen");
    let auth_loose = Auth::EnvFile {
        engine: Engine::Opencode,
        model: None,
        env_file_path: path,
    };
    assert!(
        auth_loose.try_extra_env().is_err(),
        "world-readable env-file must fail the dispatcher's permission check",
    );
}
