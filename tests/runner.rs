//! Integration tests for `runner::run_once`'s pre-claim PR check (#42).
//!
//! These tests only exercise the pre-claim path: when the list-PRs check
//! is blocked or fails, `run_once` short-circuits before it would
//! otherwise touch the workspace, sandbox, or Docker. The "not blocked,
//! no ready-for-agent issues" case also exits cleanly via the existing
//! `RunOutcome::Idle` path without needing a real repo, so we can
//! drive the function entirely against a wiremock GitHub.

use std::io::Cursor;
use std::path::Path;
use std::process::Command;
use std::str::FromStr;

use bellows::config::Config;
use bellows::runner::{
    capture_and_remove_agent_notes, post_agent_notes_comment_if_present, run_once, BlockReason,
    RunError, RunOutcome,
};
use bellows::workspace;
use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn octocrab_pointed_at(uri: String) -> octocrab::Octocrab {
    octocrab::OctocrabBuilder::new()
        .base_uri(uri)
        .expect("base uri")
        .build()
        .expect("octocrab")
}

fn init_remote_repo(path: &Path) {
    run_git(path, &["init"]);
    run_git(path, &["config", "user.email", "test@example.com"]);
    run_git(path, &["config", "user.name", "Test"]);
    std::fs::write(path.join("README.md"), "test\n").unwrap();
    run_git(path, &["add", "."]);
    run_git(path, &["commit", "-m", "initial"]);
}

fn run_git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("git invocation");
    assert!(status.success(), "git {:?} failed in {:?}", args, cwd);
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
        RunOutcome::Blocked {
            reason: BlockReason::OpenAgentPrs { pr_numbers },
        } => {
            assert_eq!(pr_numbers, vec![41]);
        }
        other => panic!("expected Blocked(OpenAgentPrs), got {other:?}"),
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
        RunOutcome::Blocked {
            reason: BlockReason::OpenAgentPrs { pr_numbers },
        } => assert_eq!(pr_numbers, vec![99]),
        other => panic!("expected Blocked(OpenAgentPrs[99]), got {other:?}"),
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
        RunOutcome::Blocked {
            reason: BlockReason::OpenAgentPrs { pr_numbers },
        } => assert!(
            pr_numbers.is_empty(),
            "fail-closed Blocked has empty pr_numbers, got {pr_numbers:?}",
        ),
        other => panic!("expected Blocked(OpenAgentPrs[]) (fail-closed), got {other:?}"),
    }
}

// ---- Issue #35: multi-repo polling. Oldest-by-`created_at` across all
//      configured repos is claimed first. ----

#[tokio::test]
async fn run_once_picks_oldest_issue_across_multiple_repos_by_created_at() {
    // Issue #35 acceptance criterion: with two `[[repo]]` entries
    // configured and one open `ready-for-agent` issue in each (different
    // `created_at`), the OLDER issue is claimed first regardless of
    // `[[repo]]` ordering. We assert this by stubbing the issues
    // endpoint on each repo and observing which issue the runner
    // attempts to fetch the agent brief for next — `MissingAgentBrief`
    // surfaces the chosen issue number without driving the rest of the
    // pipeline (no Docker, no clone, no claim PATCH).
    //
    // repo-a's issue was created EARLIER than repo-b's, so repo-a's
    // issue #10 must be selected even though repo-b appears first in
    // the config. Pinning the cross-repo tiebreak this way catches
    // an implementation that defaulted to "first repo's first issue"
    // instead of computing the global oldest.
    let mock = MockServer::start().await;

    // Both repos cleared on the pre-claim PR check.
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

    // repo-a has the older issue (#10, 2026-01-01).
    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-a/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 10,
                "title": "older issue from repo-a",
                "created_at": "2026-01-01T00:00:00Z",
                "labels": [{ "name": "ready-for-agent" }]
            }
        ])))
        .mount(&mock)
        .await;

    // repo-b has the newer issue (#20, 2026-02-01). It must NOT be the
    // chosen issue this tick — selection is global-oldest, not first
    // repo wins.
    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-b/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 20,
                "title": "newer issue from repo-b",
                "created_at": "2026-02-01T00:00:00Z",
                "labels": [{ "name": "ready-for-agent" }]
            }
        ])))
        .mount(&mock)
        .await;

    // Pre-claim stale-branch sweep (#76): no stale refs on repo-a for
    // issue #10. The sweep must succeed before the runner moves on to
    // the brief fetch, otherwise the test would surface Blocked rather
    // than MissingAgentBrief.
    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-a/git/matching-refs/heads/agent/10-"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    // Agent-brief comments endpoint returns empty for the chosen issue
    // so the runner surfaces `MissingAgentBrief(N)` and short-circuits
    // BEFORE touching the workspace, sandbox, or claim path. The N we
    // see in the error proves which issue the runner picked.
    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-a/issues/10/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

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
            n, 10,
            "oldest issue across repos should be picked (#10 in repo-a, created 2026-01-01); got #{n}",
        ),
        other => panic!(
            "expected MissingAgentBrief(10) — the oldest issue's brief is missing — got {other:?}",
        ),
    }
}

