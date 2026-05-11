use std::path::Path;
use std::process::Command;

use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use bellows::workspace::{
    commit_all, compute_diff_against_base, diff_between_touches_only_agent_notes, head_sha,
    open_pr, prepare, push_branch, OpenPrRequest,
};

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

fn current_branch(repo: &Path) -> String {
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(repo)
        .output()
        .expect("git branch");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[tokio::test]
async fn prepare_clones_remote_into_tempdir_and_creates_agent_branch() {
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());

    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = prepare(&remote_url, "agent/42-fix-the-foo-bug")
        .await
        .expect("prepare should succeed");

    assert!(workspace.path().exists(), "workspace path should exist");
    assert!(workspace.path().join(".git").exists(), "should be a git repo");
    assert_eq!(current_branch(workspace.path()), "agent/42-fix-the-foo-bug");
    assert_eq!(workspace.branch_name(), "agent/42-fix-the-foo-bug");
    assert_eq!(workspace.default_branch(), "master");
}

#[tokio::test]
async fn commit_all_stages_and_commits_arbitrary_workspace_changes() {
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    let workspace = prepare(&remote_url, "agent/42-fix-the-foo-bug")
        .await
        .unwrap();

    // Simulate what a containerised agent would produce: arbitrary files
    // in the workspace, including a nested directory. Filenames must not
    // match Bellows-managed local exclusions (`.bellows-*`, `target/`,
    // etc.) — those represent internal handoff state we deliberately
    // keep out of commits.
    std::fs::write(workspace.path().join("agent-output.txt"), "marker").unwrap();
    std::fs::write(workspace.path().join("hello.txt"), "world").unwrap();
    std::fs::create_dir(workspace.path().join("subdir")).unwrap();
    std::fs::write(workspace.path().join("subdir").join("nested.md"), "x").unwrap();

    commit_all(&workspace).await.expect("commit_all should succeed");

    let names = Command::new("git")
        .args(["log", "-1", "--name-only", "--format="])
        .current_dir(workspace.path())
        .output()
        .unwrap();
    let names_text = String::from_utf8(names.stdout).unwrap();
    assert!(names_text.contains("agent-output.txt"), "log: {}", names_text);
    assert!(names_text.contains("hello.txt"), "log: {}", names_text);
    assert!(names_text.contains("subdir/nested.md"), "log: {}", names_text);

    let oneline = Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(workspace.path())
        .output()
        .unwrap();
    let oneline_text = String::from_utf8(oneline.stdout).unwrap();
    assert_eq!(
        oneline_text.lines().count(),
        2,
        "expected initial + commit_all, got: {}",
        oneline_text
    );
}

#[tokio::test]
async fn head_sha_returns_a_full_sha_that_matches_git_rev_parse_head() {
    // Cycle 1 tracer bullet: the new public-API surface exists and the
    // git rev-parse HEAD shellout returns the same string the workspace
    // helper does. Used by the slice-9.6 per-finding loop to detect
    // whether HEAD advanced across an invocation (cf. brief acceptance
    // criterion: commit_landed=false when HEAD did not advance).
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    let workspace = prepare(&remote_url, "agent/42-fix-the-foo-bug")
        .await
        .unwrap();

    let sha = head_sha(&workspace).await.expect("head_sha should succeed");

    let expected = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace.path())
        .output()
        .unwrap();
    let expected_sha = String::from_utf8(expected.stdout).unwrap().trim().to_string();
    assert_eq!(sha, expected_sha);
    // Sanity: full-length SHA (not abbreviated).
    assert_eq!(sha.len(), 40, "expected full 40-char sha, got {:?}", sha);
}

#[tokio::test]
async fn diff_between_returns_true_when_only_agent_notes_changed_between_refs() {
    // Cycle 2: the "agent only edited notes" case — the positive
    // outcome the helper exists to detect. The agent did NOT commit a
    // code fix; bellows committed an agent-notes-only commit on its
    // behalf. The runner must see commit_landed=false so the
    // verbatim-title check in compute_coverage_violations runs.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    let workspace = prepare(&remote_url, "agent/42-fix-the-foo-bug")
        .await
        .unwrap();

    let base = head_sha(&workspace).await.unwrap();

    std::fs::write(workspace.path().join("agent-notes.md"), "stuck\n").unwrap();
    commit_all(&workspace).await.unwrap();
    let head = head_sha(&workspace).await.unwrap();

    let only_notes = diff_between_touches_only_agent_notes(&workspace, &base, &head)
        .await
        .expect("helper should succeed");
    assert!(
        only_notes,
        "single agent-notes-only commit must be reported as notes-only"
    );
}

