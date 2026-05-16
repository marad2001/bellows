//! Acceptance criteria for the third-engine integration (issue #120 /
//! ADR-0008): OpenCode CLI driving the DeepSeek V4 Pro model.
//!
//! This file pins AC1 and AC2 — the config-layer surface for
//! `Engine::Opencode` and the generalised
//! `EngineLabelOverride::parse`. Other ACs live in sibling test files
//! (`engine_opencode_signatures.rs`, `engine_opencode_classify.rs`,
//! ...) so the per-AC test-first / make-it-pass commit ordering stays
//! visible in `git log`.

use bellows::config::{
    ChainEntry, Engine, EngineChainParseError, EngineLabelOverride, EngineLabelOverrideError,
};

// -----------------------------------------------------------------
// AC1: Engine::Opencode parses round-trip in chain entries.
// -----------------------------------------------------------------

#[test]
fn engine_enum_has_opencode_variant_and_round_trips_name() {
    let oc = Engine::Opencode;
    assert_eq!(oc.as_name(), "opencode");
    assert_eq!(Engine::from_name("opencode"), Some(Engine::Opencode));
    // Existing engines unaffected.
    assert_eq!(Engine::from_name("claude"), Some(Engine::Claude));
    assert_eq!(Engine::from_name("codex"), Some(Engine::Codex));
}

#[test]
fn chain_entry_parses_bare_opencode_string() {
    // `"opencode"` → ChainEntry { engine: Opencode, model: None }.
    let entry: ChainEntry = "opencode".parse().expect("bare opencode must parse");
    assert_eq!(entry.engine, Engine::Opencode);
    assert_eq!(entry.model, None);
}

#[test]
fn chain_entry_parses_opencode_with_provider_model_string_containing_slash() {
    // `"opencode:deepseek/deepseek-v4-pro"` → ChainEntry with the
    // full provider/model string preserved as opaque pass-through.
    // The slash inside the model side must NOT be treated specially —
    // ChainEntry splits on the FIRST `:`.
    let entry: ChainEntry = "opencode:deepseek/deepseek-v4-pro"
        .parse()
        .expect("opencode:provider/model must parse");
    assert_eq!(entry.engine, Engine::Opencode);
    assert_eq!(entry.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
}

#[test]
fn chain_entry_unknown_engine_error_message_mentions_opencode() {
    // The error message should mention opencode now that it's a known
    // engine, so an operator scanning the error sees the full set.
    let err = "wat:foo".parse::<ChainEntry>().unwrap_err();
    let msg = format!("{err}");
    assert!(
        matches!(err, EngineChainParseError::UnknownEngine(ref name) if name == "wat"),
        "expected UnknownEngine(\"wat\"), got {:?}",
        err,
    );
    assert!(
        msg.contains("opencode"),
        "error message must list opencode as a known engine: {msg}",
    );
}

// -----------------------------------------------------------------
// AC2: EngineLabelOverride::parse refuses any 2+ engine labels
// with an error message naming the conflicting labels.
// -----------------------------------------------------------------

#[test]
fn engine_label_override_two_engine_labels_error_names_both_labels() {
    let labels = vec!["engine:claude".to_string(), "engine:opencode".to_string()];
    let err = EngineLabelOverride::parse(&labels).expect_err("two labels must error");
    let msg = format!("{err}");
    assert!(
        msg.contains("engine:claude"),
        "error must name engine:claude in conflicting labels: {msg}",
    );
    assert!(
        msg.contains("engine:opencode"),
        "error must name engine:opencode in conflicting labels: {msg}",
    );
    // Matches the new "count > 1" shape generalisation.
    assert!(
        matches!(err, EngineLabelOverrideError::AmbiguousEngineLabels { .. }),
        "expected AmbiguousEngineLabels, got {err:?}",
    );
}

#[test]
fn engine_label_override_three_engine_labels_error_names_all_three() {
    let labels = vec![
        "engine:claude".to_string(),
        "engine:codex".to_string(),
        "engine:opencode".to_string(),
    ];
    let err = EngineLabelOverride::parse(&labels).expect_err("three labels must error");
    let msg = format!("{err}");
    assert!(msg.contains("engine:claude"), "error must name claude: {msg}");
    assert!(msg.contains("engine:codex"), "error must name codex: {msg}");
    assert!(
        msg.contains("engine:opencode"),
        "error must name opencode: {msg}"
    );
}

#[test]
fn engine_label_override_single_opencode_label_yields_opencode() {
    let labels = vec![
        "ready-for-agent".to_string(),
        "engine:opencode".to_string(),
    ];
    let override_ = EngineLabelOverride::parse(&labels).expect("parse");
    assert_eq!(override_, Some(Engine::Opencode));
}
