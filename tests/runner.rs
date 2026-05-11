//! Integration tests for `runner::run_once`'s pre-claim PR check (#42).
//!
//! These tests only exercise the pre-claim path: when the list-PRs check
//! is blocked or fails, `run_once` short-circuits before it would
//! otherwise touch the workspace, sandbox, or Docker. The "not blocked,
//! no ready-for-agent issues" case also exits cleanly via the existing
//! `RunOutcome::Idle` path without needing a real repo, so we can
//! drive the function entirely against a wiremock GitHub.

use std::io::Cursor;
use std::str::FromStr;

use bellows::config::Config;
use bellows::runner::{run_once, RunOutcome};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn octocrab_pointed_at(uri: String) -> octocrab::Octocrab {
    octocrab::OctocrabBuilder::new()
        .base_uri(uri)
        .expect("base uri")
        .build()
        .expect("octocrab")
}

fn config_for(mock_uri: &str) -> Config {
    // wiremock at e.g. http://127.0.0.1:54321 — we encode owner/repo as
    // path segments so `parse_owner_repo` sees `marad2001/test-repo`.
    let toml = format!(
        r#"
[repo]
url = "{mock_uri}/marad2001/test-repo"

[github]
pat_env_var = "BELLOWS_TEST_PAT"
"#
    );
    Config::from_str(&toml).expect("config parses")
}

#[tokio::test]
async fn run_once_returns_blocked_when_an_open_agent_pr_exists_and_skips_find_next_issue() {
    // Brief: "A polling tick that finds at least one open `agent/*` PR
    // returns `RunOutcome::Blocked` and does NOT call `find_next_issue`
    // or claim any issue." We deliberately mount NO mock for the issues
    // endpoint — if `run_once` called it, wiremock would 404 and the
    // run would surface a different RunOutcome (or error). The Blocked
    // outcome here is what proves the issues call was skipped.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/pulls"))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "number": 41, "head": { "ref": "agent/41-foo" }, "draft": false }
        ])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let config = config_for(&mock.uri());
    let mut log = Cursor::new(Vec::new());
    let outcome = run_once(&client, &config, &mut log, None)
        .await
        .expect("run_once should succeed");
    match outcome {
        RunOutcome::Blocked { pr_numbers } => {
            assert_eq!(pr_numbers, vec![41]);
        }
        other => panic!("expected Blocked, got {other:?}"),
    }
}

#[tokio::test]
async fn run_once_returns_blocked_when_draft_agent_pr_is_open() {
    // Brief: "Draft PRs on `agent/*` branches block exactly like
    // ready-for-review PRs". A draft agent PR is bellows's typical
    // stuck-after-CI-failure state — exactly the situation the pre-claim
    // check exists to catch.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/pulls"))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "number": 99, "head": { "ref": "agent/99-draft" }, "draft": true }
        ])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let config = config_for(&mock.uri());
    let mut log = Cursor::new(Vec::new());
    let outcome = run_once(&client, &config, &mut log, None)
        .await
        .expect("run_once should succeed");
    match outcome {
        RunOutcome::Blocked { pr_numbers } => assert_eq!(pr_numbers, vec![99]),
        other => panic!("expected Blocked {{ [99] }}, got {other:?}"),
    }
}

#[tokio::test]
async fn run_once_does_not_block_on_open_prs_with_non_agent_branches() {
    // Brief: "Manual / human-authored PRs on non-`agent/*` branches do
    // NOT block. The filter is strict on the `agent/` prefix." With only
    // a human-authored PR open and no ready-for-agent issues, the
    // pre-claim check passes and the tick falls through to the existing
    // Idle path.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/pulls"))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "number": 50, "head": { "ref": "fix-something-human" }, "draft": false }
        ])))
        .mount(&mock)
        .await;

    // No issues labelled ready-for-agent — pre-claim check passes, the
    // issues endpoint returns empty, and run_once should return Idle.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let config = config_for(&mock.uri());
    let mut log = Cursor::new(Vec::new());
    let outcome = run_once(&client, &config, &mut log, None)
        .await
        .expect("run_once should succeed");
    assert!(
        matches!(outcome, RunOutcome::Idle),
        "expected Idle (human PR must not block), got {outcome:?}",
    );
}

#[tokio::test]
async fn run_once_returns_blocked_fail_closed_when_list_prs_call_fails() {
    // Brief: "When the GitHub list-PRs API call fails for any reason,
    // bellows treats it as blocked (fail-closed). The next polling
    // tick retries." When we don't know whether master is gated, the
    // safe answer is to refuse to claim — the same answer we give
    // when we *know* master is gated.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/pulls"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let config = config_for(&mock.uri());
    let mut log = Cursor::new(Vec::new());
    let outcome = run_once(&client, &config, &mut log, None)
        .await
        .expect("run_once should succeed (errors map to Blocked, not Err)");
    // pr_numbers is empty because we couldn't list them.
    match outcome {
        RunOutcome::Blocked { pr_numbers } => assert!(
            pr_numbers.is_empty(),
            "fail-closed Blocked has empty pr_numbers, got {pr_numbers:?}",
        ),
        other => panic!("expected Blocked (fail-closed), got {other:?}"),
    }
}
