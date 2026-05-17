//! Integration tests for `runner::run_once`'s pre-claim container gate
//! (issue #126 / ADR-0009 slice 4). The old pre-claim PR-open gate
//! (issue #42) is removed: an open `agent/*` PR no longer blocks the
//! polling tick. The new gate is a container-presence check — if any
//! `bellows-managed=true` container is running, the tick is `Blocked`
//! with `BlockReason::AgentContainerRunning`; otherwise it proceeds.
//!
//! These tests drive `run_once` with an injected `AgentContainerProbe`
//! so we don't need a real Docker daemon, and short-circuit at
//! `MissingAgentBrief(N)` to prove the runner reached `find_next_issue`
//! without touching the workspace or sandbox.

use std::future::Future;
use std::io::Cursor;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bellows::config::Config;
use bellows::runner::{
    run_once, AgentContainerProbe, BlockReason, ProbeError, RunError, RunOutcome,
    RunningAgentContainer,
};
use chrono::{DateTime, TimeZone, Utc};
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

/// Test double for the production Docker-backed probe. Always reports
/// "no agent container running" so the gate clears.
struct NoContainerProbe;

impl AgentContainerProbe for NoContainerProbe {
    fn detect<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<RunningAgentContainer>, ProbeError>> + Send + 'a>>
    {
        Box::pin(async { Ok(None) })
    }
}

/// Test double that reports a specific running container.
struct StaticContainerProbe {
    container: RunningAgentContainer,
    calls: Arc<AtomicUsize>,
}

impl AgentContainerProbe for StaticContainerProbe {
    fn detect<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<RunningAgentContainer>, ProbeError>> + Send + 'a>>
    {
        let c = self.container.clone();
        let calls = Arc::clone(&self.calls);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(c))
        })
    }
}

// ---- AC1: open agent PRs no longer block the polling tick. ----

#[tokio::test]
async fn run_once_no_longer_blocks_when_an_open_agent_pr_exists() {
    // Issue #126 / ADR-0009: the pre-claim PR-open gate is dropped.
    // An open `agent/*` PR must NOT cause `run_once` to return
    // `Blocked` — that proxy for the concurrency=1 invariant is
    // replaced by a direct container-presence check (driven by the
    // injected probe below, which reports "no container running").
    //
    // We do NOT mount a `/pulls` mock: under the old code, the
    // pre-claim list_open_agent_prs call would 404 against wiremock
    // and surface as `Blocked` (the fail-closed path). Under the new
    // code, run_once must never make that call; it must go straight
    // to `find_next_issue` → stale-branch sweep → brief fetch and
    // short-circuit at `MissingAgentBrief(42)` because we don't
    // mount a brief comment.
    let mock = MockServer::start().await;

    // The runner should hit the issues endpoint with the
    // ready-for-agent label filter and pick #42.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues"))
        .and(query_param("labels", "ready-for-agent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 42,
                "title": "post-gate-drop pickup",
                "created_at": "2026-05-12T10:00:00Z",
                "labels": [{ "name": "ready-for-agent" }]
            }
        ])))
        .mount(&mock)
        .await;

    // Stale-branch sweep returns no stale refs.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/git/matching-refs/heads/agent/42-"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    // Brief comment list empty → MissingAgentBrief short-circuit
    // proves we got past every pre-claim check.
    Mock::given(method("GET"))
        .and(path("/repos/marad2001/test-repo/issues/42/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock)
        .await;

    let client = octocrab_pointed_at(mock.uri());
    let config = config_for(&mock.uri());
    let mut log = Cursor::new(Vec::new());
    let probe = NoContainerProbe;

    let outcome = run_once(&client, &config, &mut log, None, Some(&probe)).await;

    match outcome {
        Err(RunError::MissingAgentBrief(n)) => assert_eq!(
            n, 42,
            "with the open-PR gate dropped and no container running, run_once should reach the brief fetch for issue #42",
        ),
        other => panic!(
            "expected MissingAgentBrief(42) (no pre-claim block), got {other:?}",
        ),
    }
}

// ---- AC2 / AC4: when a bellows-* container is running, `run_once`
//      returns Blocked(AgentContainerRunning { container_id,
//      started_at }) and does NOT call find_next_issue. ----

#[tokio::test]
async fn run_once_returns_blocked_when_agent_container_is_running() {
    // Brief AC: "a new test where a `bellows-*` container is detected
    // returns `Blocked(AgentContainerRunning { ... })` and does NOT
    // call `find_next_issue`." We deliberately mount NO mock for
    // /issues or /pulls — if `run_once` called either, wiremock would
    // 404 and the outcome shape would change.
    let mock = MockServer::start().await;

    let started_at: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 5, 17, 10, 30, 0).unwrap();
    let probe = StaticContainerProbe {
        container: RunningAgentContainer {
            container_id: "abc123def456".to_string(),
            started_at,
        },
        calls: Arc::new(AtomicUsize::new(0)),
    };

    let client = octocrab_pointed_at(mock.uri());
    let config = config_for(&mock.uri());
    let mut log = Cursor::new(Vec::new());

    let outcome = run_once(&client, &config, &mut log, None, Some(&probe))
        .await
        .expect("run_once should succeed with the container-presence Blocked path");

    match outcome {
        RunOutcome::Blocked {
            reason:
                BlockReason::AgentContainerRunning {
                    container_id,
                    started_at: sa,
                },
        } => {
            assert_eq!(container_id, "abc123def456");
            assert_eq!(sa, started_at);
        }
        other => panic!(
            "expected Blocked(AgentContainerRunning {{ container_id: \"abc123def456\", .. }}), got {other:?}",
        ),
    }

    // mock had no expectations; the verifying property is the
    // outcome shape — if `find_next_issue` had been called, the
    // /issues GET would 404 and run_once would surface Err
    // (octocrab) instead of the Blocked variant we asserted on.
}

// ---- AC3: container-presence check is GLOBAL across the polling
//      loop. A container running for repo A blocks claim attempts on
//      repo B. ----

#[tokio::test]
async fn run_once_container_gate_blocks_globally_across_all_repos() {
    // Brief AC: "Container-presence check is GLOBAL across the
    // polling loop (not per-repo). A container running for repo A
    // blocks claim attempts on repo B. Verified by a test." We
    // configure TWO repos and pin a running container; neither
    // repo's /issues endpoint must be called.
    let mock = MockServer::start().await;

    let started_at: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 5, 17, 11, 0, 0).unwrap();
    let probe = StaticContainerProbe {
        container: RunningAgentContainer {
            container_id: "global-block-id".to_string(),
            started_at,
        },
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let calls = Arc::clone(&probe.calls);

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

    let outcome = run_once(&client, &config, &mut log, None, Some(&probe))
        .await
        .expect("run_once should succeed with the global container-presence Blocked path");

    match outcome {
        RunOutcome::Blocked {
            reason: BlockReason::AgentContainerRunning { container_id, .. },
        } => {
            assert_eq!(container_id, "global-block-id");
        }
        other => panic!(
            "expected Blocked(AgentContainerRunning) for the multi-repo case, got {other:?}",
        ),
    }

    // The probe should be consulted exactly once for the whole tick
    // — the gate is global, not per-repo. Two repos × one probe-per-
    // repo would surface here as `calls == 2`.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "container probe must be consulted ONCE per tick (global), not per-repo",
    );
}