#[tokio::test]
async fn diff_between_returns_false_when_only_code_files_changed_across_two_commits() {
    // Cycle 3: the "agent committed real fixes" case — the most common
    // true-negative. Two code commits between base and head; no
    // agent-notes.md touched. commit_landed must be true at the call
    // site (the helper returns false → !helper → true). Models the
    // PR #38 / issue #36 scenario where the agent self-committed a
    // code fix under its own commit message.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    let workspace = prepare(&remote_url, "agent/42-fix-the-foo-bug")
        .await
        .unwrap();

    let base = head_sha(&workspace).await.unwrap();

    std::fs::write(workspace.path().join("src.rs"), "fn a() {}\n").unwrap();
    commit_all(&workspace).await.unwrap();
    std::fs::write(workspace.path().join("other.rs"), "fn b() {}\n").unwrap();
    commit_all(&workspace).await.unwrap();
    let head = head_sha(&workspace).await.unwrap();

    let only_notes = diff_between_touches_only_agent_notes(&workspace, &base, &head)
        .await
        .unwrap();
    assert!(
        !only_notes,
        "two code-only commits must NOT be reported as notes-only"
    );
}

#[tokio::test]
async fn diff_between_returns_false_when_mixed_code_and_agent_notes_commits_exist() {
    // Cycle 4: code commit dominates. A HEAD~1..HEAD inspection (as the
    // PR #37 helper did) CANNOT represent this case; here the
    // agent-notes-only commit happens to be the most-recent one, but a
    // real code fix sat before it. The general-case helper must scan the
    // full base..head diff and report false — commit_landed=true at the
    // call site.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    let workspace = prepare(&remote_url, "agent/42-fix-the-foo-bug")
        .await
        .unwrap();

    let base = head_sha(&workspace).await.unwrap();

    std::fs::write(workspace.path().join("src.rs"), "fn a() {}\n").unwrap();
    commit_all(&workspace).await.unwrap();
    std::fs::write(workspace.path().join("agent-notes.md"), "explanation\n").unwrap();
    commit_all(&workspace).await.unwrap();
    let head = head_sha(&workspace).await.unwrap();

    let only_notes = diff_between_touches_only_agent_notes(&workspace, &base, &head)
        .await
        .unwrap();
    assert!(
        !only_notes,
        "mixed commits with a code commit must NOT be reported as notes-only"
    );
}

#[tokio::test]
async fn diff_between_returns_false_when_base_equals_head() {
    // Cycle 5: pin the contract for base == head (no advancement
    // between refs). Chosen contract: Ok(false) — the empty diff is
    // not exactly `["agent-notes.md"]`, so vacuously false. The
    // runner short-circuits on the head_after == head_before path
    // before calling this helper anyway; pinning Ok(false) here is
    // defensive consistency so a future caller that drops the
    // short-circuit still gets a sensible answer.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    let workspace = prepare(&remote_url, "agent/42-fix-the-foo-bug")
        .await
        .unwrap();

    let sha = head_sha(&workspace).await.unwrap();

    let only_notes = diff_between_touches_only_agent_notes(&workspace, &sha, &sha)
        .await
        .expect("base==head must NOT surface an error");
    assert!(
        !only_notes,
        "base==head is the empty diff; cannot be exactly [agent-notes.md]"
    );
}

#[tokio::test]
async fn compute_diff_against_base_returns_full_branch_diff_as_string() {
    // Slice 8: the weak-test guard scans `git diff <base>...HEAD` for
    // new Rust test attributes. It needs the diff as a String (rather
    // than written to a tempfile) so the scan can run synchronously in
    // the runner without an extra file IO round-trip. Pin the
    // round-trip here: a commit that adds a `#[test]` function appears
    // verbatim in the returned diff.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    let workspace = prepare(&remote_url, "agent/42-fix-the-foo-bug")
        .await
        .unwrap();

    std::fs::write(
        workspace.path().join("tests").join("new.rs"),
        "#[test]\nfn slice_8_smoke() {\n    assert_eq!(1, 1);\n}\n",
    )
    .or_else(|_| {
        std::fs::create_dir_all(workspace.path().join("tests"))?;
        std::fs::write(
            workspace.path().join("tests").join("new.rs"),
            "#[test]\nfn slice_8_smoke() {\n    assert_eq!(1, 1);\n}\n",
        )
    })
    .unwrap();
    commit_all(&workspace).await.unwrap();

    let diff = compute_diff_against_base(&workspace)
        .await
        .expect("compute_diff_against_base should succeed");

    assert!(
        diff.contains("#[test]"),
        "diff should contain the added test attribute line: {diff}"
    );
    assert!(
        diff.contains("slice_8_smoke"),
        "diff should contain the added function name: {diff}"
    );
}

