use serde_json::json;
use wiremock::matchers::{body_json, body_partial_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use bellows::tracker::{
    add_issue_labels, apply_verdict, claim, delete_stale_agent_branches, fetch_agent_brief,
    fetch_issue_with_comments, finalise, find_next_issue, list_blocked_by_issues,
    list_needs_triage_issues, post_pr_comment, transition_to_cancelled, ClaimError,
    FinaliseRequest,
};
use bellows::triage::TriageVerdict;
use wiremock::matchers::body_string_contains;

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
        "blocked-by",
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
        "blocked-by",
    )
    .await
    .expect("call should succeed");

    let issue = result.expect("expected an issue");
    assert_eq!(issue.number, 42, "should return the un-claimed issue, not the in-progress one");
}

#[tokio::test]
async fn find_next_issue_paginates_before_choosing_lowest_number() {
    let mock = MockServer::start().await;
    let page_two = format!("{}/repos/marad2001/test-repo/issues?page=2", mock.uri());

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .and(query_param("labels", "ready-for-agent"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Link", format!("<{page_two}>; rel=\"next\""))
                .set_body_json(json!([
                    {
                        "number": 90,
                        "title": "first page candidate",
                        "labels": [{ "name": "ready-for-agent" }]
                    }
                ])),
        )
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 12,
                "title": "second page lowest",
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
        "blocked-by",
    )
    .await
    .expect("call should succeed");

    let issue = result.expect("expected an issue");
    assert_eq!(
        issue.number, 12,
        "pagination must happen before sorting so later pages can contain the true lowest candidate",
    );
}

#[tokio::test]
async fn list_blocked_by_issues_returns_dependents_from_every_page() {
    let mock = MockServer::start().await;
    let page_two = format!("{}/repos/marad2001/test-repo/issues?page=2", mock.uri());

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .and(query_param("labels", "ready-for-agent,blocked-by"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Link", format!("<{page_two}>; rel=\"next\""))
                .set_body_json(json!([
                    {
                        "number": 50,
                        "title": "first page dependent",
                        "labels": [
                            { "name": "ready-for-agent" },
                            { "name": "blocked-by" }
                        ]
                    }
                ])),
        )
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 51,
                "title": "second page dependent",
                "labels": [
                    { "name": "ready-for-agent" },
                    { "name": "blocked-by" }
                ]
            }
        ])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let issues = list_blocked_by_issues(
        &client,
        "marad2001",
        "test-repo",
        "ready-for-agent",
        "blocked-by",
    )
    .await
    .expect("call should succeed");

    let numbers: Vec<u64> = issues.into_iter().map(|i| i.number).collect();
    assert_eq!(
        numbers,
        vec![50, 51],
        "re-loop sweeping must consider blocked dependents beyond the first GitHub page",
    );
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
    let outcome = finalise(
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

    assert!(!outcome.externally_cancelled);
    let label_names: Vec<&str> = outcome.issue.labels.iter().map(|l| l.name.as_str()).collect();
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
    let outcome = finalise(
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

    assert!(!outcome.externally_cancelled);
    let label_names: Vec<&str> = outcome.issue.labels.iter().map(|l| l.name.as_str()).collect();
    assert!(label_names.contains(&"agent-failed"));
    assert!(!label_names.contains(&"agent-in-progress"));
    assert!(!label_names.contains(&"agent-done"));
}

#[tokio::test]
async fn add_issue_labels_posts_agent_noted_label_to_pr_issue_endpoint() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/repos/marad2001/test-repo/issues/99/labels"))
        .and(body_json(json!({ "labels": ["agent-noted"] })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "name": "agent-noted" }
        ])))
        .expect(1)
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let labels = add_issue_labels(
        &client,
        "marad2001",
        "test-repo",
        99,
        &["agent-noted"],
    )
    .await
    .expect("add_issue_labels should succeed");

    let label_names: Vec<&str> = labels.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(label_names, vec!["agent-noted"]);
}

