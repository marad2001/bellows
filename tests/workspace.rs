use std::path::Path;
use std::process::Command;

use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use bellows::workspace::{commit_all, open_pr, prepare, push_branch};

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

    // Simulate what a containerised stub agent (or future Claude agent) would
    // produce: arbitrary files in the workspace, including a nested directory.
    std::fs::write(workspace.path().join(".bellows-stub-marker"), "marker").unwrap();
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
    assert!(names_text.contains(".bellows-stub-marker"), "log: {}", names_text);
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
async fn push_branch_pushes_agent_branch_to_remote() {
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    let workspace = prepare(&remote_url, "agent/42-fix-the-foo-bug")
        .await
        .unwrap();
    std::fs::write(
        workspace.path().join(".bellows-stub-marker"),
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
        "marad2001",
        "test-repo",
        "agent/42-fix-the-foo-bug",
        "master",
        "Bellows stub run for issue #42",
        "Closes #42.",
        false,
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
        "marad2001",
        "test-repo",
        "agent/8-cargo-test-failed",
        "main",
        "Bellows agent run for issue #8",
        "Closes #8. Final tests red.",
        true,
    )
    .await
    .expect("open_pr should succeed");

    assert_eq!(pr.number, 42);
}
