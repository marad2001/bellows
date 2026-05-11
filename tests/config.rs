use std::str::FromStr;

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

#[test]
fn agent_section_defaults_apply_when_omitted() {
    // Slice 6: the per-issue wall-clock budget defaults to 60 minutes
    // when [agent].wall_clock_minutes is unspecified. Slice 8: the
    // weak-test-guard skip label defaults to "refactor" when
    // [agent].weak_test_guard_skip_label is unspecified.
    let config = Config::from_str(MINIMAL_CONFIG).unwrap();
    assert_eq!(config.agent.wall_clock_minutes.get(), 60);
    assert_eq!(config.agent.weak_test_guard_skip_label, "refactor");
}

#[test]
fn agent_section_weak_test_guard_skip_label_can_be_overridden() {
    // Slice 8 acceptance criterion: the runtime config exposes a
    // configurable skip-label string under `[agent]`. An operator
    // running e.g. a documentation-heavy fork can rename the label
    // without code changes.
    let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"

[agent]
weak_test_guard_skip_label = "no-tests-needed"
"#;
    let config = Config::from_str(config_text).unwrap();
    assert_eq!(config.agent.weak_test_guard_skip_label, "no-tests-needed");
    // Wall-clock default still applies when only the skip-label was
    // overridden — the two fields are independent.
    assert_eq!(config.agent.wall_clock_minutes.get(), 60);
}

#[test]
fn agent_section_wall_clock_minutes_can_be_overridden() {
    let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"

[agent]
wall_clock_minutes = 5
"#;
    let config = Config::from_str(config_text).unwrap();
    assert_eq!(config.agent.wall_clock_minutes.get(), 5);
}

#[test]
fn agent_section_wall_clock_minutes_rejects_zero() {
    // Defending against a misconfigured `wall_clock_minutes = 0`. With
    // a plain u64 the budget would be marked exceeded on the first
    // call and the runner would pass `None` as the deadline to the
    // implement phase — which `run_container` reads as "no deadline" —
    // bypassing the cap entirely. NonZeroU64 makes `0` a parse-time
    // error.
    let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"

[agent]
wall_clock_minutes = 0
"#;
    let result = Config::from_str(config_text);
    assert!(
        result.is_err(),
        "expected wall_clock_minutes = 0 to fail parsing, got {:?}",
        result.as_ref().map(|_| "Ok"),
    );
}