#[tokio::test]
async fn run_once_only_blocks_when_every_configured_repo_is_blocked() {
    // Issue #35 nuance: per-repo pre-claim check. Repo A being blocked
    // by its own open `agent/*` PR must NOT block claims from repo B.
    // The cross-repo invariant is concurrency=1, which the polling loop
    // maintains by being serial. Verifying this avoids regressing into
    // a "block everything if any repo is blocked" behaviour that would
    // stall multi-repo deployments whenever any one repo's CI is slow.
    let mock = MockServer::start().await;

    // repo-a is blocked by its own open agent PR.
    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-a/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "number": 41, "head": { "ref": "agent/41-foo" }, "draft": false }
        ])))
        .mount(&mock)
        .await;

    // repo-b is clear.
    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-b/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    // repo-b has a ready-for-agent issue #50.
    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-b/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 50,
                "title": "unblocked issue from repo-b",
                "created_at": "2026-03-01T00:00:00Z",
                "labels": [{ "name": "ready-for-agent" }]
            }
        ])))
        .mount(&mock)
        .await;

    // Pre-claim stale-branch sweep (#76) for repo-b/#50: no stale refs.
    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-b/git/matching-refs/heads/agent/50-"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    // Brief missing on #50 -> short-circuit at MissingAgentBrief(50)
    // proves repo-b's issue was picked despite repo-a being blocked.
    Mock::given(method("GET"))
        .and(path("/repos/owner-x/repo-b/issues/50/comments"))
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
            n, 50,
            "repo-b's unblocked issue should be selected even when repo-a is blocked",
        ),
        other => panic!(
            "expected MissingAgentBrief(50) (repo-b's issue), got {other:?}",
        ),
    }
}

// ---- Issue #76 / ADR-0003: pre-claim deletion of stale agent/<N>-*
//      branches on origin. The sweep fires after find_next_issue picks a
//      candidate but before the claim PATCH. Successful sweep proceeds;
//      failure path returns RunOutcome::Blocked with the failing branch
//      named in the reason so the operator can recover. ----

#[tokio::test]
async fn run_once_sweeps_stale_agent_branches_before_claiming() {
    // Brief AC: "`runner::run_once` calls `delete_stale_agent_branches`
    // after `find_next_issue` and before `claim`. Success path proceeds."
    // We drive run_once up to MissingAgentBrief (so we never hit the
    // workspace clone) and use a wiremock `expect(1)` to assert that the
    // DELETE against the stale ref was actually issued. If the sweep
    // were skipped, expect(1) would fail when the MockServer drops.
    let mock = MockServer::start().await;

    // Pre-claim PR check: clear.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/pulls"))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    // One ready-for-agent issue #16.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .and(query_param("labels", "ready-for-agent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 16,
                "title": "Pre-claim sweep target",
                "created_at": "2026-05-12T10:00:00Z",
                "labels": [{ "name": "ready-for-agent" }]
            }
        ])))
        .mount(&mock)
        .await;

    // matching-refs returns a stale agent/16-* ref.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/git/matching-refs/heads/agent/16-"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "ref": "refs/heads/agent/16-old-slug",
                "node_id": "n1",
                "url": "http://example/refs/heads/agent/16-old-slug",
                "object": { "sha": "aaa", "type": "commit", "url": "http://example/commits/aaa" }
            }
        ])))
        .mount(&mock)
        .await;

    // The DELETE must fire exactly once.
    Mock::given(method("DELETE"))
        .and(path("/repos/marad2001/test-repo/git/refs/heads/agent/16-old-slug"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&mock)
        .await;

    // Brief is missing on the issue -> short-circuit with MissingAgentBrief.
    // We choose this path over a real claim because it avoids the workspace
    // clone while still proving the sweep already happened.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/16/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let config = config_for(&mock.uri());
    let mut log = Cursor::new(Vec::new());
    let outcome = run_once(&client, &config, &mut log, None).await;
    match outcome {
        Err(RunError::MissingAgentBrief(16)) => {}
        other => panic!(
            "expected MissingAgentBrief(16) (sweep must have completed before brief fetch), got {other:?}",
        ),
    }
    // mock drops at end of scope -> verifies expect(1) on the DELETE.
}

