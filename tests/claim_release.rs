//! Issue #193: a run that aborts after claiming an issue and BEFORE a PR
//! exists returns that issue to the configured pickup label, so the next
//! polling tick re-claims it through the normal path instead of leaving it
//! stranded at the in-progress label until a process restart.
//!
//! The tests drive the two steps the polling loop's error arm performs —
//! `run_once` (which records the in-flight claim) and
//! `release_claim_after_run_error` (which returns it) — against a wiremock
//! GitHub. The post-claim abort is produced naturally: the configured repo
//! URL points at the mock server, so the workspace clone that follows the
//! claim fails, which is exactly the shape of the sandbox-layer aborts that
//! stranded seven issues in the 2026-07-25 -> 2026-07-28 window.

use std::io::Cursor;
use std::str::FromStr;

use bellows::config::Config;
use bellows::runner::{
    release_claim_after_run_error, run_once, InFlightClaim, InFlightClaimSlot, RunError,
};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Deliberately non-default label strings: every assertion below is on the
/// operator's configured names, so an implementation that hardcoded
/// `ready-for-agent` / `agent-in-progress` fails these tests (brief AC6).
const PICKUP: &str = "queue-me";
const IN_PROGRESS: &str = "agent-running";
/// An unrelated label that must survive the release untouched.
const KEEP: &str = "keep-me";

fn octocrab_pointed_at(uri: String) -> octocrab::Octocrab {
    octocrab::OctocrabBuilder::new()
        .base_uri(uri)
        .expect("base uri")
        .build()
        .expect("octocrab")
}

fn config_for(mock_uri: &str) -> Config {
    let toml = format!(
        r#"
[repo]
url = "{mock_uri}/marad2001/test-repo"

[github]
pat_env_var = "BELLOWS_TEST_PAT"

[polling]
pickup_label = "{PICKUP}"

[runtime_labels]
agent_in_progress = "{IN_PROGRESS}"
"#
    );
    Config::from_str(&toml).expect("config parses")
}

/// Mount everything `run_once` needs to select, brief-check and CLAIM
/// issue `number`, and nothing beyond it. The clone that follows the claim
/// targets the mock server (not a git remote) and therefore fails, so the
/// tick returns `Err` with the issue sitting at the in-progress label —
/// the exact pre-PR abort this issue is about.
/// `labels` is the issue's label set as GitHub currently holds it —
/// parameterised so a test can feed back the exact set a previous release
/// left behind.
async fn mock_claim_then_abort(mock: &MockServer, number: u64, labels: &[String]) {
    let current: Vec<serde_json::Value> = labels.iter().map(|l| json!({ "name": l })).collect();
    // Pre-claim PR-open gate: clear.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(mock)
        .await;

    // The polling query selects on the configured pickup label.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .and(query_param("labels", PICKUP))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": number,
                "title": "aborts before opening a PR",
                "created_at": "2026-07-28T09:00:00Z",
                "labels": current
            }
        ])))
        .mount(mock)
        .await;

    // Agent brief present, so the candidate is claimable.
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/marad2001/test-repo/issues/{number}/comments"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "body": "## Agent Brief\n\nDo the thing." }
        ])))
        .mount(mock)
        .await;

    // Pre-claim stale-branch sweep: nothing to delete.
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/marad2001/test-repo/git/matching-refs/heads/agent/{number}-"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(mock)
        .await;

    // `claim` GETs the issue first — served ONCE, so the release's own GET
    // below sees the post-claim label set instead.
    Mock::given(method("GET"))
        .and(path(format!("/repos/marad2001/test-repo/issues/{number}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": number,
            "title": "aborts before opening a PR",
            "labels": current
        })))
        .up_to_n_times(1)
        .mount(mock)
        .await;

    // ...then PATCHes pickup -> in-progress.
    Mock::given(method("PATCH"))
        .and(path(format!("/repos/marad2001/test-repo/issues/{number}")))
        .and(body_string_contains(IN_PROGRESS))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": number,
            "title": "aborts before opening a PR",
            "labels": [{ "name": IN_PROGRESS }, { "name": KEEP }]
        })))
        .mount(mock)
        .await;

    // The claimed issue as the release step will find it.
    Mock::given(method("GET"))
        .and(path(format!("/repos/marad2001/test-repo/issues/{number}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": number,
            "title": "aborts before opening a PR",
            "labels": [{ "name": IN_PROGRESS }, { "name": KEEP }]
        })))
        .mount(mock)
        .await;
}

/// Mount the release's PATCH — the one that swaps the in-progress label
/// back to the pickup label. Distinguished from the claim's PATCH by its
/// body: the claim sends the in-progress label, the release sends the
/// pickup one.
async fn mock_release_patch(mock: &MockServer, number: u64, response: ResponseTemplate) {
    Mock::given(method("PATCH"))
        .and(path(format!("/repos/marad2001/test-repo/issues/{number}")))
        .and(body_string_contains(PICKUP))
        .respond_with(response)
        .expect(1)
        .mount(mock)
        .await;
}

