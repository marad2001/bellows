use std::path::Path;
use std::process::Command;

use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use bellows::workspace::{commit_marker, open_pr, prepare, push_branch};

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
}

#[tokio::test]
async fn commit_marker_writes_file_and_commits_to_agent_branch() {
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    let workspace = prepare(&remote_url, "agent/42-fix-the-foo-bug")
        .await
        .unwrap();

    let marker_content = "issue=42 timestamp=2026-05-09T13:00:00Z";
    commit_marker(&workspace, marker_content)
        .await
        .expect("commit_marker should succeed");

    let marker_path = workspace.path().join(".bellows-stub-marker");
    assert!(marker_path.exists(), "marker file should exist");
    let on_disk = std::fs::read_to_string(&marker_path).unwrap();
    assert_eq!(on_disk.trim(), marker_content);

    let log = Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(workspace.path())
        .output()
        .unwrap();
    let log_text = String::from_utf8(log.stdout).unwrap();
    assert_eq!(
        log_text.lines().count(),
        2,
        "expected initial commit + marker commit, got: {}",
        log_text
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
    commit_marker(&workspace, "issue=42 timestamp=2026-05-09T13:00:00Z")
        .await
        .unwrap();

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
async fn open_pr_posts_a_non_draft_pull_request_against_the_default_branch() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(wm_path("/repos/marad2001/test-repo/pulls"))
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
    )
    .await
    .expect("open_pr should succeed");

    assert_eq!(pr.number, 99);
    assert_eq!(
        pr.html_url,
        "https://github.com/marad2001/test-repo/pull/99"
    );
}