#[tokio::test]
async fn compute_diff_against_base_returns_empty_string_when_branch_has_no_extra_commits() {
    // Edge case the runner guards against indirectly: a workspace
    // with no commits beyond the base branch produces an empty diff.
    // The weak-test guard's has_new_tests scan returns false on the
    // empty string (no `+` lines to inspect), which is the right
    // semantics — a no-op run has no new tests but also has no
    // implementation code, so the guard's gating in the runner
    // skips it on the halt path.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    let workspace = prepare(&remote_url, "agent/42-fix-the-foo-bug")
        .await
        .unwrap();

    let diff = compute_diff_against_base(&workspace)
        .await
        .expect("compute_diff_against_base should succeed");

    assert!(
        diff.trim().is_empty(),
        "branch at parity with base must produce empty diff, got: {diff:?}"
    );
}

#[tokio::test]
async fn push_branch_pushes_agent_branch_to_remote() {
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    let workspace = prepare(&remote_url, "agent/42-fix-the-foo-bug")
        .await
        .unwrap();
    std::fs::write(
        workspace.path().join("agent-output.txt"),
        "issue=42 timestamp=2026-05-09T13:00:00Z",
    )
    .unwrap();
    commit_all(&workspace).await.unwrap();

    push_branch(&workspace).await.expect("push should succeed");

    let output = Command::new("git")
        .args(["branch", "--list", "agent/42-fix-the-foo-bug"])
        .current_dir(remote_dir.path())
        .output()
        .unwrap();
    let branches = String::from_utf8(output.stdout).unwrap();
    assert!(
        branches.contains("agent/42-fix-the-foo-bug"),
        "expected agent branch on remote, got: {:?}",
        branches
    );
}

#[tokio::test]
async fn open_pr_with_draft_false_posts_a_regular_pull_request() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(wm_path("/repos/marad2001/test-repo/pulls"))
        .and(body_partial_json(json!({ "draft": false })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "number": 99,
            "html_url": "https://github.com/marad2001/test-repo/pull/99"
        })))
        .mount(&mock)
        .await;

    let client = octocrab::OctocrabBuilder::new()
        .base_uri(mock.uri())
        .expect("base uri")
        .build()
        .expect("octocrab");

    let pr = open_pr(
        &client,
        OpenPrRequest {
            owner: "marad2001",
            repo: "test-repo",
            head_branch: "agent/42-fix-the-foo-bug",
            base_branch: "master",
            title: "Bellows stub run for issue #42",
            body: "Closes #42.",
            draft: false,
        },
    )
    .await
    .expect("open_pr should succeed");

    assert_eq!(pr.number, 99);
    assert_eq!(
        pr.html_url,
        "https://github.com/marad2001/test-repo/pull/99"
    );
}

#[tokio::test]
async fn open_pr_with_draft_true_posts_a_draft_pull_request() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(wm_path("/repos/marad2001/test-repo/pulls"))
        .and(body_partial_json(json!({ "draft": true })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "number": 42,
            "html_url": "https://github.com/marad2001/test-repo/pull/42"
        })))
        .mount(&mock)
        .await;

    let client = octocrab::OctocrabBuilder::new()
        .base_uri(mock.uri())
        .expect("base uri")
        .build()
        .expect("octocrab");

    let pr = open_pr(
        &client,
        OpenPrRequest {
            owner: "marad2001",
            repo: "test-repo",
            head_branch: "agent/8-cargo-test-failed",
            base_branch: "main",
            title: "Bellows agent run for issue #8",
            body: "Closes #8. Final tests red.",
            draft: true,
        },
    )
    .await
    .expect("open_pr should succeed");

    assert_eq!(pr.number, 42);
}