#[tokio::test]
async fn finalise_skips_label_patch_when_in_progress_label_already_removed_externally() {
    // Slice 10 contract: when `bellows kill <N>` ran in another terminal,
    // it already removed the in_progress label and applied agent-cancelled.
    // The running orchestrator's finalise must (a) still post the log
    // comment so the operator sees what happened, (b) NOT issue the PATCH
    // (the operator's label is already correct), and (c) signal back via
    // `externally_cancelled = true` so run_once can return RunOutcome::Cancelled.
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
            "title": "Cancelled run",
            "labels": [
                { "name": "agent-cancelled" },
                { "name": "enhancement" }
            ]
        })))
        .mount(&mock)
        .await;

    // Deliberately mount NO PATCH mock: if finalise issues a PATCH against
    // an issue whose in_progress label is already gone, wiremock will
    // return 404 and the test will fail.

    let client = octocrab_pointed_at(mock.uri());
    let outcome = finalise(
        &client,
        FinaliseRequest {
            owner: "marad2001",
            repo: "test-repo",
            issue_number: 55,
            pr_number: 77,
            in_progress_label: "agent-in-progress",
            outcome_label: "agent-failed",
            log_body: "Run interrupted",
        },
    )
    .await
    .expect("finalise should succeed even when externally cancelled");

    assert!(outcome.externally_cancelled, "expected externally_cancelled = true");
    let label_names: Vec<&str> = outcome.issue.labels.iter().map(|l| l.name.as_str()).collect();
    // The returned issue reflects the current state (operator already labelled it).
    assert!(label_names.contains(&"agent-cancelled"));
    assert!(!label_names.contains(&"agent-in-progress"));
}

#[tokio::test]
async fn finalise_completes_label_transition_when_comment_post_fails_with_422() {
    // Issue #87 AC #1 + AC #3. When the run-log comment POST fails (e.g.
    // GitHub returns 422 because the body exceeds the 64 KiB hard limit
    // on issue/PR comments), the label transition must still complete
    // and `finalise` must return `Ok` so the surrounding pipeline reaches
    // a terminal `RunOutcome` instead of leaving the source issue stuck
    // at `agent-in-progress`. The comment-post failure is observability
    // only — the caller surfaces it via `comment_post_failure` for the
    // operator's log warning.
    let mock = MockServer::start().await;

    // GET issue: in_progress is still on the issue (no operator-side
    // cancellation happened mid-run).
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 42,
            "title": "Big issue with huge run-log",
            "labels": [
                { "name": "agent-in-progress" },
                { "name": "bug" }
            ]
        })))
        .mount(&mock)
        .await;

    // PATCH issue: succeeds, transitions to agent-done. This MUST happen
    // even though the comment POST below fails — AC #1's load-bearing
    // assertion is the label state machine reaching its terminal state
    // regardless of comment-post observability.
    Mock::given(method("PATCH"))
        .and(path("/repos/marad2001/test-repo/issues/42"))
        .and(body_json(json!({ "labels": ["agent-done", "bug"] })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 42,
            "title": "Big issue with huge run-log",
            "labels": [
                { "name": "agent-done" },
                { "name": "bug" }
            ]
        })))
        .mount(&mock)
        .await;

    // POST run-log comment: 422 with the exact body-too-long error the
    // live failure on workboard-financial-advice #15 produced.
    Mock::given(method("POST"))
        .and(path("/repos/marad2001/test-repo/issues/99/comments"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message": "Validation Failed",
            "errors": [{
                "resource": "IssueComment",
                "code": "unprocessable",
                "field": "data",
                "message": "Body is too long (maximum is 65536 characters)"
            }],
            "documentation_url": "https://docs.github.com/rest"
        })))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let outcome = finalise(
        &client,
        FinaliseRequest {
            owner: "marad2001",
            repo: "test-repo",
            issue_number: 42,
            pr_number: 99,
            in_progress_label: "agent-in-progress",
            outcome_label: "agent-done",
            log_body: "Run completed; this body will be rejected by the mock",
        },
    )
    .await
    .expect(
        "finalise must return Ok when only the comment POST fails — the label \
         state machine is independent of comment observability (AC #3)",
    );

    // AC #1: label transition reached the terminal state.
    assert!(!outcome.externally_cancelled);
    let label_names: Vec<&str> = outcome.issue.labels.iter().map(|l| l.name.as_str()).collect();
    assert!(
        label_names.contains(&"agent-done"),
        "expected agent-done in returned labels, got {:?}",
        label_names,
    );
    assert!(!label_names.contains(&"agent-in-progress"));

    // AC #3: the failure is surfaced to the caller so the polling loop /
    // log writer can record an operator-visible warning. We don't pin
    // the exact rendering — octocrab's Display may evolve — but the
    // field must be Some, carrying a non-empty message.
    let failure = outcome
        .comment_post_failure
        .as_ref()
        .expect("expected comment_post_failure to be Some so the caller can log a warning");
    assert!(
        !failure.is_empty(),
        "comment_post_failure must carry a non-empty diagnostic, got empty string",
    );
}

