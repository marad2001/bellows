//! Integration tests for the polling loop's `blocked-by` filtering
//! and lowest-issue-number claim ordering (issue #116, ADR-0007).
//!
//! These tests drive `runner::run_once` end-to-end against a wiremock
//! GitHub. The runner's standard short-circuit on `MissingAgentBrief`
//! lets each test pin which issue the runner chose without driving
//! the pipeline past the brief-fetch step (no Docker, no clone, no
//! claim PATCH).

use std::io::Cursor;
use std::str::FromStr;

use bellows::config::Config;
use bellows::runner::{run_once, RunError, RunOutcome};
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

fn single_repo_config(mock_uri: &str) -> Config {
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
async fn normal_pass_filters_out_issues_carrying_blocked_by_label() {
    // AC #116-1: Given a repo with #1 (ready-for-agent + blocked-by),
    // #2 (ready-for-agent), #3 (ready-for-agent), the claim picks #2
    // — #1 has the lowest number but carries blocked-by and must be
    // filtered out. We surface the chosen issue via the runner's
    // MissingAgentBrief short-circuit — that error names the chosen
    // issue number without driving the pipeline past brief-fetch.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/pulls"))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .and(query_param("labels", "ready-for-agent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 1,
                "title": "dependent (lowest number but blocked)",
                "created_at": "2026-05-01T00:00:00Z",
                "labels": [
                    { "name": "ready-for-agent" },
                    { "name": "blocked-by" }
                ]
            },
            {
                "number": 2,
                "title": "should be picked",
                "created_at": "2026-05-02T00:00:00Z",
                "labels": [{ "name": "ready-for-agent" }]
            },
            {
                "number": 3,
                "title": "another unblocked",
                "created_at": "2026-05-03T00:00:00Z",
                "labels": [{ "name": "ready-for-agent" }]
            }
        ])))
        .mount(&mock)
        .await;

    // Pre-claim sweep for issue #2: no stale branches.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/git/matching-refs/heads/agent/2-"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    // Brief absent on the chosen issue -> short-circuit with the
    // chosen issue number in the error.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/2/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let config = single_repo_config(&mock.uri());
    let mut log = Cursor::new(Vec::new());
    let outcome = run_once(&client, &config, &mut log, None).await;
    match outcome {
        Err(RunError::MissingAgentBrief(n)) => assert_eq!(
            n, 2,
            "blocked-by-labelled #1 must be filtered out; lowest-number unblocked is #2 (got #{n})",
        ),
        other => panic!("expected MissingAgentBrief(2), got {other:?}"),
    }
}

#[tokio::test]
async fn claim_order_is_ascending_issue_number_not_created_at() {
    // AC #116-2 (tier 1): claim order is ascending `issue.number`,
    // NOT created_at. Two issues in one repo: #5 created LATER and
    // #20 created EARLIER. The old (created_at-only) sort would pick
    // #20; the new rule picks #5.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/pulls"))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .and(query_param("labels", "ready-for-agent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 20,
                "title": "older but higher-number",
                "created_at": "2026-01-01T00:00:00Z",
                "labels": [{ "name": "ready-for-agent" }]
            },
            {
                "number": 5,
                "title": "newer but lower-number",
                "created_at": "2026-05-01T00:00:00Z",
                "labels": [{ "name": "ready-for-agent" }]
            }
        ])))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/git/matching-refs/heads/agent/5-"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/5/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let config = single_repo_config(&mock.uri());
    let mut log = Cursor::new(Vec::new());
    let outcome = run_once(&client, &config, &mut log, None).await;
    match outcome {
        Err(RunError::MissingAgentBrief(n)) => assert_eq!(
            n, 5,
            "ascending issue.number must beat older created_at; expected #5, got #{n}",
        ),
        other => panic!("expected MissingAgentBrief(5), got {other:?}"),
    }
}

