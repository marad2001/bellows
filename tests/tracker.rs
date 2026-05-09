use serde_json::json;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use bellows::tracker::{
    claim, fetch_agent_brief, finalise, find_next_issue, post_pr_comment, ClaimError,
    FinaliseRequest,
};

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
async fn finalise_posts_log_comment_and_transitions_to_outcome_label() {
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
    let updated = finalise(
        &client,
        FinaliseRequest {
            owner: "marad2001",
            repo: "test-repo",
            issue_number: 42,
            pr_number: 99,
            in_progress_label: "agent-in-progress",
            outcome_label: "agent-done",
            log_body: "Run completed",
        },
    )
    .await
    .expect("finalise should succeed");

    let label_names: Vec<&str> = updated.labels.iter().map(|l| l.name.as_str()).collect();
    assert!(label_names.contains(&"agent-done"));
    assert!(label_names.contains(&"enhancement"));
    assert!(!label_names.contains(&"agent-in-progress"));
}

#[tokio::test]
async fn finalise_applies_failure_label_when_outcome_is_agent_failed() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/repos/marad2001/test-repo/issues/77/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 7 })))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/55"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 55,
            "title": "Failing run",
            "labels": [{ "name": "agent-in-progress" }]
        })))
        .mount(&mock)
        .await;

    Mock::given(method("PATCH"))
        .and(path("/repos/marad2001/test-repo/issues/55"))
        .and(body_json(json!({ "labels": ["agent-failed"] })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 55,
            "title": "Failing run",
            "labels": [{ "name": "agent-failed" }]
        })))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let updated = finalise(
        &client,
        FinaliseRequest {
            owner: "marad2001",
            repo: "test-repo",
            issue_number: 55,
            pr_number: 77,
            in_progress_label: "agent-in-progress",
            outcome_label: "agent-failed",
            log_body: "Tests failed",
        },
    )
    .await
    .expect("finalise should succeed");

    let label_names: Vec<&str> = updated.labels.iter().map(|l| l.name.as_str()).collect();
    assert!(label_names.contains(&"agent-failed"));
    assert!(!label_names.contains(&"agent-in-progress"));
    assert!(!label_names.contains(&"agent-done"));
}

#[tokio::test]
async fn fetch_agent_brief_returns_body_of_the_brief_comment() {
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/42/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "id": 1, "body": "An unrelated drive-by comment." },
            {
                "id": 2,
                "body": "> *This was generated by AI during triage.*\n\n## Agent Brief\n\n**Summary:** do the thing"
            }
        ])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let brief = fetch_agent_brief(&client, "marad2001", "test-repo", 42)
        .await
        .expect("call should succeed");
    let body = brief.expect("expected a brief");
    assert!(body.contains("## Agent Brief"));
    assert!(body.contains("do the thing"));
}

#[tokio::test]
async fn fetch_agent_brief_returns_none_when_no_brief_comment_exists() {
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/42/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "id": 1, "body": "A drive-by comment that says nothing about a brief." },
            { "id": 2, "body": "Yet another." }
        ])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let brief = fetch_agent_brief(&client, "marad2001", "test-repo", 42)
        .await
        .expect("call should succeed");
    assert!(brief.is_none(), "expected None, got {:?}", brief);
}

#[tokio::test]
async fn post_pr_comment_posts_body_to_the_pr_comments_endpoint() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/repos/marad2001/test-repo/issues/99/comments"))
        .and(body_json(json!({
            "body": "## Review findings\n\nfound stuff"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    post_pr_comment(
        &client,
        "marad2001",
        "test-repo",
        99,
        "## Review findings\n\nfound stuff",
    )
    .await
    .expect("post_pr_comment should succeed");
}