fn released_issue_body(number: u64) -> serde_json::Value {
    json!({
        "number": number,
        "title": "aborts before opening a PR",
        "labels": [{ "name": PICKUP }, { "name": KEEP }]
    })
}

/// The label set of the last PATCH the mock server received, in the order
/// bellows sent it.
async fn last_patched_labels(mock: &MockServer) -> Vec<String> {
    let requests = mock
        .received_requests()
        .await
        .expect("request recording enabled");
    let body = requests
        .iter()
        .rfind(|r| r.method == wiremock::http::Method::PATCH)
        .expect("at least one PATCH");
    let json: serde_json::Value = serde_json::from_slice(&body.body).expect("PATCH body is JSON");
    json["labels"]
        .as_array()
        .expect("labels array")
        .iter()
        .map(|v| v.as_str().expect("label string").to_string())
        .collect()
}

#[tokio::test]
async fn run_error_before_pr_returns_the_claimed_issue_to_the_pickup_label() {
    // Brief AC1: a run that errors after claiming and before a PR exists
    // leaves its issue carrying the configured pickup label and NOT the
    // in-progress label.
    let mock = MockServer::start().await;
    mock_claim_then_abort(&mock, 42, &[PICKUP.to_string(), KEEP.to_string()]).await;
    mock_release_patch(
        &mock,
        42,
        ResponseTemplate::new(200).set_body_json(released_issue_body(42)),
    )
    .await;

    let config = config_for(&mock.uri());
    let client = octocrab_pointed_at(mock.uri());
    let slot = InFlightClaimSlot::new();
    let mut log = Cursor::new(Vec::new());

    let err = run_once(&client, &config, &mut log, None, None, Some(&slot))
        .await
        .expect_err("the post-claim workspace clone must fail against wiremock");
    assert!(
        matches!(err, RunError::Workspace(_)),
        "expected the abort to come from the post-claim clone, got {err:?}",
    );

    // The polling loop's error arm: release what was claimed but never
    // reached a PR.
    release_claim_after_run_error(
        &client,
        &slot,
        &config.runtime_labels.agent_in_progress,
        &config.polling.pickup_label,
        &err.to_string(),
        &mut log,
    )
    .await;

    let labels = last_patched_labels(&mock).await;
    assert!(
        labels.iter().any(|l| l == PICKUP),
        "released issue must carry the configured pickup label; PATCHed {labels:?}",
    );
    assert!(
        !labels.iter().any(|l| l == IN_PROGRESS),
        "released issue must no longer carry the in-progress label; PATCHed {labels:?}",
    );
    assert!(
        labels.iter().any(|l| l == KEEP),
        "unrelated labels must survive the release; PATCHed {labels:?}",
    );

    // AC5: the run log names the released issue and the reason, and reads
    // differently from the startup-reconcile line so the two recovery
    // routes can be told apart.
    let log_str = String::from_utf8(log.into_inner()).expect("utf-8 log");
    assert!(
        log_str.contains("run-abort release: returned claimed issue #42 (marad2001/test-repo)"),
        "log must name the released issue: {log_str}",
    );
    assert!(
        log_str.contains(&err.to_string()),
        "log must name the reason the issue was released: {log_str}",
    );
    assert!(
        !log_str.contains("startup reconcile"),
        "the release line must not read as a startup-reconcile line: {log_str}",
    );
}

#[tokio::test]
async fn run_error_after_the_pr_exists_releases_nothing() {
    // Brief AC3: a run that errors AFTER its PR exists is not released.
    // That path already reaches finalise and applies the right outcome
    // label; returning it to the pickup queue would put finished work back
    // in front of the next tick.
    //
    // The seam is the slot: `run_once` empties it the instant `open_pr`
    // returns, so "the PR exists" IS "the slot is empty". This test
    // reproduces that transition and asserts the release makes no GitHub
    // call whatsoever — the mock server has nothing mounted, so any
    // request at all would show up in `received_requests`.
    let mock = MockServer::start().await;
    let client = octocrab_pointed_at(mock.uri());
    let mut log = Cursor::new(Vec::new());

    let slot = InFlightClaimSlot::new();
    slot.set(InFlightClaim {
        owner: "marad2001".to_string(),
        repo: "test-repo".to_string(),
        issue_number: 42,
    });
    // ...the PR is opened, and the claim stops being releasable.
    slot.clear();

    release_claim_after_run_error(
        &client,
        &slot,
        IN_PROGRESS,
        PICKUP,
        "post-PR failure in finalise",
        &mut log,
    )
    .await;

    let requests = mock
        .received_requests()
        .await
        .expect("request recording enabled");
    assert!(
        requests.is_empty(),
        "a post-PR failure must not touch the issue's labels; sent {:?}",
        requests
            .iter()
            .map(|r| format!("{} {}", r.method, r.url.path()))
            .collect::<Vec<_>>(),
    );
    let log_str = String::from_utf8(log.into_inner()).expect("utf-8 log");
    assert!(
        log_str.is_empty(),
        "nothing was released, so nothing should be logged: {log_str}",
    );
}