#[tokio::test]
async fn cross_repo_ties_on_number_break_on_older_created_at() {
    // AC #116-2 (tier 2): with the SAME issue number in two repos,
    // the OLDER created_at wins.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-a/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-b/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    // Both repos have issue #7. repo-a's is created 2026-01-01,
    // repo-b's is created 2026-06-01. Tie on number → older
    // created_at (repo-a) wins.
    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-a/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 7,
                "title": "older repo-a #7",
                "created_at": "2026-01-01T00:00:00Z",
                "labels": [{ "name": "ready-for-agent" }]
            }
        ])))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-b/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 7,
                "title": "newer repo-b #7",
                "created_at": "2026-06-01T00:00:00Z",
                "labels": [{ "name": "ready-for-agent" }]
            }
        ])))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-a/git/matching-refs/heads/agent/7-"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-a/issues/7/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    // Note: repo-b is listed FIRST in the config to ensure the
    // tie-break is genuinely on created_at and not on
    // declared-repo-order.
    let toml = format!(
        r#"
[[repo]]
url = "{base}/owner-x/repo-b"

[[repo]]
url = "{base}/owner-x/repo-a"

[github]
pat_env_var = "BELLOWS_TEST_PAT"
"#,
        base = mock.uri(),
    );
    let config = Config::from_str(&toml).expect("multi-repo config parses");
    let client = octocrab_pointed_at(mock.uri());
    let mut log = Cursor::new(Vec::new());
    let outcome = run_once(&client, &config, &mut log, None).await;
    match outcome {
        Err(RunError::MissingAgentBrief(n)) => assert_eq!(
            n, 7,
            "the chosen issue is #7, but we need to verify which repo's #7 was picked via the brief fetch URL",
        ),
        // The MissingAgentBrief variant only carries the number, so
        // this assertion alone doesn't fully prove "repo-a's #7 was
        // chosen". The wiremock setup itself does: we only mounted
        // a matching-refs route + comments route for repo-a/#7. If
        // run_once picked repo-b's #7, it would issue a request to
        // /repos/owner-x/repo-b/git/matching-refs/heads/agent/7-,
        // which has no mock and would surface as Err(Octocrab) (a
        // 404 from wiremock's default unmounted response), not
        // Err(MissingAgentBrief).
        other => panic!(
            "expected MissingAgentBrief(7) on repo-a's #7 (older created_at), got {other:?}",
        ),
    }
}

#[tokio::test]
async fn cross_repo_ties_on_number_and_created_at_break_on_repo_declared_order() {
    // AC #116-2 (tier 3): same number, same created_at — fall through
    // to declared `[[repo]]` order. repo-a appears first in the
    // config → repo-a's #11 must be selected.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-a/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-b/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-a/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 11,
                "title": "repo-a #11",
                "created_at": "2026-04-01T00:00:00Z",
                "labels": [{ "name": "ready-for-agent" }]
            }
        ])))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-b/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 11,
                "title": "repo-b #11",
                "created_at": "2026-04-01T00:00:00Z",
                "labels": [{ "name": "ready-for-agent" }]
            }
        ])))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-a/git/matching-refs/heads/agent/11-"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-a/issues/11/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    let toml = format!(
        r#"
[[repo]]
url = "{base}/owner-x/repo-a"

[[repo]]
url = "{base}/owner-x/repo-b"

[github]
pat_env_var = "BELLOWS_TEST_PAT"
"#,
        base = mock.uri(),
    );
    let config = Config::from_str(&toml).expect("multi-repo config parses");
    let client = octocrab_pointed_at(mock.uri());
    let mut log = Cursor::new(Vec::new());
    let outcome = run_once(&client, &config, &mut log, None).await;
    match outcome {
        Err(RunError::MissingAgentBrief(n)) => assert_eq!(
            n, 11,
            "tie on number+created_at must break to repo-a (declared first); see mock route surface",
        ),
        other => panic!(
            "expected MissingAgentBrief(11) on repo-a's #11 (declared first), got {other:?}",
        ),
    }
}

