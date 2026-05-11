//! Integration tests for the GitHub Actions auto-merge workflow.
//!
//! Issue #43 adds a `.github/workflows/auto-merge.yml` workflow that
//! squash-merges bellows-authored PRs (head ref starts with `agent/`,
//! non-draft, targeting the default branch) once `ci.yml` passes — closing
//! the bellows-on-bellows merge loop so the operator does not have to
//! click "merge" by hand. These tests pin the load-bearing content of
//! that workflow file so a future drive-by edit cannot silently break
//! the safety filters (the head-prefix gate, the draft gate, the
//! permissions block) without flipping a red light here.
//!
//! The workflow itself is a YAML file rather than Rust; these tests
//! treat it as a textual contract and assert on substrings the same
//! way [`tests/ci.rs`] pins `ci.yml`'s load-bearing args.

use std::fs;
use std::path::PathBuf;

fn workflow_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".github")
        .join("workflows")
        .join("auto-merge.yml")
}

fn read_workflow() -> String {
    fs::read_to_string(workflow_path()).unwrap_or_else(|e| {
        panic!(
            "auto-merge workflow must exist at .github/workflows/auto-merge.yml: {}",
            e
        )
    })
}

fn assert_contains_all(body: &str, needles: &[&str], context: &str) {
    let mut missing = Vec::new();
    for needle in needles {
        if !body.contains(needle) {
            missing.push(*needle);
        }
    }
    assert!(
        missing.is_empty(),
        "auto-merge workflow is missing {} in section {:?}: {:?}",
        if missing.len() == 1 { "value" } else { "values" },
        context,
        missing,
    );
}

#[test]
fn auto_merge_workflow_file_exists_at_canonical_path() {
    let path = workflow_path();
    assert!(
        path.is_file(),
        "auto-merge workflow must exist at {} (GitHub Actions only picks \
         up workflows under .github/workflows/)",
        path.display(),
    );
    let body = read_workflow();
    assert!(
        !body.trim().is_empty(),
        "auto-merge workflow must not be empty",
    );
}

#[test]
fn auto_merge_workflow_triggers_on_ci_workflow_run_completion() {
    let body = read_workflow();
    // Brief / ADR-0001: the workflow_run trigger watching the `CI`
    // workflow IS the CI gate. The auto-merge workflow MUST fire from
    // a completed `CI` run — not from `pull_request` (would merge
    // before CI starts) and not from `pull_request_target` (which
    // ADR-0001 explicitly rules out for supply-chain safety). The
    // `types: [completed]` filter narrows the event to terminal CI
    // outcomes so the job runs once per CI run, not on every status
    // tick.
    assert_contains_all(
        &body,
        &["workflow_run", "workflows:", "CI", "types:", "completed"],
        "trigger / workflow_run on CI",
    );
}

