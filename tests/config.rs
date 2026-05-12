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
    // Issue #35 multi-repo polling: the legacy `[repo].url` form is
    // normalised into a one-element `repos` list. Both shapes feed
    // into `config.repos` so downstream callers don't branch on the
    // wire shape.
    assert_eq!(config.repos.len(), 1);
    assert_eq!(config.repos[0].url, "https://github.com/marad2001/bellows");
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
    // Issue #69 (ADR-0002) acceptance: `[auth].ssh_keys_volume` is a
    // new sibling field to `credentials_volume`, defaulting to
    // `"bellows-deploy-keys"`. A minimal config that omits the
    // section entirely must still produce the default so existing
    // operator orchestrator.toml files keep parsing.
    assert_eq!(config.auth.ssh_keys_volume, "bellows-deploy-keys");
}

#[test]
fn auth_section_ssh_keys_volume_can_be_overridden() {
    // Issue #69 (ADR-0002) acceptance: operators can rename the
    // deploy-keys volume in case the default collides with another
    // volume on their host, mirroring the existing
    // `credentials_volume` override hook.
    let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"

[auth]
ssh_keys_volume = "my-custom-keys"
"#;
    let config = Config::from_str(config_text).unwrap();
    assert_eq!(config.auth.ssh_keys_volume, "my-custom-keys");
    // The credentials volume default must still apply when only the
    // ssh-keys volume was overridden — the two fields are independent.
    assert_eq!(config.auth.credentials_volume, "bellows-claude-credentials");
}

#[test]
fn repo_deploy_keys_defaults_to_empty_vec_when_omitted() {
    // Issue #69 (ADR-0002) acceptance: per-repo `deploy_keys` is the
    // explicit opt-in for the SSH-deploy-keys mount. Omitting it (or
    // leaving the `[[repo]]` block in its existing shape) must continue
    // to parse and produce an empty Vec — preserving the "no creds in
    // sandbox by default" posture.
    let config = Config::from_str(MINIMAL_CONFIG).unwrap();
    assert_eq!(config.repos[0].deploy_keys, Vec::<String>::new());
}

#[test]
fn repo_deploy_keys_parses_non_empty_list_for_array_of_tables_form() {
    // Issue #69 (ADR-0002) acceptance: a `[[repo]]` block can declare
    // one or more deploy-key names that the mount filter will then
    // mount into the agent and cargo-checks containers spawned for
    // that repo. The list is preserved verbatim — the names are
    // resolved against the volume's filesystem at startup, not here.
    let config_text = r#"
[[repo]]
url = "https://github.com/marad2001/workboard-financial-advice"
deploy_keys = ["workboard-core", "workboard-shared"]

[[repo]]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"
"#;
    let config = Config::from_str(config_text).unwrap();
    assert_eq!(config.repos.len(), 2);
    assert_eq!(
        config.repos[0].deploy_keys,
        vec!["workboard-core".to_string(), "workboard-shared".to_string()],
    );
    assert!(
        config.repos[1].deploy_keys.is_empty(),
        "second repo must default to empty deploy_keys when omitted",
    );
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

// ---- Issue #35: multi-repo polling config shape ----

#[test]
fn parses_multi_repo_array_of_tables_form() {
    // Issue #35 acceptance criterion (a): a `[[repo]]` array-of-tables
    // config parses into `config.repos` with one element per `[[repo]]`
    // entry, in the order they appeared in the file. Order matters
    // because backward-compat callers (e.g. `bellows triage`) default
    // to the first configured repo.
    let config_text = r#"
[[repo]]
url = "https://github.com/marad2001/repo-a"

[[repo]]
url = "https://github.com/marad2001/repo-b"

[github]
pat_env_var = "GITHUB_TOKEN"
"#;
    let config = Config::from_str(config_text).expect("multi-repo config should parse");
    assert_eq!(
        config.repos.len(),
        2,
        "expected exactly two configured repos, got {:?}",
        config.repos.iter().map(|r| &r.url).collect::<Vec<_>>(),
    );
    assert_eq!(config.repos[0].url, "https://github.com/marad2001/repo-a");
    assert_eq!(config.repos[1].url, "https://github.com/marad2001/repo-b");
}

#[test]
fn existing_single_repo_table_form_continues_to_parse_as_one_element_list() {
    // Issue #35 acceptance criterion (b): the legacy `[repo]\nurl = ...`
    // form must continue to parse so existing operator
    // `orchestrator.toml` files keep working after the multi-repo slice
    // lands. The shape is normalised into a one-element `repos` list
    // so downstream code can iterate uniformly.
    let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows-test"

[github]
pat_env_var = "GITHUB_TOKEN"
"#;
    let config = Config::from_str(config_text).expect("legacy single-repo config must parse");
    assert_eq!(config.repos.len(), 1);
    assert_eq!(
        config.repos[0].url,
        "https://github.com/marad2001/bellows-test",
    );
}

#[test]
fn gates_section_defaults_apply_when_omitted() {
    // ADR-0004 acceptance: a config that omits the `[gates]` table
    // entirely must still parse, and the fallback flag strings must
    // preserve today's strict-default behaviour
    // (`--all-targets --all-features -- -D warnings` for clippy,
    // `--all-targets --all-features` for test). Existing operator
    // `orchestrator.toml` files therefore keep parsing with no edits.
    let config = Config::from_str(MINIMAL_CONFIG).unwrap();
    assert_eq!(
        config.gates.clippy_flags,
        "--all-targets --all-features -- -D warnings",
    );
    assert_eq!(config.gates.test_flags, "--all-targets --all-features");
}

#[test]
fn gates_section_clippy_flags_can_be_overridden() {
    // ADR-0004 acceptance: operators can declare a different fallback
    // posture for the cargo-checks gate clippy invocation, used
    // whenever bellows cannot parse the target repo's CI workflow.
    let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"

[gates]
clippy_flags = "--all-targets -- -D clippy::correctness -D clippy::suspicious"
"#;
    let config = Config::from_str(config_text).unwrap();
    assert_eq!(
        config.gates.clippy_flags,
        "--all-targets -- -D clippy::correctness -D clippy::suspicious",
    );
    // test_flags default must still apply when only clippy_flags
    // was overridden — the two fields are independent.
    assert_eq!(config.gates.test_flags, "--all-targets --all-features");
}

#[test]
fn gates_section_test_flags_can_be_overridden() {
    // ADR-0004 acceptance: operators can pin a different test
    // feature-flag posture for the cargo-checks gate fallback.
    let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"

[gates]
test_flags = "--features in-memory"
"#;
    let config = Config::from_str(config_text).unwrap();
    assert_eq!(config.gates.test_flags, "--features in-memory");
    assert_eq!(
        config.gates.clippy_flags,
        "--all-targets --all-features -- -D warnings",
    );
}

#[test]
fn empty_repo_list_rejected_at_parse_time() {
    // Issue #35 acceptance criterion (c): a config with no [[repo]]
    // entry at all must be rejected at parse time. An empty repo list
    // would silently produce an Idle polling loop forever, which is
    // never what the operator meant — clearly an error.
    //
    // We exercise this by omitting the `[repo]` / `[[repo]]` table
    // entirely; an empty array-of-tables (`[[repo]]` with no body) is
    // not expressible in TOML, but a *missing* table is.
    let config_text = r#"
[github]
pat_env_var = "GITHUB_TOKEN"
"#;
    let result = Config::from_str(config_text);
    assert!(
        result.is_err(),
        "config with no repos must be rejected at parse time, got {:?}",
        result.as_ref().map(|_| "Ok"),
    );
}
