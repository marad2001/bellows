//! Multi-engine type plumbing for issue #81 / ADR-0005.
//!
//! Each test below pins one acceptance criterion from issue #81's
//! brief to a checkable assertion against the `Engine` /
//! `ChainEntry` / per-phase config types. Failing-test commits land
//! first; the make-it-pass commits follow.

use std::str::FromStr;

use bellows::config::{ChainEntry, Config, Engine, EngineChainParseError, EngineLabelOverride};

// -----------------------------------------------------------------
// AC1 — `Engine` enum carries Claude + Codex.
// -----------------------------------------------------------------

#[test]
fn engine_enum_has_claude_and_codex_variants() {
    let claude = Engine::Claude;
    let codex = Engine::Codex;
    assert_ne!(claude, codex);
    // Round-trippable name string is the load-bearing surface — the
    // runner uses it for `BELLOWS_ENGINE=<name>`, the chain-parser
    // uses it for matching the leading `<engine>:<model>` token, and
    // the label parser uses it for matching `engine:<name>` labels.
    assert_eq!(claude.as_name(), "claude");
    assert_eq!(codex.as_name(), "codex");
}

#[test]
fn engine_from_name_round_trips_claude_and_codex() {
    assert_eq!(Engine::from_name("claude"), Some(Engine::Claude));
    assert_eq!(Engine::from_name("codex"), Some(Engine::Codex));
    // Anything else is `None` — the chain-parser turns this into a
    // config-load error; the label parser turns it into "ignore the
    // label, it's not one of ours."
    assert_eq!(Engine::from_name("gpt-5"), None);
    assert_eq!(Engine::from_name(""), None);
    // Case-sensitive — the operator's config and labels are
    // lower-case by convention; surfacing a typo as an error rather
    // than silently matching keeps the failure mode operator-legible.
    assert_eq!(Engine::from_name("Claude"), None);
}

// -----------------------------------------------------------------
// AC2 — `ChainEntry { engine, model: Option<String> }` parses
// from the flat TOML string form.
// -----------------------------------------------------------------

#[test]
fn chain_entry_parses_bare_engine_string_as_no_model_pin() {
    // `"claude"` → `ChainEntry { engine: Claude, model: None }`.
    // The CLI's default-model behaviour applies when `model` is
    // `None`; the runner omits the `-m` flag in this case.
    let entry: ChainEntry = "claude".parse().expect("bare engine must parse");
    assert_eq!(entry.engine, Engine::Claude);
    assert_eq!(entry.model, None);
}

#[test]
fn chain_entry_parses_engine_with_model_pin() {
    // `"codex:gpt-5.5"` → `ChainEntry { engine: Codex,
    // model: Some("gpt-5.5".to_string()) }`. The model string is
    // pass-through — bellows doesn't validate against an allow-list.
    let entry: ChainEntry =
        "codex:gpt-5.5".parse().expect("engine:model must parse");
    assert_eq!(entry.engine, Engine::Codex);
    assert_eq!(entry.model.as_deref(), Some("gpt-5.5"));
}

#[test]
fn chain_entry_splits_only_on_first_colon() {
    // Model strings may themselves contain `:` (e.g. a future
    // organisation-prefixed model name). The brief mandates split-
    // on-first-`:` so the model side stays opaque pass-through.
    let entry: ChainEntry =
        "claude:opus-4-7:beta".parse().expect("split-on-first colon");
    assert_eq!(entry.engine, Engine::Claude);
    assert_eq!(entry.model.as_deref(), Some("opus-4-7:beta"));
}

#[test]
fn chain_entry_rejects_unknown_engine_string() {
    // Unknown engine name at config-load time is a hard error — the
    // brief specifies "Engine name validated against the Engine enum
    // at config-load time (unknown engine rejected)."
    let err = "gpt:gpt-5".parse::<ChainEntry>().unwrap_err();
    assert!(
        matches!(err, EngineChainParseError::UnknownEngine(ref name) if name == "gpt"),
        "expected UnknownEngine(\"gpt\"), got {:?}", err,
    );
}