#[tokio::test]
async fn finalise_truncates_oversized_log_body_before_posting() {
    // Issue #87 AC #2. `finalise` is the GitHub API boundary — if a
    // caller hands it a body that would exceed GitHub's 64 KiB comment
    // limit, finalise must clip it (with a clear footer pointing the
    // operator at bellows.log for the full output) so the POST itself
    // succeeds. Defensive truncation here makes the contract hold
    // regardless of whatever caller-side size-awareness exists.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 42,
            "title": "Big run",
            "labels": [{ "name": "agent-in-progress" }]
        })))
        .mount(&mock)
        .await;

    Mock::given(method("PATCH"))
        .and(path("/repos/marad2001/test-repo/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 42,
            "title": "Big run",
            "labels": [{ "name": "agent-done" }]
        })))
        .mount(&mock)
        .await;

    // Generic POST mock — accepts any body so we can capture and inspect
    // what finalise actually sent after truncation.
    Mock::given(method("POST"))
        .and(path("/repos/marad2001/test-repo/issues/99/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .mount(&mock)
        .await;

    // ~80 KiB log body — well past GitHub's 64 KiB hard limit.
    let oversized = "a".repeat(80_000);

    let client = octocrab_pointed_at(mock.uri());
    let outcome = finalise(
        &client,
        FinaliseRequest {
            owner: "marad2001",
            repo: "test-repo",
            issue_number: 42,
            pr_number: 99,
            in_progress_label: "agent-in-progress",
            outcome_label: "agent-done",
            log_body: &oversized,
        },
    )
    .await
    .expect("finalise must not return Err when handed an oversized body");

    // Comment POST succeeded (truncation kept the body under the API
    // limit), so no failure was recorded.
    assert!(
        outcome.comment_post_failure.is_none(),
        "expected no comment_post_failure after defensive truncation, got: {:?}",
        outcome.comment_post_failure,
    );

    // Inspect what was actually POSTed.
    let requests = mock
        .received_requests()
        .await
        .expect("wiremock should expose received requests");
    let post_body = requests
        .iter()
        .find(|r| {
            r.method == wiremock::http::Method::POST
                && r.url.path() == "/repos/marad2001/test-repo/issues/99/comments"
        })
        .expect("expected at least one POST to the comments endpoint")
        .body
        .clone();
    let posted: serde_json::Value = serde_json::from_slice(&post_body)
        .expect("POST body should be JSON");
    let posted_text = posted
        .get("body")
        .and_then(|v| v.as_str())
        .expect("posted JSON should have a `body` field");

    // The actually-posted body stays well under GitHub's 64 KiB ceiling.
    assert!(
        posted_text.chars().count() <= 65000,
        "posted body must be under GitHub's 64 KiB comment limit; got {} chars",
        posted_text.chars().count(),
    );
    // Truncation footer is visible to the operator.
    assert!(
        posted_text.to_lowercase().contains("truncated"),
        "posted body must contain a 'truncated' marker so the reader \
         can tell content was clipped",
    );
    assert!(
        posted_text.contains("bellows.log"),
        "truncation footer must point the operator at bellows.log for \
         the full output",
    );
}

#[tokio::test]
async fn transition_to_cancelled_posts_short_comment_and_swaps_labels() {
    // The `bellows kill <N>` GitHub-side handler. Posts a short
    // AI-disclaimer-style comment so a human reading the issue knows what
    // happened, then swaps `agent-in-progress` for `agent-cancelled`.
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/repos/marad2001/test-repo/issues/55/comments"))
        // The body is fixed-template ("> *bellows: cancelled by operator at <ts>*")
        // — pin the recognisable substrings rather than the full timestamp so
        // the test is not tied to wall-clock now().
        .and(body_string_contains("bellows"))
        .and(body_string_contains("cancelled by operator"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 8 })))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/55"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 55,
            "title": "Run to cancel",
            "labels": [
                { "name": "agent-in-progress" },
                { "name": "enhancement" }
            ]
        })))
        .mount(&mock)
        .await;

    Mock::given(method("PATCH"))
        .and(path("/repos/marad2001/test-repo/issues/55"))
        .and(body_json(json!({ "labels": ["agent-cancelled", "enhancement"] })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 55,
            "title": "Run to cancel",
            "labels": [
                { "name": "agent-cancelled" },
                { "name": "enhancement" }
            ]
        })))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let updated = transition_to_cancelled(
        &client,
        "marad2001",
        "test-repo",
        55,
        "agent-in-progress",
        "agent-cancelled",
    )
    .await
    .expect("transition_to_cancelled should succeed");

    let label_names: Vec<&str> = updated.labels.iter().map(|l| l.name.as_str()).collect();
    assert!(label_names.contains(&"agent-cancelled"));
    assert!(label_names.contains(&"enhancement"));
    assert!(!label_names.contains(&"agent-in-progress"));
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

