use serde_json::json;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use bellows::tracker::{claim, finalise_success, find_next_issue, ClaimError};

fn octocrab_pointed_at(uri: String) -> octocrab::Octocrab {
    octocrab::OctocrabBuilder::new()
        .base_uri(uri)
        .expect("base uri")
        .build()
        .expect("octocrab")
}

#[tokio::test]
async fn find_next_issue_returns_an_issue_when_one_carries_the_pickup_label() {
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "number": 42, "title": "Fix the foo bug", "labels": [{ "name": "ready-for-agent" }] }
        ])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());

    let result = find_next_issue(
        &client,
        "marad2001",
        "test-repo",
        "ready-for-agent",
        "agent-in-progress",
    )
    .await
    .expect("call should succeed");

    let issue = result.expect("expected an issue");
    assert_eq!(issue.number, 42);
    assert_eq!(issue.title, "Fix the foo bug");
}

#[tokio::test]
async fn find_next_issue_skips_issues_already_carrying_the_in_progress_label() {
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 41,
                "title": "Already claimed",
                "labels": [
                    { "name": "ready-for-agent" },
                    { "name": "agent-in-progress" }
                ]
            },
            {
                "number": 42,
                "title": "Available",
                "labels": [{ "name": "ready-for-agent" }]
            }
        ])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());

    let result = find_next_issue(
        &client,
        "marad2001",
        "test-repo",
        "ready-for-agent",
        "agent-in-progress",
    )
    .await
    .expect("call should succeed");

    let issue = result.expect("expected an issue");
    assert_eq!(issue.number, 42, "should return the un-claimed issue, not the in-progress one");
}

#[tokio::test]
async fn claim_swaps_pickup_label_for_in_progress_preserving_other_labels() {
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 42,
            "title": "Fix the foo bug",
            "labels": [
                { "name": "ready-for-agent" },
                { "name": "enhancement" }
            ]
        })))
        .mount(&mock)
        .await;

    let expected_body = json!({ "labels": ["agent-in-progress", "enhancement"] });
    Mock::given(method("PATCH"))
        .and(path("/repos/marad2001/test-repo/issues/42"))
        .and(body_json(expected_body))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 42,
            "title": "Fix the foo bug",
            "labels": [
                { "name": "agent-in-progress" },
                { "name": "enhancement" }
            ]
        })))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let claimed = claim(
        &client,
        "marad2001",
        "test-repo",
        42,
        "ready-for-agent",
        "agent-in-progress",
    )
    .await
    .expect("claim should succeed");

    let label_names: Vec<&str> = claimed.labels.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(claimed.number, 42);
    assert!(label_names.contains(&"agent-in-progress"));
    assert!(label_names.contains(&"enhancement"));
    assert!(!label_names.contains(&"ready-for-agent"));
}

#[tokio::test]
async fn claim_returns_contended_when_pickup_label_already_swapped() {
    let mock = MockServer::start().await;

    // Simulate a concurrent orchestrator having already won the race:
    // by the time we GET, the pickup label has already been swapped for in-progress.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 42,
            "title": "Fix the foo bug",
            "labels": [{ "name": "agent-in-progress" }]
        })))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let result = claim(
        &client,
        "marad2001",
        "test-repo",
        42,
        "ready-for-agent",
        "agent-in-progress",
    )
    .await;

    assert!(
        matches!(result, Err(ClaimError::Contended)),
        "expected Contended, got {:?}",
        result.as_ref().map(|_| "Ok"),
    );
}

#[tokio::test]
async fn finalise_success_posts_log_comment_and_transitions_to_done() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/repos/marad2001/test-repo/issues/99/comments"))
        .and(body_json(json!({ "body": "Run completed" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 42,
            "title": "Fix the foo bug",
            "labels": [
                { "name": "agent-in-progress" },
                { "name": "enhancement" }
            ]
        })))
        .mount(&mock)
        .await;

    Mock::given(method("PATCH"))
        .and(path("/repos/marad2001/test-repo/issues/42"))
        .and(body_json(json!({ "labels": ["agent-done", "enhancement"] })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 42,
            "title": "Fix the foo bug",
            "labels": [
                { "name": "agent-done" },
                { "name": "enhancement" }
            ]
        })))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let updated = finalise_success(
        &client,
        "marad2001",
        "test-repo",
        42,
        99,
        "agent-in-progress",
        "agent-done",
        "Run completed",
    )
    .await
    .expect("finalise should succeed");

    let label_names: Vec<&str> = updated.labels.iter().map(|l| l.name.as_str()).collect();
    assert!(label_names.contains(&"agent-done"));
    assert!(label_names.contains(&"enhancement"));
    assert!(!label_names.contains(&"agent-in-progress"));
}