#[tokio::test]
async fn run_once_returns_idle_when_only_blocked_by_issues_exist_but_no_brief_to_fetch() {
    // AC #116-3 precondition: when the filtered set is empty AND
    // there are blocked-by-labelled issues, run_once does NOT return
    // Idle — it runs the re-loop sweep within the same tick. This
    // test pins the behaviour from the OTHER side: a blocked-by
    // issue WITHOUT an `## Agent Brief` comment (Unverifiable) keeps
    // its label and produces RunOutcome::Idle on this tick, with the
    // re-loop summary line "swept 1 blocked-by issues, cleared 0"
    // visible in the captured log.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/pulls"))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    // Filtered list will be empty: the only ready-for-agent issue
    // carries blocked-by.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .and(query_param("labels", "ready-for-agent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 42,
                "title": "dependent",
                "created_at": "2026-05-01T00:00:00Z",
                "labels": [
                    { "name": "ready-for-agent" },
                    { "name": "blocked-by" }
                ]
            }
        ])))
        .mount(&mock)
        .await;

    // Re-loop sweep tries to fetch the dependent's agent brief.
    // We return a comment without a `## Agent Brief` header — the
    // parser treats this as Unverifiable, the runner leaves the
    // label in place, the sweep counts cleared=0.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/42/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "body": "Some unrelated comment, no brief header." }
        ])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let config = single_repo_config(&mock.uri());
    let mut log = Cursor::new(Vec::new());
    let outcome = run_once(&client, &config, &mut log, None).await
        .expect("run_once should succeed");
    assert!(
        matches!(outcome, RunOutcome::Idle),
        "expected Idle (blocked-by issue stayed labelled), got {outcome:?}",
    );

    let log_text = String::from_utf8(log.into_inner()).expect("log is utf8");
    assert!(
        log_text.contains("bellows: re-loop swept 1 blocked-by issues, cleared 0"),
        "expected re-loop summary line in log, got:\n{log_text}",
    );
}