// Issue #126 / ADR-0009 slice 4: the `list_open_agent_prs` tests have
// been removed alongside the function itself. The pre-claim PR-open
// gate (issue #42) has been replaced with a global container-presence
// probe, and the function had no other callers.

#[tokio::test]
async fn list_needs_triage_issues_filters_to_open_issues_with_needs_triage_label_oldest_first() {
    // Slice T2 (#22): the backlog-drain CLI queries this endpoint for
    // every open `needs-triage` issue oldest-first. The query params
    // are part of the contract — `sort=created&direction=asc` is what
    // produces the oldest-first ordering, and `state=open` ensures
    // closed issues don't leak in.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .and(query_param("labels", "needs-triage"))
        .and(query_param("state", "open"))
        .and(query_param("sort", "created"))
        .and(query_param("direction", "asc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "number": 3, "title": "filed first",  "labels": [{ "name": "needs-triage" }] },
            { "number": 7, "title": "filed later",  "labels": [{ "name": "needs-triage" }] }
        ])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let issues = list_needs_triage_issues(&client, "marad2001", "test-repo", "needs-triage")
        .await
        .expect("call should succeed");

    assert_eq!(issues.len(), 2);
    assert_eq!(issues[0].number, 3, "oldest must come first");
    assert_eq!(issues[1].number, 7);
}

#[tokio::test]
async fn list_needs_triage_issues_returns_empty_when_no_needs_triage_issues_open() {
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .and(query_param("labels", "needs-triage"))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let issues = list_needs_triage_issues(&client, "marad2001", "test-repo", "needs-triage")
        .await
        .expect("call should succeed");

    assert!(issues.is_empty(), "expected empty, got {:?}", issues);
}

#[tokio::test]
async fn list_needs_triage_issues_surfaces_github_errors() {
    // Slice T2 backlog drain must distinguish "empty backlog" from
    // "GitHub call failed" so the operator sees the failure rather
    // than a misleading "0 issues processed" summary.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .respond_with(ResponseTemplate::new(500).set_body_string("server exploded"))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let result = list_needs_triage_issues(&client, "marad2001", "test-repo", "needs-triage").await;
    assert!(result.is_err(), "expected Err, got {:?}", result.map(|_| "Ok"));
}

// ----------------------------------------------------------------------
// Slice T1 (#21): fetch_issue_with_comments + apply_verdict. The
// `bellows triage <N>` command depends on a typed IssueBundle that
// carries the issue body, current labels, and full comment history
// into the sandbox-side input file, and on an apply_verdict that
// posts comments, transitions labels, and (for wontfix) closes the
// issue. Workspace-side .out-of-scope handling lives in
// `workspace::commit_to_branch`, not here.
// ----------------------------------------------------------------------

