//! AC11 of issue #120: the dispatcher injects the opencode
//! DeepSeek API key into the agent container at run-time.
//!
//! Architecturally this is the moral equivalent of `docker run
//! --env-file <path>`: the env-file written at setup-auth time
//! (AC9) is read at container-create time, parsed for
//! `KEY=VALUE` lines, and each entry is appended to the container's
//! env array. Because bellows uses bollard's structured
//! `ContainerCreateBody.env: Vec<String>` rather than spawning the
//! `docker` CLI, the AC11 surface is the `Auth::EnvFile` variant
//! plus the pure helper `parse_env_file_lines`.

use std::os::unix::fs::PermissionsExt;

use bellows::auth::{parse_env_file_lines, Auth};
use bellows::config::Engine;
use bellows::main_helpers::write_opencode_env_file;

#[test]
fn parse_env_file_lines_extracts_key_value_pairs() {
    let body = "DEEPSEEK_API_KEY=sk-one\nOTHER_VAR=two\n";
    let parsed = parse_env_file_lines(body).expect("parse");
    assert_eq!(
        parsed,
        vec![
            "DEEPSEEK_API_KEY=sk-one".to_string(),
            "OTHER_VAR=two".to_string(),
        ],
    );
}

#[test]
fn parse_env_file_lines_ignores_blank_lines_and_comments() {
    // Comment lines (#) and blank lines are common in human-curated
    // env-files. The parser must skip them.
    let body = "# leading comment\n\nDEEPSEEK_API_KEY=sk-real\n\n# trailing\n";
    let parsed = parse_env_file_lines(body).expect("parse");
    assert_eq!(parsed, vec!["DEEPSEEK_API_KEY=sk-real".to_string()]);
}

#[test]
fn parse_env_file_lines_errors_on_malformed_line_without_equals() {
    let body = "DEEPSEEK_API_KEY=sk-ok\nsk-leaked-secret\n";
    let err = parse_env_file_lines(body).expect_err("must error on malformed line");
    let msg = format!("{err:#}");
    assert!(
        msg.to_lowercase().contains("malformed"),
        "error should explain the malformed env-file line: {msg}",
    );
    assert!(
        msg.contains("line 2"),
        "error should name the line number without echoing line content: {msg}",
    );
    assert!(
        !msg.contains("sk-leaked-secret"),
        "error must not echo malformed env-file content because it may contain a secret: {msg}",
    );
}

#[test]
fn auth_envfile_engine_returns_opencode() {
    // The Auth::EnvFile variant carries an engine just like
    // Auth::Subscription does; the runner uses it to set
    // `BELLOWS_ENGINE=opencode` on the spawned container.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("opencode.env");
    write_opencode_env_file(&path, "sk-test").expect("write");
    let auth = Auth::EnvFile {
        engine: Engine::Opencode,
        model: None,
        env_file_path: path,
    };
    assert_eq!(auth.engine(), Engine::Opencode);
}

#[test]
fn auth_envfile_extra_env_includes_bellows_engine_and_env_file_kv_pairs() {
    // The runner appends `auth.extra_env()` to the container's env
    // array. For opencode, that must include both:
    //   - `BELLOWS_ENGINE=opencode` (so the policy image's
    //     `run-agent` script branches to the opencode arm)
    //   - the parsed lines of the env-file (the DeepSeek API key)
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("opencode.env");
    write_opencode_env_file(&path, "sk-dispatch").expect("write");
    let auth = Auth::EnvFile {
        engine: Engine::Opencode,
        model: None,
        env_file_path: path,
    };
    let env = auth.extra_env();
    assert!(
        env.iter().any(|e| e == "BELLOWS_ENGINE=opencode"),
        "must export BELLOWS_ENGINE=opencode: {env:?}",
    );
    assert!(
        env.iter().any(|e| e == "DEEPSEEK_API_KEY=sk-dispatch"),
        "must inject the parsed env-file lines: {env:?}",
    );
}

#[test]
fn auth_envfile_extra_env_includes_bellows_model_when_pinned() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("opencode.env");
    write_opencode_env_file(&path, "sk-pinned").expect("write");
    let auth = Auth::EnvFile {
        engine: Engine::Opencode,
        model: Some("deepseek-chat".to_string()),
        env_file_path: path,
    };
    let env = auth.extra_env();
    assert!(
        env.iter().any(|e| e == "BELLOWS_MODEL=deepseek-chat"),
        "must export BELLOWS_MODEL when the chain entry pinned a model: {env:?}",
    );
}

#[test]
fn auth_envfile_extra_env_errors_when_env_file_missing() {
    // If the env-file does not exist (operator forgot to run
    // setup-auth, or path typo'd in config), the dispatcher must
    // surface that loudly rather than silently spawn an unauthenticated
    // container that would later 401.
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("does-not-exist.env");
    let auth = Auth::EnvFile {
        engine: Engine::Opencode,
        model: None,
        env_file_path: missing,
    };
    let result = auth.try_extra_env();
    assert!(
        result.is_err(),
        "missing env-file must surface as an error, not a silent unauthenticated container",
    );
}

#[test]
fn auth_envfile_refuses_to_read_world_readable_env_file() {
    // Defence-in-depth: the env-file should already be 0600 from
    // setup-auth, but if an operator (or backup-restore) loosened it,
    // refuse to read a world-readable file rather than ship the
    // API key to a container under permissive perms.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("opencode.env");
    write_opencode_env_file(&path, "sk-loose").expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("loosen");
    let auth = Auth::EnvFile {
        engine: Engine::Opencode,
        model: None,
        env_file_path: path,
    };
    let result = auth.try_extra_env();
    assert!(
        result.is_err(),
        "world-readable env-file must surface as an error",
    );
}
