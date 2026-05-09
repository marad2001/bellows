use bellows::config::{AuthMethod, Config};

const MINIMAL_CONFIG: &str = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"
"#;

#[test]
fn parses_minimal_orchestrator_toml() {
    let config = Config::from_str(MINIMAL_CONFIG).expect("minimal config should parse");
    assert_eq!(config.repo.url, "https://github.com/marad2001/bellows");
    assert_eq!(config.github.pat_env_var, "GITHUB_TOKEN");
}

#[test]
fn polling_section_defaults_apply_when_omitted() {
    let config = Config::from_str(MINIMAL_CONFIG).unwrap();
    assert_eq!(config.polling.interval_seconds, 45);
    assert_eq!(config.polling.pickup_label, "ready-for-agent");
}

#[test]
fn runtime_labels_section_defaults_apply_when_omitted() {
    let config = Config::from_str(MINIMAL_CONFIG).unwrap();
    assert_eq!(config.runtime_labels.agent_in_progress, "agent-in-progress");
    assert_eq!(config.runtime_labels.agent_done, "agent-done");
    assert_eq!(config.runtime_labels.agent_failed, "agent-failed");
    assert_eq!(config.runtime_labels.agent_rate_limited, "agent-rate-limited");
    assert_eq!(config.runtime_labels.agent_cancelled, "agent-cancelled");
}

#[test]
fn logging_section_defaults_apply_when_omitted() {
    let config = Config::from_str(MINIMAL_CONFIG).unwrap();
    assert_eq!(config.logging.path, std::path::PathBuf::from("bellows.log"));
}

#[test]
fn auth_section_defaults_apply_when_omitted() {
    let config = Config::from_str(MINIMAL_CONFIG).unwrap();
    assert!(matches!(config.auth.method, AuthMethod::Subscription));
    assert_eq!(config.auth.credentials_volume, "bellows-claude-credentials");
}