#[tokio::test]
async fn fetch_issue_with_comments_bundles_body_labels_and_ordered_comment_history() {
    // Operator contract: the agent sees the issue body, the current
    // label set, and the full ordered comment history. Comment order
    // matters — a prior triage note may include questions the
    // reporter then answered in a later comment, and the agent must
    // see the answer-after-the-question sequence to make the right
    // verdict.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/77"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 77,
            "title": "Foo crashes on empty input",
            "body": "Repro: pass `\"\"` to `foo()` and it panics.",
            "labels": [
                { "name": "needs-info" },
                { "name": "bug" }
            ]
        })))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/77/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "body": "What version were you on?" },
            { "body": "0.4.2." }
        ])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let bundle = fetch_issue_with_comments(&client, "marad2001", "test-repo", 77)
        .await
        .expect("fetch_issue_with_comments should succeed");

    assert_eq!(bundle.number, 77);
    assert_eq!(bundle.title, "Foo crashes on empty input");
    assert!(bundle.body.as_deref().unwrap().contains("Repro"));
    assert_eq!(
        bundle
            .labels
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>(),
        vec!["needs-info", "bug"],
    );
    assert_eq!(bundle.comments.len(), 2);
    assert!(bundle.comments[0].contains("What version"));
    assert!(bundle.comments[1].contains("0.4.2"));
}

#[tokio::test]
async fn fetch_issue_with_comments_returns_an_empty_comment_list_when_no_comments_yet() {
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 42,
            "title": "Fresh issue",
            "body": null,
            "labels": [{ "name": "needs-triage" }]
        })))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/42/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let bundle = fetch_issue_with_comments(&client, "marad2001", "test-repo", 42)
        .await
        .expect("fetch_issue_with_comments should succeed");
    assert!(bundle.body.is_none());
    assert!(bundle.comments.is_empty());
    assert_eq!(
        bundle
            .labels
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>(),
        vec!["needs-triage"],
    );
}

#[tokio::test]
async fn apply_verdict_for_ready_for_agent_posts_comment_brief_and_swaps_labels() {
    // ready-for-agent flow: post the disclaimer-prefixed comment, post
    // the `## Agent Brief` as a SEPARATE comment so the downstream
    // bellows-run pipeline's `tracker::fetch_agent_brief` (which scans
    // for the literal `## Agent Brief` header) picks it up, then swap
    // any current triage state label for `ready-for-agent`.
    let mock = MockServer::start().await;

    // GET current state — currently `needs-triage`.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/77"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 77,
            "title": "Foo",
            "labels": [
                { "name": "needs-triage" },
                { "name": "bug" }
            ]
        })))
        .mount(&mock)
        .await;

    // POST the verdict's main comment_body (with AI disclaimer prefix).
    Mock::given(method("POST"))
        .and(path("/repos/marad2001/test-repo/issues/77/comments"))
        .and(body_string_contains("generated by AI during triage"))
        .and(body_string_contains("Moving to ready-for-agent."))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .expect(1)
        .mount(&mock)
        .await;

    // POST a separate `## Agent Brief` comment so fetch_agent_brief picks it up.
    Mock::given(method("POST"))
        .and(path("/repos/marad2001/test-repo/issues/77/comments"))
        .and(body_string_contains("## Agent Brief"))
        .and(body_string_contains("Fix the foo bug"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 2 })))
        .expect(1)
        .mount(&mock)
        .await;

    // PATCH labels: drop needs-triage, add ready-for-agent, preserve `bug`.
    Mock::given(method("PATCH"))
        .and(path("/repos/marad2001/test-repo/issues/77"))
        .and(body_json(json!({ "labels": ["bug", "ready-for-agent"] })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 77,
            "title": "Foo",
            "labels": [
                { "name": "bug" },
                { "name": "ready-for-agent" }
            ]
        })))
        .mount(&mock)
        .await;

    let verdict = TriageVerdict::parse(
        "{
            \"category\": \"bug\",
            \"state\": \"ready-for-agent\",
            \"reasoning\": \"clear repro\",
            \"comment_body\": \"Moving to ready-for-agent.\",
            \"agent_brief\": \"## Agent Brief\\n\\nFix the foo bug.\"
        }",
    )
    .expect("valid verdict");

    let client = octocrab_pointed_at(mock.uri());
    apply_verdict(&client, "marad2001", "test-repo", 77, &verdict)
        .await
        .expect("apply_verdict should succeed");
}