/// Extract the top-level `permissions:` block — its `scope: level`
/// key-value pairs — from the workflow body. Returns the parsed pairs
/// in the order they appear.
///
/// Deliberately a tiny YAML-shaped parser rather than a `serde_yaml`
/// dependency: the workflow has a single top-level `permissions:`
/// block, which is all we need to inspect to assert the "exactly
/// these scopes" invariant.
fn extract_permissions_block(body: &str) -> Vec<(String, String)> {
    let mut in_block = false;
    let mut entries = Vec::new();
    for line in body.lines() {
        if !in_block {
            // Only match the TOP-LEVEL `permissions:` key (column 0,
            // nothing after the colon). A job-level `permissions:` is
            // indented and so will not match.
            if line == "permissions:" {
                in_block = true;
            }
            continue;
        }
        let is_indented = line.starts_with(' ') || line.starts_with('\t');
        if !is_indented {
            if line.trim().is_empty() {
                continue;
            }
            // Reached the next top-level key — block has ended.
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            entries.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    entries
}

#[test]
fn auto_merge_workflow_permissions_block_is_exactly_pull_requests_and_contents_write() {
    let body = read_workflow();
    // Brief acceptance criterion: the workflow's `permissions:` block
    // must request ONLY `pull-requests: write` and `contents: write`
    // — no other scopes. Using the default `GITHUB_TOKEN` means
    // whatever the workflow asks for is the blast radius of a
    // compromise; over-scoping (e.g. `actions: write`, `issues:
    // write`) would let a future bug do more damage than the slice
    // needs. The test asserts on the *set* of declared scopes, not
    // just substrings, so adding an extra scope flips this test red
    // even if the original two are still present.
    let perms = extract_permissions_block(&body);
    assert!(
        !perms.is_empty(),
        "auto-merge workflow MUST declare a top-level `permissions:` \
         block (so it does not inherit the repo default scopes). \
         Got:\n{}",
        body,
    );
    let mut scopes: Vec<(String, String)> = perms
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    scopes.sort();
    let expected: Vec<(String, String)> = vec![
        ("contents".to_string(), "write".to_string()),
        ("pull-requests".to_string(), "write".to_string()),
    ];
    assert_eq!(
        scopes, expected,
        "auto-merge workflow's `permissions:` block must be EXACTLY \
         `pull-requests: write` and `contents: write` — no extra \
         scopes (over-scoping widens the blast radius of the default \
         GITHUB_TOKEN). Got: {:?}",
        scopes,
    );
}

#[test]
fn auto_merge_workflow_filters_to_agent_branches_only() {
    let body = read_workflow();
    // Brief acceptance criterion: a human-authored PR on a non-`agent/*`
    // branch MUST NOT auto-merge. The bellows branch convention
    // (enforced by `agent_branch_name`) is that bellows-authored PRs
    // have `head.ref` starting with `agent/`; the workflow's filter
    // must reference both `head.ref` (so the filter is read from the
    // PR object, not e.g. the workflow_run's head_branch alone, which
    // can be misleading) and the literal `agent/` prefix string.
    //
    // We assert both substrings appear *somewhere* in the workflow
    // — i.e. that the filter is present at all. The dedicated
    // "skip if not starting with agent/" semantics are inherently
    // about runtime behavior; the test pins the load-bearing literals
    // so a future drop of the prefix filter shows up as a red light.
    assert_contains_all(
        &body,
        &["head.ref", "agent/"],
        "filter / head.ref agent prefix",
    );
}

#[test]
fn auto_merge_workflow_filters_fork_prs_by_head_repo_equality() {
    let body = read_workflow();
    // Important finding (review of issue #43): the original filter set
    // (head.ref prefix, draft, state, base.ref) did NOT include a
    // head-repo equality check. An external contributor could fork the
    // repo, push a branch named `agent/exploit`, open a PR targeting
    // the default branch, get CI green on innocuous-looking code, and
    // have this workflow squash-merge it with the base repo's
    // GITHUB_TOKEN — bypassing review entirely. ADR-0001 rejects
    // `pull_request_target` for supply-chain reasons, but the merge
    // step here is the supply-chain risk regardless of how the
    // workflow itself executes.
    //
    // Bellows only ever pushes to `origin` (`workspace::push_branch`),
    // so legitimate bellows PRs are never from a fork; the fork gate
    // is pure defence with no false-positive cost.
    //
    // Pin both `head.repo` (so the filter is read off the PR object,
    // not some derived flag) and `full_name` (the unambiguous
    // `owner/repo` identifier — comparing by id would also work but
    // `full_name` is the existing GitHub-API convention and matches
    // the suggestion in the review finding). A future drive-by edit
    // that drops the fork gate flips this test red.
    assert_contains_all(
        &body,
        &["head.repo", "full_name", "base.repo"],
        "filter / fork PR head.repo equality",
    );
}

#[test]
fn auto_merge_workflow_filters_drafts_open_and_default_branch() {
    let body = read_workflow();
    // Brief acceptance criteria, three filters:
    //
    //  (a) draft PRs must NOT auto-merge — bellows opens drafts on
    //      non-Success runs and those sit open for operator review;
    //  (b) state must be `open` — closed/merged PRs are not candidates;
    //  (c) base.ref must equal the repo's default branch — a PR
    //      retargeted to a feature branch must not auto-merge to it.
    //
    // We assert the load-bearing literals (`draft`, `state`, `base.ref`,
    // `default_branch`) appear so a future drop of one of the filters
    // shows up as a red light.
    assert_contains_all(
        &body,
        &["draft", "state", "base.ref", "default_branch"],
        "filter / draft+state+default-branch",
    );
}

#[test]
fn auto_merge_workflow_calls_merge_api_with_squash_method() {
    let body = read_workflow();
    // Brief: the merge call MUST use `merge_method: 'squash'` (the
    // existing bellows convention; rebase / merge-commit options are
    // explicitly out of scope). The merge happens via the GitHub REST
    // call `PUT /repos/{owner}/{repo}/pulls/{pull_number}/merge`,
    // which the github-script wrapper exposes as
    // `github.rest.pulls.merge({..., merge_method: 'squash'})`.
    // Pin both the call site and the literal squash method.
    assert_contains_all(
        &body,
        &["pulls.merge", "merge_method", "squash"],
        "merge / squash call",
    );
}

#[test]
fn auto_merge_workflow_falls_back_to_head_sha_lookup_for_empty_pull_requests() {
    let body = read_workflow();
    // Brief: the `workflow_run` event's `pull_requests` array can be
    // empty (e.g. for PRs from forks or PRs whose head SHA the event
    // payload does not surface). The workflow must fall back to
    // looking up open PRs by head SHA via the search API in that
    // case, otherwise some legitimate bellows PRs would be skipped.
    // Pin the load-bearing references: the `head_sha` field on the
    // event, and the search call shape.
    assert_contains_all(
        &body,
        &["pull_requests", "head_sha", "search.issuesAndPullRequests"],
        "fallback / head-SHA lookup",
    );
}

#[test]
fn auto_merge_workflow_does_not_use_pull_request_target() {
    let body = read_workflow();
    // ADR-0001 explicitly rules out `pull_request_target` for this
    // workflow on supply-chain grounds: `pull_request_target` runs in
    // the context of the PR's `head` ref with write permissions, which
    // a malicious PR could exploit. `workflow_run` runs in the context
    // of the default branch's workflow definition, so the PR cannot
    // alter what the workflow does. Pin this so a future drive-by
    // edit cannot "make it work for forks" by switching triggers.
    assert!(
        !body.contains("pull_request_target"),
        "auto-merge workflow MUST NOT use `pull_request_target` \
         (ADR-0001: supply-chain risk — a malicious PR's branch \
         contents would run with write scopes). Use `workflow_run` \
         instead.\nGot:\n{}",
        body,
    );
}

#[test]
fn auto_merge_workflow_early_exits_when_ci_did_not_succeed() {
    let body = read_workflow();
    // Brief acceptance criterion: a PR where ci.yml failed must NOT
    // auto-merge. The `workflow_run` event fires regardless of CI's
    // outcome (success / failure / cancelled / timed_out), so the
    // job MUST gate on `github.event.workflow_run.conclusion ==
    // 'success'`. Without this gate, the workflow would happily merge
    // PRs whose CI just turned red — defeating the slice. Pin the
    // exact expression so a future drive-by edit cannot weaken the
    // gate (e.g. to `!= 'failure'`, which would let `cancelled`
    // through).
    assert!(
        body.contains("github.event.workflow_run.conclusion == 'success'"),
        "auto-merge workflow MUST gate on `github.event.workflow_run.\
         conclusion == 'success'` (the CI-passed signal). Without this, \
         CI failures would still auto-merge. Got:\n{}",
        body,
    );
}