#[test]
fn chain_entry_accepts_unknown_model_as_opaque_pass_through() {
    // Brief: "Model is opaque pass-through string — no allow-list,
    // since available models depend on subscription tier and shift
    // over time; CLI reports unknown-model at run time." So
    // `claude:wat-9000` parses fine; the CLI will reject it later.
    let entry: ChainEntry =
        "claude:wat-9000".parse().expect("opaque model");
    assert_eq!(entry.engine, Engine::Claude);
    assert_eq!(entry.model.as_deref(), Some("wat-9000"));
}

#[test]
fn chain_entry_rejects_empty_string() {
    // An empty entry is a parser error rather than silently
    // defaulting to anything — the brief calls out "empty chain
    // rejected at config-load," and an empty entry would be the
    // mechanism that produced an empty chain.
    let err = "".parse::<ChainEntry>().unwrap_err();
    assert!(
        matches!(err, EngineChainParseError::Empty),
        "expected Empty, got {:?}", err,
    );
}

// -----------------------------------------------------------------
// AC3 — per-phase `cli_chain` config.
// -----------------------------------------------------------------

const MULTI_PHASE_CONFIG: &str = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"

[phases.implement]
cli_chain = ["claude:opus-4-7", "codex:gpt-5.5"]

[phases.review]
cli_chain = ["codex:gpt-5.5", "claude:sonnet-4-6"]

[phases.review_fix]
cli_chain = ["claude"]

[phases.security_review]
cli_chain = ["codex"]

[phases.security_fix]
cli_chain = ["claude", "codex"]
"#;

#[test]
fn config_parses_per_phase_cli_chain_with_mixed_pinned_models() {
    let config = Config::from_str(MULTI_PHASE_CONFIG).expect("multi-phase config must parse");
    let implement = &config.phases.implement.cli_chain;
    assert_eq!(implement.len(), 2);
    assert_eq!(implement[0].engine, Engine::Claude);
    assert_eq!(implement[0].model.as_deref(), Some("opus-4-7"));
    assert_eq!(implement[1].engine, Engine::Codex);
    assert_eq!(implement[1].model.as_deref(), Some("gpt-5.5"));

    let review = &config.phases.review.cli_chain;
    assert_eq!(review.len(), 2);
    assert_eq!(review[0].engine, Engine::Codex);
    assert_eq!(review[0].model.as_deref(), Some("gpt-5.5"));
    assert_eq!(review[1].engine, Engine::Claude);
    assert_eq!(review[1].model.as_deref(), Some("sonnet-4-6"));

    let review_fix = &config.phases.review_fix.cli_chain;
    assert_eq!(review_fix.len(), 1);
    assert_eq!(review_fix[0].engine, Engine::Claude);
    assert_eq!(review_fix[0].model, None);

    let security_review = &config.phases.security_review.cli_chain;
    assert_eq!(security_review.len(), 1);
    assert_eq!(security_review[0].engine, Engine::Codex);
    assert_eq!(security_review[0].model, None);

    let security_fix = &config.phases.security_fix.cli_chain;
    assert_eq!(security_fix.len(), 2);
    assert_eq!(security_fix[0].engine, Engine::Claude);
    assert_eq!(security_fix[1].engine, Engine::Codex);
}

#[test]
fn config_phases_default_to_single_claude_entry_when_section_omitted() {
    // Brief: "Default-default for each phase = ["claude"]." A
    // minimal config (with no `[phases.X]` tables) must produce
    // `cli_chain = [ChainEntry::claude]` for every phase. This is
    // the v1 single-engine behaviour the backwards-compat story
    // depends on.
    let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"
"#;
    let config = Config::from_str(config_text).unwrap();
    for chain in [
        &config.phases.implement.cli_chain,
        &config.phases.review.cli_chain,
        &config.phases.review_fix.cli_chain,
        &config.phases.security_review.cli_chain,
        &config.phases.security_fix.cli_chain,
    ] {
        assert_eq!(chain.len(), 1, "default chain must be one entry");
        assert_eq!(chain[0].engine, Engine::Claude);
        assert_eq!(chain[0].model, None);
    }
}