#[tokio::test]
async fn apply_verdict_for_needs_info_posts_only_the_comment_and_swaps_labels() {
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/88"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 88,
            "title": "Vague",
            "labels": [{ "name": "needs-triage" }]
        })))
        .mount(&mock)
        .await;

    // Only one comment expected — the AI-disclaimer-prefixed body. No
    // separate brief; the agent_brief / human_brief paths must not fire.
    Mock::given(method("POST"))
        .and(path("/repos/marad2001/test-repo/issues/88/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .expect(1)
        .mount(&mock)
        .await;

    Mock::given(method("PATCH"))
        .and(path("/repos/marad2001/test-repo/issues/88"))
        .and(body_json(json!({ "labels": ["needs-info"] })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 88,
            "title": "Vague",
            "labels": [{ "name": "needs-info" }]
        })))
        .mount(&mock)
        .await;

    let verdict = TriageVerdict::parse(
        "{
            \"category\": \"bug\",
            \"state\": \"needs-info\",
            \"reasoning\": \"no repro\",
            \"comment_body\": \"Need repro steps please.\"
        }",
    )
    .expect("valid verdict");

    let client = octocrab_pointed_at(mock.uri());
    apply_verdict(&client, "marad2001", "test-repo", 88, &verdict)
        .await
        .expect("apply_verdict should succeed");
}

#[tokio::test]
async fn apply_verdict_for_ready_for_human_posts_main_comment_and_human_brief() {
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/99"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 99,
            "title": "Big design call",
            "labels": [{ "name": "needs-triage" }]
        })))
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/marad2001/test-repo/issues/99/comments"))
        .and(body_string_contains("Routing this to a human"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .expect(1)
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/marad2001/test-repo/issues/99/comments"))
        .and(body_string_contains("## Human Brief"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 2 })))
        .expect(1)
        .mount(&mock)
        .await;

    Mock::given(method("PATCH"))
        .and(path("/repos/marad2001/test-repo/issues/99"))
        .and(body_json(json!({ "labels": ["ready-for-human"] })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 99,
            "title": "Big design call",
            "labels": [{ "name": "ready-for-human" }]
        })))
        .mount(&mock)
        .await;

    let verdict = TriageVerdict::parse(
        "{
            \"category\": \"enhancement\",
            \"state\": \"ready-for-human\",
            \"reasoning\": \"needs human judgement\",
            \"comment_body\": \"Routing this to a human.\",
            \"human_brief\": \"## Human Brief\\n\\nDecide on the schema migration.\"
        }",
    )
    .expect("valid verdict");

    let client = octocrab_pointed_at(mock.uri());
    apply_verdict(&client, "marad2001", "test-repo", 99, &verdict)
        .await
        .expect("apply_verdict should succeed");
}

#[tokio::test]
async fn apply_verdict_for_wontfix_bug_posts_comment_swaps_labels_and_closes_issue() {
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 123,
            "title": "Stale",
            "labels": [
                { "name": "needs-info" },
                { "name": "bug" }
            ]
        })))
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/marad2001/test-repo/issues/123/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .expect(1)
        .mount(&mock)
        .await;

    // Label transition AND the close happen via PATCH on the issue —
    // bellows merges them into a single PATCH to keep the issue's
    // observed state coherent (no half-applied verdict on transient
    // network failures between the two PATCHes).
    Mock::given(method("PATCH"))
        .and(path("/repos/marad2001/test-repo/issues/123"))
        .and(body_partial_json(json!({ "state": "closed" })))
        .and(body_partial_json(json!({ "labels": ["bug", "wontfix"] })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 123,
            "title": "Stale",
            "state": "closed",
            "labels": [
                { "name": "bug" },
                { "name": "wontfix" }
            ]
        })))
        .mount(&mock)
        .await;

    let verdict = TriageVerdict::parse(
        "{
            \"category\": \"bug\",
            \"state\": \"wontfix\",
            \"reasoning\": \"not reproducible\",
            \"comment_body\": \"Closing as wontfix.\",
            \"close_issue\": true
        }",
    )
    .expect("valid verdict");

    let client = octocrab_pointed_at(mock.uri());
    apply_verdict(&client, "marad2001", "test-repo", 123, &verdict)
        .await
        .expect("apply_verdict should succeed");
}