#[tokio::test]
async fn run_once_returns_blocked_when_stale_branch_deletion_fails() {
    // Brief AC: "On `Err`, return `RunOutcome::Blocked` with the failing
    // branch name in the block reason." The 403 case is the canonical
    // failure mode — branch protection or PAT scope refuses the DELETE.
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
                "number": 16,
                "title": "Pre-claim sweep target",
                "created_at": "2026-05-12T10:00:00Z",
                "labels": [{ "name": "ready-for-agent" }]
            }
        ])))
        .mount(&mock)
        .await;

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
    let config = config_for(&mock.uri());
    let mut log = Cursor::new(Vec::new());
    let outcome = run_once(&client, &config, &mut log, None)
        .await
        .expect("run_once should map DELETE failure to RunOutcome::Blocked, not Err");
    match outcome {
        RunOutcome::Blocked {
            reason: BlockReason::StaleAgentBranchDeletionFailed { branch, .. },
        } => {
            assert_eq!(branch, "agent/16-protected", "block reason must name the failing branch");
        }
        other => panic!(
            "expected Blocked(StaleAgentBranchDeletionFailed for agent/16-protected), got {other:?}",
        ),
    }
}

#[tokio::test]
async fn run_once_does_not_sweep_stale_branches_when_blocked_by_open_pr() {
    // Slice-b precedence AC: "if any `agent/*` PR is open anywhere, the
    // new check is not consulted (bellows is already blocked at the
    // slice-b layer)." Wiremock would 404 a matching-refs request because
    // we don't mount that endpoint here; if run_once issued it, the
    // outcome shape would change. The Blocked(OpenAgentPrs) outcome
    // proves the sweep was skipped.
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/pulls"))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "number": 41, "head": { "ref": "agent/41-foo" }, "draft": false }
        ])))
        .mount(&mock)
        .await;

    // matching-refs is intentionally NOT mocked: wiremock will 404, which
    // would surface in run_once as an error if the sweep were called.

    let client = octocrab_pointed_at(mock.uri());
    let config = config_for(&mock.uri());
    let mut log = Cursor::new(Vec::new());
    let outcome = run_once(&client, &config, &mut log, None)
        .await
        .expect("run_once should succeed via the slice-b Blocked path");
    match outcome {
        RunOutcome::Blocked {
            reason: BlockReason::OpenAgentPrs { pr_numbers },
        } => {
            assert_eq!(pr_numbers, vec![41]);
        }
        other => panic!(
            "expected Blocked(OpenAgentPrs[41]) — sweep must not fire when slice-b already blocked, got {other:?}",
        ),
    }
}

#[tokio::test]
async fn run_once_logs_sweep_summary_when_deletions_happen() {
    // Brief AC: "Successful deletions emit a one-line summary log
    // (`bellows: pre-claim swept N stale agent/<N>-* branch(es) before
    // claiming issue #<N>`) once per claim, immediately before the
    // existing `claimed issue #<N>` line." We drive run_once via the
    // MissingAgentBrief short-circuit; the summary log fires after the
    // sweep completes, so we can assert it from the captured log buffer
    // even though we never reach the workspace-clone step.
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
                "number": 16,
                "title": "Pre-claim sweep target",
                "created_at": "2026-05-12T10:00:00Z",
                "labels": [{ "name": "ready-for-agent" }]
            }
        ])))
        .mount(&mock)
        .await;

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

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/16/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let config = config_for(&mock.uri());
    let mut log = Cursor::new(Vec::new());
    let _ = run_once(&client, &config, &mut log, None).await;
    let log_str = String::from_utf8(log.into_inner()).expect("log is utf-8");
    assert!(
        log_str.contains("bellows: pre-claim swept 2 stale agent/16-* branch(es) before claiming issue #16"),
        "expected the brief's exemplar summary line, got: {log_str}",
    );
}

#[tokio::test]
async fn run_once_does_not_log_sweep_summary_when_no_branches_deleted() {
    // Brief AC: "Successful deletions emit a one-line summary log ...
    // once per claim". Zero deletions is the steady-state case and must
    // stay silent — otherwise every clean tick spams the log with
    // "swept 0 branches".
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
                "number": 16,
                "title": "Clean run",
                "created_at": "2026-05-12T10:00:00Z",
                "labels": [{ "name": "ready-for-agent" }]
            }
        ])))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/git/matching-refs/heads/agent/16-"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/16/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let config = config_for(&mock.uri());
    let mut log = Cursor::new(Vec::new());
    let _ = run_once(&client, &config, &mut log, None).await;
    let log_str = String::from_utf8(log.into_inner()).expect("log is utf-8");
    assert!(
        !log_str.contains("pre-claim swept"),
        "zero-deletion ticks must stay silent, got: {log_str}",
    );
}

// ---- Issue #85: agent-notes.md must be ephemeral to the pipeline run.
//      The file should NOT appear on the agent branch's pushed end-state
//      (so squash-merge to master can never inherit stale notes), and
//      its content should be surfaced via a PR comment so the operator
//      still sees what the agent flagged. ----