#[tokio::test]
async fn re_loop_sweep_strips_blocked_by_when_all_blockers_closed() {
    // AC #116-3 main path: when the filtered set is empty AND a
    // blocked-by issue's brief lists `**Blocked by:** #95` and #95
    // is CLOSED, the sweep PATCHes the dependent's labels to drop
    // `blocked-by`, the summary logs "cleared 1", and the outcome
    // is Idle for this tick (the cleared dependent becomes
    // claimable on the NEXT tick).
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/pulls"))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .and(query_param("labels", "ready-for-agent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 100,
                "title": "dependent",
                "created_at": "2026-05-01T00:00:00Z",
                "labels": [
                    { "name": "ready-for-agent" },
                    { "name": "blocked-by" }
                ]
            }
        ])))
        .mount(&mock)
        .await;

    // Re-loop sweep step 1: fetch brief for #100.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/100/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "body": "## Agent Brief\n\n**Blocked by:** #95\n\nrest of brief." }
        ])))
        .mount(&mock)
        .await;

    // Re-loop sweep step 2: check #95's state.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/95"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 95,
            "title": "the blocker",
            "state": "closed",
            "labels": []
        })))
        .mount(&mock)
        .await;

    // Re-loop sweep step 3: fetch current #100 labels for the PATCH
    // (so the existing label set is preserved minus blocked-by).
    // The runner uses GET-then-PATCH on the issue, the same shape as
    // claim/finalise.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 100,
            "title": "dependent",
            "labels": [
                { "name": "ready-for-agent" },
                { "name": "blocked-by" }
            ]
        })))
        .mount(&mock)
        .await;

    // Re-loop sweep step 4: PATCH #100 with blocked-by removed.
    // We assert with expect(1) that this fired exactly once.
    Mock::given(method("PATCH"))
        .and(path("/repos/marad2001/test-repo/issues/100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 100,
            "title": "dependent",
            "labels": [
                { "name": "ready-for-agent" }
            ]
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let config = single_repo_config(&mock.uri());
    let mut log = Cursor::new(Vec::new());
    let outcome = run_once(&client, &config, &mut log, None).await
        .expect("run_once should succeed");
    assert!(
        matches!(outcome, RunOutcome::Idle),
        "expected Idle on the tick that performed the sweep, got {outcome:?}",
    );

    let log_text = String::from_utf8(log.into_inner()).expect("log is utf8");
    assert!(
        log_text.contains("bellows: re-loop swept 1 blocked-by issues, cleared 1"),
        "expected sweep-cleared-1 summary in log, got:\n{log_text}",
    );
}

#[tokio::test]
async fn re_loop_sweep_does_not_run_when_normal_pass_found_work() {
    // AC #116-4: when the filtered candidate list is non-empty, the
    // re-loop sweep does NOT run. We mount NO comments route for the
    // blocked-by issue — if the sweep ran, it would issue a GET
    // against that comments endpoint and wiremock would return its
    // default 404, which would propagate as Err(Octocrab) and fail
    // this test's expected MissingAgentBrief outcome.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/pulls"))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .and(query_param("labels", "ready-for-agent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 5,
                "title": "dependent",
                "created_at": "2026-05-01T00:00:00Z",
                "labels": [
                    { "name": "ready-for-agent" },
                    { "name": "blocked-by" }
                ]
            },
            {
                "number": 9,
                "title": "unblocked",
                "created_at": "2026-05-02T00:00:00Z",
                "labels": [{ "name": "ready-for-agent" }]
            }
        ])))
        .mount(&mock)
        .await;

    // Only #9's pre-claim sweep + brief are mocked. If the runner
    // touched #5's comments at all (i.e. ran the re-loop sweep),
    // wiremock returns its default 404 -> Err(Octocrab) and this
    // test's expected MissingAgentBrief outcome fails.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/git/matching-refs/heads/agent/9-"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/9/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let config = single_repo_config(&mock.uri());
    let mut log = Cursor::new(Vec::new());
    let outcome = run_once(&client, &config, &mut log, None).await;
    match outcome {
        Err(RunError::MissingAgentBrief(9)) => {}
        other => panic!(
            "expected MissingAgentBrief(9) (re-loop must not have touched #5), got {other:?}",
        ),
    }

    let log_text = String::from_utf8(log.into_inner()).expect("log is utf8");
    assert!(
        !log_text.contains("re-loop swept"),
        "re-loop sweep must NOT log a summary when normal pass found work; got:\n{log_text}",
    );
}

#[tokio::test]
async fn re_loop_sweep_leaves_label_when_blocker_still_open() {
    // AC #116-3 negative case: blocker is OPEN, dependent stays
    // labelled, summary logs "cleared 0".
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/pulls"))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .and(query_param("labels", "ready-for-agent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 200,
                "title": "dependent",
                "created_at": "2026-05-01T00:00:00Z",
                "labels": [
                    { "name": "ready-for-agent" },
                    { "name": "blocked-by" }
                ]
            }
        ])))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/200/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "body": "## Agent Brief\n\n**Blocked by:** #150\n" }
        ])))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/150"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 150,
            "title": "still-open blocker",
            "state": "open",
            "labels": []
        })))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let config = single_repo_config(&mock.uri());
    let mut log = Cursor::new(Vec::new());
    let outcome = run_once(&client, &config, &mut log, None).await
        .expect("run_once should succeed");
    assert!(matches!(outcome, RunOutcome::Idle));

    let log_text = String::from_utf8(log.into_inner()).expect("log is utf8");
    assert!(
        log_text.contains("bellows: re-loop swept 1 blocked-by issues, cleared 0"),
        "expected sweep-cleared-0 summary in log, got:\n{log_text}",
    );
}
