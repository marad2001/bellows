//! AC16 of issue #120: the operator-facing README and the
//! `orchestrator.example.toml` sample mention opencode as a
//! first-class engine alongside claude and codex, so an operator
//! reading either file discovers the third engine without having to
//! read the source.

use std::fs;
use std::path::PathBuf;

fn read(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} must exist: {}", path.display(), e))
}

// -----------------------------------------------------------------
// README.md
// -----------------------------------------------------------------

#[test]
fn readme_mentions_opencode_as_a_supported_engine() {
    // The README enumerates the engines bellows can dispatch to.
    // After AC16, opencode appears in that enumeration so an
    // operator scanning the README sees the full set.
    let body = read("README.md");
    assert!(
        body.to_lowercase().contains("opencode"),
        "README must mention opencode as a supported engine: {body}",
    );
}

// -----------------------------------------------------------------
// orchestrator.example.toml
// -----------------------------------------------------------------

#[test]
fn sample_config_mentions_opencode_engine() {
    let body = read("orchestrator.example.toml");
    assert!(
        body.to_lowercase().contains("opencode"),
        "orchestrator.example.toml must mention opencode: {body}",
    );
}

#[test]
fn sample_config_shows_opencode_env_file_path_key() {
    // opencode's auth shape is an env-file (not an OAuth credentials
    // volume), so the sample's `[auth.opencode]` block exposes
    // `env_file_path` rather than `credentials_volume` — the
    // dispatcher reads the env-file at container-create time (AC11).
    let body = read("orchestrator.example.toml");
    assert!(
        body.contains("[auth.opencode]"),
        "sample must declare [auth.opencode] block: {body}",
    );
    assert!(
        body.contains("env_file_path"),
        "sample must expose env_file_path for opencode auth: {body}",
    );
}

#[test]
fn sample_config_documents_opencode_deepseek_chain_entry() {
    // The chain-entry comment must show an opencode example so
    // operators discover the canonical `"opencode:deepseek/..."`
    // form (slash in the model side is opaque pass-through, AC1).
    let body = read("orchestrator.example.toml");
    assert!(
        body.contains("opencode:deepseek")
            || body.contains(r#""opencode""#),
        "sample must show an `opencode` chain-entry example: {body}",
    );
}

#[test]
fn sample_config_lists_engine_opencode_label_in_override_comment() {
    // The per-issue engine-override comment already mentions
    // `engine:claude` and `engine:codex`. AC16 adds `engine:opencode`
    // to that list so an operator labelling an issue discovers the
    // third label.
    let body = read("orchestrator.example.toml");
    assert!(
        body.contains("engine:opencode"),
        "sample must document the engine:opencode override label: {body}",
    );
}