#[tokio::test]
async fn agent_notes_when_present_post_pr_comment_and_do_not_persist_in_pushed_tree() {
    // Issue #85 AC #5: pipeline run with the agent writing `agent-notes.md`
    // produces (a) a PR comment containing the content AND (b) a final push
    // that does not include the file in its tree.
    //
    // Drives both helpers end-to-end against a real git workspace + wiremock
    // GitHub: simulate an intermediate phase that commits + pushes
    // `agent-notes.md` as part of normal phase work, then run the finalise
    // helpers (capture+remove the file, push the deletion, post the PR
    // comment) and assert both outcomes on the remote (tree clean) and on
    // wiremock (POST received with the content embedded).
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/marad2001/test-repo/issues/77/comments"))
        .and(body_string_contains("## Agent notes"))
        .and(body_string_contains("stuck on the brief"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .expect(1)
        .mount(&mock)
        .await;

    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = workspace::prepare(&remote_url, "agent/77-ephemeral-notes")
        .await
        .expect("workspace::prepare should succeed");

    // Simulate an intermediate pipeline phase: agent writes agent-notes.md,
    // bellows's per-phase commit_all+push commits it onto the agent branch.
    // After this, the remote's tree for agent/77-ephemeral-notes contains
    // the file — this is exactly the leak the slice fixes.
    tokio::fs::write(
        workspace.path().join("agent-notes.md"),
        "stuck on the brief: refactor scope unclear\n",
    )
    .await
    .unwrap();
    workspace::commit_all(&workspace)
        .await
        .expect("intermediate commit should succeed");
    workspace::push_branch(&workspace)
        .await
        .expect("intermediate push should succeed");

    // Sanity: the remote currently has agent-notes.md in the agent branch's
    // tree (this is the pre-fix leak state).
    let pre_tree = Command::new("git")
        .args(["ls-tree", "--name-only", "agent/77-ephemeral-notes"])
        .current_dir(remote_dir.path())
        .output()
        .unwrap();
    let pre_tree_str = String::from_utf8(pre_tree.stdout).unwrap();
    assert!(
        pre_tree_str.contains("agent-notes.md"),
        "test precondition: pre-finalise pushed tree should still contain the file (pre-fix leak state); got: {pre_tree_str}",
    );

    // Finalise step under test: capture the content (for classify_exit +
    // PR comment), remove the file from the workspace.
    let agent_notes = capture_and_remove_agent_notes(&workspace)
        .await
        .expect("capture_and_remove_agent_notes should succeed");
    assert_eq!(
        agent_notes.as_deref(),
        Some("stuck on the brief: refactor scope unclear\n"),
        "captured content should preserve the raw file body for byte-span provenance",
    );
    assert!(
        !workspace.path().join("agent-notes.md").exists(),
        "the file must be removed from the workspace after capture",
    );

    // Commit + push the deletion so the agent branch's pushed tree no
    // longer contains the file.
    let head_before = workspace::head_sha(&workspace).await.unwrap();
    workspace::commit_all_and_push_if_advanced(&workspace, &head_before)
        .await
        .expect("final commit+push of the deletion should succeed");

    // Post the PR comment with the captured content.
    let client = octocrab_pointed_at(mock.uri());
    post_agent_notes_comment_if_present(
        &client,
        "marad2001",
        "test-repo",
        77,
        agent_notes.as_deref(),
    )
    .await
    .expect("post_agent_notes_comment_if_present should succeed");

    // Post-condition (AC #1): the pushed tree no longer contains the file.
    let post_tree = Command::new("git")
        .args(["ls-tree", "--name-only", "agent/77-ephemeral-notes"])
        .current_dir(remote_dir.path())
        .output()
        .unwrap();
    let post_tree_str = String::from_utf8(post_tree.stdout).unwrap();
    assert!(
        !post_tree_str.contains("agent-notes.md"),
        "agent branch's pushed tree must not contain agent-notes.md after finalise; got: {post_tree_str}",
    );
    // mock.verify() at scope-drop enforces expect(1) on the comments POST —
    // AC #2: the captured content was surfaced as a PR comment.
}

#[tokio::test]
async fn no_agent_notes_means_no_pr_comment_is_posted() {
    // Issue #85 AC #6: a bellows pipeline run where the agent does NOT write
    // `agent-notes.md` continues to work unchanged — no spurious
    // agent-notes-related PR comment.
    //
    // `expect(0)` on the comments endpoint pins the contract: if the helper
    // posts a comment despite `agent_notes` being None, the mock-server
    // drop verifies the violation and the test fails.
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/marad2001/test-repo/issues/77/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .expect(0)
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    post_agent_notes_comment_if_present(&client, "marad2001", "test-repo", 77, None)
        .await
        .expect("post_agent_notes_comment_if_present with None should succeed");
    // mock.verify() at scope-drop enforces expect(0): NO POST happened.
}