#[tokio::test]
async fn a_failing_release_leaves_the_original_run_error_intact() {
    // Brief AC4: the release is best-effort and subordinate to the run
    // error. When the label PATCH fails, the run error is still what
    // surfaces, the release failure is logged beside it, and the issue is
    // left for the startup sweep exactly as before.
    let mock = MockServer::start().await;
    mock_claim_then_abort(&mock, 42, &[PICKUP.to_string(), KEEP.to_string()]).await;
    // Mounted inline rather than via `mock_release_patch`: octocrab retries
    // a 5xx, so the request count here is the client's business, not this
    // test's.
    Mock::given(method("PATCH"))
        .and(path("/repos/marad2001/test-repo/issues/42"))
        .and(body_string_contains(PICKUP))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock)
        .await;

    let config = config_for(&mock.uri());
    let client = octocrab_pointed_at(mock.uri());
    let slot = InFlightClaimSlot::new();
    let mut log = Cursor::new(Vec::new());

    let err = run_once(&client, &config, &mut log, None, None, Some(&slot))
        .await
        .expect_err("the post-claim workspace clone must fail against wiremock");
    let before = format!("{err:?}");

    // `release_claim_after_run_error` returns `()`: there is no channel by
    // which a release failure could replace the run error, and the caller
    // does not have to decide between them.
    release_claim_after_run_error(
        &client,
        &slot,
        &config.runtime_labels.agent_in_progress,
        &config.polling.pickup_label,
        &err.to_string(),
        &mut log,
    )
    .await;

    assert_eq!(
        before,
        format!("{err:?}"),
        "the run error must survive the failed release unchanged",
    );
    assert!(
        matches!(err, RunError::Workspace(_)),
        "the surfaced error must still be the run's own, not the release's: {err:?}",
    );
    let log_str = String::from_utf8(log.into_inner()).expect("utf-8 log");
    assert!(
        log_str.contains("run-abort release: could not return claimed issue #42"),
        "the release failure must be logged beside the run error: {log_str}",
    );
}

#[tokio::test]
async fn a_released_issue_is_claimed_by_the_very_next_polling_tick() {
    // Brief AC2: the released issue is claimable on the very next polling
    // tick, with no operator action and no restart. The second tick below
    // is fed the EXACT label set the release PATCHed on the first, so the
    // two halves are genuinely connected rather than independently
    // hand-stubbed.
    let first = MockServer::start().await;
    mock_claim_then_abort(&first, 42, &[PICKUP.to_string(), KEEP.to_string()]).await;
    mock_release_patch(
        &first,
        42,
        ResponseTemplate::new(200).set_body_json(released_issue_body(42)),
    )
    .await;

    let config = config_for(&first.uri());
    let client = octocrab_pointed_at(first.uri());
    let slot = InFlightClaimSlot::new();
    let mut log = Cursor::new(Vec::new());

    let err = run_once(&client, &config, &mut log, None, None, Some(&slot))
        .await
        .expect_err("the post-claim workspace clone must fail against wiremock");
    release_claim_after_run_error(
        &client,
        &slot,
        &config.runtime_labels.agent_in_progress,
        &config.polling.pickup_label,
        &err.to_string(),
        &mut log,
    )
    .await;
    let released_labels = last_patched_labels(&first).await;

    // Next tick, against a GitHub holding the issue exactly as the release
    // left it. The claim PATCH is `expect(1)`: if the released label set
    // did not satisfy the polling query, the issue would never be selected
    // and the expectation would fail when the server drops.
    let next = MockServer::start().await;
    mock_claim_then_abort(&next, 42, &released_labels).await;

    let next_config = config_for(&next.uri());
    let next_client = octocrab_pointed_at(next.uri());
    let next_slot = InFlightClaimSlot::new();
    let mut next_log = Cursor::new(Vec::new());
    let _ = run_once(
        &next_client,
        &next_config,
        &mut next_log,
        None,
        None,
        Some(&next_slot),
    )
    .await;

    let next_log_str = String::from_utf8(next_log.into_inner()).expect("utf-8 log");
    assert!(
        next_log_str.contains("claimed issue #42"),
        "the released issue must be claimed by the next tick: {next_log_str}",
    );
    let claim_patch = last_patched_labels(&next).await;
    assert!(
        claim_patch.iter().any(|l| l == IN_PROGRESS),
        "the next tick's claim must move the released issue to the in-progress label; PATCHed {claim_patch:?}",
    );
}