#[tokio::test]
async fn apply_verdict_strips_all_canonical_triage_labels_before_adding_the_new_state_label() {
    // Re-triage path (e.g. `bellows triage <N>` against an issue that
    // was previously needs-info and now has a reporter response). The
    // current triage state label MUST be replaced, not duplicated —
    // if `needs-info` and `ready-for-agent` both stay on the issue,
    // the downstream runner's label-state machine breaks.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/55"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 55,
            "title": "Re-triage",
            "labels": [
                { "name": "needs-info" },
                { "name": "enhancement" }
            ]
        })))
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/marad2001/test-repo/issues/55/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .mount(&mock)
        .await;

    Mock::given(method("PATCH"))
        .and(path("/repos/marad2001/test-repo/issues/55"))
        .and(body_json(json!({ "labels": ["enhancement", "ready-for-agent"] })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 55,
            "title": "Re-triage",
            "labels": [
                { "name": "enhancement" },
                { "name": "ready-for-agent" }
            ]
        })))
        .mount(&mock)
        .await;

    let verdict = TriageVerdict::parse(
        "{
            \"category\": \"enhancement\",
            \"state\": \"ready-for-agent\",
            \"reasoning\": \"reporter answered the questions\",
            \"comment_body\": \"Re-triaged.\",
            \"agent_brief\": \"## Agent Brief\\n\\nProceed.\"
        }",
    )
    .expect("valid verdict");

    let client = octocrab_pointed_at(mock.uri());
    apply_verdict(&client, "marad2001", "test-repo", 55, &verdict)
        .await
        .expect("apply_verdict should succeed");
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

// ---- Issue #76 / ADR-0003: pre-claim deletion of stale `agent/<N>-*`
//      branches on origin. The function walks `git/matching-refs/heads/agent/{N}-`,
//      DELETEs every match, and returns the list of successfully-deleted
//      names. 404 on an individual DELETE is treated as success (the branch
//      raced out from under us — same end state). Any other non-success
//      propagates as Err so the runner can surface RunOutcome::Blocked. ----

#[tokio::test]
async fn delete_stale_agent_branches_deletes_a_single_matching_ref() {
    // The brief's "stale branch from a prior failed run" case: one
    // `agent/<N>-<slug>` ref exists on origin, the DELETE returns 204,
    // the function returns a vec with that branch name.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/git/matching-refs/heads/agent/16-"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "ref": "refs/heads/agent/16-foo",
                "node_id": "n1",
                "url": "http://example/refs/heads/agent/16-foo",
                "object": { "sha": "deadbeef", "type": "commit", "url": "http://example/commits/deadbeef" }
            }
        ])))
        .mount(&mock)
        .await;

    Mock::given(method("DELETE"))
        .and(path("/repos/marad2001/test-repo/git/refs/heads/agent/16-foo"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let deleted = delete_stale_agent_branches(&client, "marad2001", "test-repo", 16)
        .await
        .expect("delete_stale_agent_branches should succeed");
    assert_eq!(deleted, vec!["agent/16-foo".to_string()]);
}

#[tokio::test]
async fn delete_stale_agent_branches_deletes_every_match_for_the_issue_number() {
    // The title-edit case from the brief: the operator renamed the issue
    // between failed runs, so two distinct `agent/16-*` slugs both exist
    // on origin. Exact-slug matching would leak the orphan; the
    // anchored-prefix sweep deletes BOTH branches.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/git/matching-refs/heads/agent/16-"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "ref": "refs/heads/agent/16-foo",
                "node_id": "n1",
                "url": "http://example/refs/heads/agent/16-foo",
                "object": { "sha": "aaa", "type": "commit", "url": "http://example/commits/aaa" }
            },
            {
                "ref": "refs/heads/agent/16-bar",
                "node_id": "n2",
                "url": "http://example/refs/heads/agent/16-bar",
                "object": { "sha": "bbb", "type": "commit", "url": "http://example/commits/bbb" }
            }
        ])))
        .mount(&mock)
        .await;

    Mock::given(method("DELETE"))
        .and(path("/repos/marad2001/test-repo/git/refs/heads/agent/16-foo"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&mock)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/repos/marad2001/test-repo/git/refs/heads/agent/16-bar"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let deleted = delete_stale_agent_branches(&client, "marad2001", "test-repo", 16)
        .await
        .expect("delete_stale_agent_branches should succeed");
    let mut names = deleted;
    names.sort();
    assert_eq!(
        names,
        vec!["agent/16-bar".to_string(), "agent/16-foo".to_string()],
    );
}