#[test]
fn config_rejects_empty_cli_chain() {
    let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"

[phases.implement]
cli_chain = []
"#;
    let result = Config::from_str(config_text);
    assert!(
        result.is_err(),
        "empty cli_chain must be rejected at config-load",
    );
}

#[test]
fn config_rejects_cli_chain_with_unknown_engine() {
    let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"

[phases.implement]
cli_chain = ["claude", "gpt:gpt-5"]
"#;
    let result = Config::from_str(config_text);
    assert!(
        result.is_err(),
        "unknown engine in cli_chain must be rejected at config-load",
    );
}

// -----------------------------------------------------------------
// AC4 — per-engine auth + flat-key rewrite.
// -----------------------------------------------------------------

#[test]
fn auth_per_engine_credentials_volumes_parse() {
    let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"

[auth.claude]
credentials_volume = "my-claude-creds"

[auth.codex]
credentials_volume = "my-codex-creds"
"#;
    let config = Config::from_str(config_text).unwrap();
    assert_eq!(config.auth.claude.credentials_volume, "my-claude-creds");
    assert_eq!(config.auth.codex.credentials_volume, "my-codex-creds");
}

#[test]
fn flat_auth_credentials_volume_rewrites_to_claude_for_backwards_compat() {
    // Brief: "The previous flat key `auth.credentials_volume`
    // continues to work — bellows rewrites it to
    // `auth.claude.credentials_volume` at config-load time for
    // backwards compatibility."
    let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"

[auth]
credentials_volume = "legacy-volume-name"
"#;
    let config = Config::from_str(config_text).unwrap();
    assert_eq!(config.auth.claude.credentials_volume, "legacy-volume-name");
    // The codex volume falls back to the default name — an operator
    // who never touches codex sees no change.
    assert_eq!(config.auth.codex.credentials_volume, "bellows-codex-credentials");
}

#[test]
fn auth_defaults_apply_when_section_omitted() {
    let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"
"#;
    let config = Config::from_str(config_text).unwrap();
    assert_eq!(config.auth.claude.credentials_volume, "bellows-claude-credentials");
    assert_eq!(config.auth.codex.credentials_volume, "bellows-codex-credentials");
}

// -----------------------------------------------------------------
// AC5 — label parsing for `engine:claude` / `engine:codex`.
// -----------------------------------------------------------------

#[test]
fn engine_label_override_single_claude_label_yields_claude() {
    let labels = vec!["ready-for-agent".to_string(), "engine:claude".to_string()];
    let override_ = EngineLabelOverride::parse(&labels).expect("parse");
    assert_eq!(override_, Some(Engine::Claude));
}

#[test]
fn engine_label_override_single_codex_label_yields_codex() {
    let labels = vec!["ready-for-agent".to_string(), "engine:codex".to_string()];
    let override_ = EngineLabelOverride::parse(&labels).expect("parse");
    assert_eq!(override_, Some(Engine::Codex));
}

#[test]
fn engine_label_override_no_engine_label_yields_none() {
    let labels = vec!["ready-for-agent".to_string()];
    let override_ = EngineLabelOverride::parse(&labels).expect("parse");
    assert_eq!(override_, None);
}

#[test]
fn engine_label_override_both_engine_labels_is_ambiguous_error() {
    // Brief: "Both `engine:claude` and `engine:codex` on the same
    // issue → refuse-to-claim, parallel to `MissingAgentBrief`."
    let labels = vec![
        "ready-for-agent".to_string(),
        "engine:claude".to_string(),
        "engine:codex".to_string(),
    ];
    let result = EngineLabelOverride::parse(&labels);
    assert!(
        result.is_err(),
        "both engine labels must produce a refusal: got {:?}",
        result,
    );
}