#[tokio::test]
async fn delete_stale_agent_branches_treats_404_on_delete_as_success_and_continues() {
    // Idempotency contract per ADR-0003: a 404 on an individual DELETE
    // means the branch raced out from under us — same end state as
    // having deleted it ourselves. The function must NOT propagate the
    // 404 as Err, and must continue processing any remaining refs.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/git/matching-refs/heads/agent/16-"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "ref": "refs/heads/agent/16-gone",
                "node_id": "n1",
                "url": "http://example/refs/heads/agent/16-gone",
                "object": { "sha": "aaa", "type": "commit", "url": "http://example/commits/aaa" }
            },
            {
                "ref": "refs/heads/agent/16-still-here",
                "node_id": "n2",
                "url": "http://example/refs/heads/agent/16-still-here",
                "object": { "sha": "bbb", "type": "commit", "url": "http://example/commits/bbb" }
            }
        ])))
        .mount(&mock)
        .await;

    Mock::given(method("DELETE"))
        .and(path("/repos/marad2001/test-repo/git/refs/heads/agent/16-gone"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "message": "Reference does not exist",
            "documentation_url": "https://docs.github.com/..."
        })))
        .mount(&mock)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/repos/marad2001/test-repo/git/refs/heads/agent/16-still-here"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let deleted = delete_stale_agent_branches(&client, "marad2001", "test-repo", 16)
        .await
        .expect("404 on individual DELETE must be treated as success");
    // The 404'd branch counts as successfully removed (idempotency), and
    // the next ref is still processed.
    let mut names = deleted;
    names.sort();
    assert_eq!(
        names,
        vec![
            "agent/16-gone".to_string(),
            "agent/16-still-here".to_string(),
        ],
    );
}

#[tokio::test]
async fn delete_stale_agent_branches_propagates_403_as_err_so_runner_can_block() {
    // Branch protection or PAT scope: GitHub returns 403 on the DELETE.
    // Per the brief, this must surface as Err so the runner returns
    // RunOutcome::Blocked. The next tick will retry — the contract is
    // idempotent across retries.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/git/matching-refs/heads/agent/16-"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "ref": "refs/heads/agent/16-protected",
                "node_id": "n1",
                "url": "http://example/refs/heads/agent/16-protected",
                "object": { "sha": "aaa", "type": "commit", "url": "http://example/commits/aaa" }
            }
        ])))
        .mount(&mock)
        .await;

    Mock::given(method("DELETE"))
        .and(path("/repos/marad2001/test-repo/git/refs/heads/agent/16-protected"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "message": "Resource not accessible by integration",
            "documentation_url": "https://docs.github.com/..."
        })))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let result = delete_stale_agent_branches(&client, "marad2001", "test-repo", 16).await;
    assert!(
        result.is_err(),
        "403 on DELETE must propagate as Err so the runner can return Blocked, got Ok({:?})",
        result.ok(),
    );
}

#[tokio::test]
async fn delete_stale_agent_branches_returns_empty_vec_when_no_refs_match() {
    // Common case on a clean repo: no `agent/16-*` refs exist. The
    // matching-refs endpoint returns an empty array; the function must
    // NOT issue any DELETE calls and must return an empty vec so the
    // runner sees "zero swept" and proceeds to claim.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/git/matching-refs/heads/agent/16-"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    // No DELETE mock mounted: if the function issues one, wiremock will
    // 404 and the test will surface that as an Err. The Ok(empty) path
    // is what proves no DELETE was attempted.

    let client = octocrab_pointed_at(mock.uri());
    let deleted = delete_stale_agent_branches(&client, "marad2001", "test-repo", 16)
        .await
        .expect("no matching refs should be the happy path, not an error");
    assert!(deleted.is_empty(), "expected empty, got {:?}", deleted);
}

#[tokio::test]
async fn delete_stale_agent_branches_uses_anchored_prefix_with_trailing_dash() {
    // ADR-0003 rationale: the dash separator in the anchored prefix
    // prevents collision between `agent/16-*` and `agent/160-*`. Pin the
    // matching-refs URL so a future implementation can't silently drop
    // the dash and start eating cross-issue branches when an issue's
    // number is a prefix of another's.
    let mock = MockServer::start().await;

    // ANY matching-refs path under `agent/16-` (with the dash) returns
    // empty; a path WITHOUT the dash (or with the wrong number) would
    // hit a wiremock-default 404 — which the function would propagate
    // as Err, failing the test.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/git/matching-refs/heads/agent/16-"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let deleted = delete_stale_agent_branches(&client, "marad2001", "test-repo", 16)
        .await
        .expect("function must query the anchored-with-dash prefix");
    assert!(deleted.is_empty());
}
