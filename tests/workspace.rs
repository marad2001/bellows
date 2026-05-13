use std::path::Path;
use std::process::Command;

use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use bellows::config::GatesConfig;
use bellows::workflow_parse::Provenance;
use bellows::workspace::{
    commit_all, commit_all_and_push_if_advanced, commit_to_branch, compute_diff_against_base,
    diff_between_touches_only_agent_notes, generate_commit_log, head_sha, open_pr, prepare,
    prepare_with_gates, push_branch, OpenPrRequest,
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

// ----------------------------------------------------------------------
// Issue #52: commit_all_and_push_if_advanced — the slice-9.6 four-corner
// commit/push pattern packaged as a reusable helper. The nit-batch
// invocation in runner.rs needs the same shape the per-finding loop
// already uses; rather than duplicate the dance, both call sites use
// this helper. Tests pin the four post-agent-invocation outcomes:
//   1. agent self-commit         → HEAD advanced; commit_all is a no-op;
//                                  we push.
//   2. bellows commit-on-behalf  → HEAD advances via commit_all; we push.
//   3. mixed (agent commit + leftover edits) → both happen, single push.
//   4. no advancement            → HEAD unchanged; we do NOT push.
// ----------------------------------------------------------------------

fn remote_has_branch(remote: &Path, branch: &str) -> bool {
    let output = Command::new("git")
        .args(["branch", "--list", branch])
        .current_dir(remote)
        .output()
        .expect("git branch --list");
    let stdout = String::from_utf8(output.stdout).unwrap();
    stdout.lines().any(|l| l.contains(branch))
}

fn remote_branch_sha(remote: &Path, branch: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", branch])
        .current_dir(remote)
        .output()
        .expect("git rev-parse on remote");
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).unwrap().trim().to_string())
}

#[tokio::test]
async fn commit_all_and_push_if_advanced_pushes_when_agent_self_committed() {
    // Slice for issue #52, corner 1: the agent self-committed its fix
    // inside the sandbox (HEAD advanced under the agent's own commit
    // message). Bellows's subsequent commit_all has nothing to stage and
    // returns NoChangesToCommit — the legacy `Ok(()) => push` shape lost
    // the commit. The helper detects HEAD advancement independently of
    // commit_all's return and pushes the agent commit to origin.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = prepare(&remote_url, "agent/52-nit-batch-self-commit")
        .await
        .unwrap();

    let head_before = head_sha(&workspace).await.unwrap();

    // Simulate an in-container agent self-commit: write a file AND
    // commit it ourselves, mimicking what `claude` would do when it
    // decides to `git commit` its own fix.
    std::fs::write(workspace.path().join("fixed.rs"), "fn fixed() {}\n").unwrap();
    run_git(workspace.path(), &["add", "fixed.rs"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "agent: drop derive(Default) for new_without_default"],
    );

    let head_after = commit_all_and_push_if_advanced(&workspace, &head_before)
        .await
        .expect("helper should succeed on agent self-commit");

    assert_ne!(
        head_after, head_before,
        "head_after must reflect the agent's self-commit, not the pre-invocation HEAD"
    );
    let local_head = head_sha(&workspace).await.unwrap();
    assert_eq!(
        head_after, local_head,
        "helper must return the post-commit local HEAD verbatim"
    );

    let remote_sha = remote_branch_sha(remote_dir.path(), "agent/52-nit-batch-self-commit")
        .expect("remote must hold the pushed branch");
    assert_eq!(
        remote_sha, head_after,
        "remote branch must point at the agent's self-commit, not the pre-invocation HEAD"
    );
}

#[tokio::test]
async fn commit_all_and_push_if_advanced_pushes_when_bellows_commits_on_behalf() {
    // Corner 2: agent left uncommitted edits in the workspace (the
    // historical Ok(())-on-commit_all case). The helper produces the
    // "Bellows agent run" commit, sees HEAD advance, and pushes —
    // preserving the legacy nit-batch behaviour exactly.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = prepare(&remote_url, "agent/52-bellows-commits-on-behalf")
        .await
        .unwrap();

    let head_before = head_sha(&workspace).await.unwrap();
    std::fs::write(workspace.path().join("edits.rs"), "fn edits() {}\n").unwrap();

    let head_after = commit_all_and_push_if_advanced(&workspace, &head_before)
        .await
        .expect("helper should succeed on bellows-on-behalf shape");

    assert_ne!(head_after, head_before, "HEAD must advance after commit_all");
    let remote_sha = remote_branch_sha(remote_dir.path(), "agent/52-bellows-commits-on-behalf")
        .expect("remote must hold the pushed branch");
    assert_eq!(remote_sha, head_after, "remote must absorb the bellows-on-behalf commit");
}

#[tokio::test]
async fn commit_all_and_push_if_advanced_pushes_once_for_mixed_self_commit_plus_leftover_edits() {
    // Corner 3: the agent self-committed *and* left further uncommitted
    // edits behind (e.g. agent-notes scratch). The helper must absorb
    // both into the post-invocation state and push once; the resulting
    // remote SHA must be at-or-after the agent's self-commit.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = prepare(&remote_url, "agent/52-mixed")
        .await
        .unwrap();

    let head_before = head_sha(&workspace).await.unwrap();

    std::fs::write(workspace.path().join("fix.rs"), "fn fix() {}\n").unwrap();
    run_git(workspace.path(), &["add", "fix.rs"]);
    run_git(workspace.path(), &["commit", "-m", "agent: fix"]);
    let after_self_commit = head_sha(&workspace).await.unwrap();

    // Leftover uncommitted edits in the same invocation.
    std::fs::write(workspace.path().join("agent-notes.md"), "trailing\n").unwrap();

    let head_after = commit_all_and_push_if_advanced(&workspace, &head_before)
        .await
        .expect("helper should succeed on mixed shape");

    assert_ne!(head_after, head_before);
    assert_ne!(
        head_after, after_self_commit,
        "HEAD must advance past the agent's self-commit when bellows-on-behalf also fires"
    );
    let remote_sha = remote_branch_sha(remote_dir.path(), "agent/52-mixed")
        .expect("remote must hold the pushed branch");
    assert_eq!(remote_sha, head_after, "remote must reflect the final HEAD");
}

// ----------------------------------------------------------------------
// Slice T1 (#21): commit_to_branch — direct-to-master commit helper
// used by `bellows triage <N>` when the verdict is wontfix-enhancement
// and the workspace must persist an .out-of-scope/<slug>.md precedent
// directly on master (no agent/* branch, no PR). The helper switches
// the workspace to the target branch, writes each (path, content) pair,
// stages it, commits with the supplied message, and pushes.
// ----------------------------------------------------------------------

fn init_remote_repo_accepting_master_push(path: &Path) {
    init_remote_repo(path);
    // Non-bare remote refuses pushes to its currently-checked-out
    // branch by default. updateInstead is the standard "treat the
    // remote like a deployment target" knob and is the right
    // semantics for these tests — bellows pushes wontfix-enhancement
    // commits directly to master.
    run_git(path, &["config", "receive.denyCurrentBranch", "updateInstead"]);
}

#[tokio::test]
async fn commit_to_branch_writes_files_and_pushes_directly_to_master() {
    // Slice T1 happy path: wontfix-enhancement workflow. The agent
    // produced an out_of_scope_filename + out_of_scope_content payload;
    // bellows writes it under `.out-of-scope/<filename>` on master and
    // pushes the precedent. A later operator can see the file on master
    // and a future triage agent can read it to align with prior
    // precedents.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo_accepting_master_push(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = prepare(&remote_url, "agent/triage-tmp").await.unwrap();

    let files = vec![(
        ".out-of-scope/auto-rerun.md".to_string(),
        "# Auto-rerun out of scope\n\nReason: ...\n".to_string(),
    )];

    commit_to_branch(
        &workspace,
        "master",
        "bellows triage: record auto-rerun as out-of-scope",
        &files,
    )
    .await
    .expect("commit_to_branch should succeed");

    let show = Command::new("git")
        .args(["show", "master:.out-of-scope/auto-rerun.md"])
        .current_dir(remote_dir.path())
        .output()
        .unwrap();
    assert!(show.status.success(), "remote master must contain the new file: {:?}", show);
    let content = String::from_utf8(show.stdout).unwrap();
    assert!(
        content.contains("Auto-rerun out of scope"),
        "remote master file content mismatch: {content:?}"
    );

    let log = Command::new("git")
        .args(["log", "-1", "--format=%s", "master"])
        .current_dir(remote_dir.path())
        .output()
        .unwrap();
    let subject = String::from_utf8(log.stdout).unwrap();
    assert!(
        subject.contains("record auto-rerun as out-of-scope"),
        "master's HEAD commit subject must reflect the supplied message: {subject:?}"
    );
}

#[tokio::test]
async fn commit_to_branch_creates_parent_directories_when_writing_nested_paths() {
    // The `.out-of-scope/` directory does not exist in a brand-new
    // repository. The helper must mkdir -p the parent so the write
    // succeeds; otherwise wontfix-enhancement against a clean repo
    // would crash on the file write.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo_accepting_master_push(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = prepare(&remote_url, "agent/triage-tmp").await.unwrap();

    let files = vec![(
        ".out-of-scope/nested/dir/file.md".to_string(),
        "body\n".to_string(),
    )];

    commit_to_branch(&workspace, "master", "create deeply-nested precedent", &files)
        .await
        .expect("commit_to_branch should succeed for deeply-nested paths");

    let show = Command::new("git")
        .args(["show", "master:.out-of-scope/nested/dir/file.md"])
        .current_dir(remote_dir.path())
        .output()
        .unwrap();
    assert!(show.status.success(), "deeply-nested file must exist on master");
    assert_eq!(String::from_utf8(show.stdout).unwrap(), "body\n");
}

#[tokio::test]
async fn commit_to_branch_writes_multiple_files_in_one_commit() {
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo_accepting_master_push(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = prepare(&remote_url, "agent/triage-tmp").await.unwrap();

    let files = vec![
        (".out-of-scope/a.md".to_string(), "a\n".to_string()),
        (".out-of-scope/b.md".to_string(), "b\n".to_string()),
    ];

    commit_to_branch(&workspace, "master", "two precedents", &files)
        .await
        .unwrap();

    let names = Command::new("git")
        .args(["log", "-1", "--name-only", "--format=", "master"])
        .current_dir(remote_dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8(names.stdout).unwrap();
    assert!(stdout.contains(".out-of-scope/a.md"), "log names: {stdout}");
    assert!(stdout.contains(".out-of-scope/b.md"), "log names: {stdout}");

    let oneline = Command::new("git")
        .args(["log", "--oneline", "master"])
        .current_dir(remote_dir.path())
        .output()
        .unwrap();
    let line_count = String::from_utf8(oneline.stdout).unwrap().lines().count();
    assert_eq!(
        line_count, 2,
        "master must have exactly initial + one new commit (both files in the same commit)"
    );
}

#[tokio::test]
async fn commit_to_branch_does_not_leave_workspace_on_the_target_branch_indefinitely() {
    // Defensive: the workspace was prepared on an agent/* branch. The
    // helper switches to the target (master) to commit, but the
    // workspace is short-lived and the test merely pins that the
    // helper succeeds without panicking. We don't promise to restore
    // the prior branch — the caller (bellows triage) discards the
    // workspace afterwards.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo_accepting_master_push(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = prepare(&remote_url, "agent/triage-tmp").await.unwrap();

    commit_to_branch(
        &workspace,
        "master",
        "test",
        &[(".out-of-scope/foo.md".to_string(), "x".to_string())],
    )
    .await
    .expect("commit_to_branch should succeed");

    // The workspace's local HEAD now points at the new master commit.
    let local_master = Command::new("git")
        .args(["rev-parse", "master"])
        .current_dir(workspace.path())
        .output()
        .unwrap();
    let remote_master = Command::new("git")
        .args(["rev-parse", "master"])
        .current_dir(remote_dir.path())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(local_master.stdout).unwrap().trim(),
        String::from_utf8(remote_master.stdout).unwrap().trim(),
        "workspace's master SHA must match remote master after push"
    );
}

#[tokio::test]
async fn commit_all_and_push_if_advanced_does_not_push_when_head_did_not_advance() {
    // Corner 4: agent did nothing — no commit, no edits. commit_all
    // returns NoChangesToCommit, HEAD did not advance, and the helper
    // must NOT push (no commits exist to push, and a no-op push would
    // still be wasted IO). The branch should not appear on the remote.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = prepare(&remote_url, "agent/52-no-op")
        .await
        .unwrap();

    let head_before = head_sha(&workspace).await.unwrap();
    let head_after = commit_all_and_push_if_advanced(&workspace, &head_before)
        .await
        .expect("helper should succeed on no-op");

    assert_eq!(head_after, head_before, "HEAD must not advance on a no-op invocation");
    assert!(
        !remote_has_branch(remote_dir.path(), "agent/52-no-op"),
        "remote must not receive a push when HEAD did not advance"
    );
}

// ---- Issue #40: generate_commit_log for the test-first review backstop ----

#[tokio::test]
async fn generate_commit_log_captures_clean_test_first_ordering_with_file_status() {
    // Acceptance criterion (brief): "workspace::generate_commit_log
    // writes the commit log over the `<default_branch>...HEAD` range to
    // that file." The clean test-first case has two commits on the
    // agent branch — a failing-test commit followed by a make-it-pass
    // commit. The reviewer must be able to read the file and see the
    // test-file paths arrived in the first commit, the source-file
    // paths in the second. `git log --name-status` is the wire format
    // the runner feeds the reviewer; we pin that the commit-log file
    // contains both commit subjects AND the touched paths so the
    // reviewer can reason about ordering.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    let workspace = prepare(&remote_url, "agent/40-test-first")
        .await
        .unwrap();

    std::fs::create_dir_all(workspace.path().join("tests")).unwrap();
    std::fs::write(
        workspace.path().join("tests").join("foo.rs"),
        "#[test]\nfn foo_returns_42() { assert_eq!(crate::foo(), 42); }\n",
    )
    .unwrap();
    run_git(workspace.path(), &["add", "tests/foo.rs"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "add failing test for foo_returns_42"],
    );

    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(
        workspace.path().join("src").join("foo.rs"),
        "pub fn foo() -> i32 { 42 }\n",
    )
    .unwrap();
    run_git(workspace.path(), &["add", "src/foo.rs"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "implement foo() to make foo_returns_42 pass"],
    );

    let dest = ".bellows-review-commit-log.txt";
    generate_commit_log(&workspace, dest)
        .await
        .expect("generate_commit_log should succeed");

    let log = std::fs::read_to_string(workspace.path().join(dest)).unwrap();

    assert!(
        log.contains("add failing test for foo_returns_42"),
        "commit log must include the failing-test commit subject: {log}"
    );
    assert!(
        log.contains("implement foo() to make foo_returns_42 pass"),
        "commit log must include the make-it-pass commit subject: {log}"
    );
    assert!(
        log.contains("tests/foo.rs"),
        "commit log must include test-file path (needs --name-status or equivalent): {log}"
    );
    assert!(
        log.contains("src/foo.rs"),
        "commit log must include source-file path (needs --name-status or equivalent): {log}"
    );
    // Ordering invariant: the failing-test commit appears in the log
    // BEFORE the make-it-pass commit, in some traversal direction.
    // `git log` defaults to reverse-chronological (newest first), so
    // we just assert both substrings exist; the reviewer-claude
    // applies its own ordering reasoning based on the commit headers.
    let test_pos = log.find("add failing test").unwrap();
    let impl_pos = log.find("implement foo()").unwrap();
    assert_ne!(
        test_pos, impl_pos,
        "the two commits must be distinguishable in the log: {log}"
    );
}

#[tokio::test]
async fn generate_commit_log_makes_mega_commit_ordering_visible_via_name_status() {
    // The mega-commit violation shape: a single commit on the agent
    // branch that touches both test files and source files. The
    // commit-log artefact must surface this — the reviewer cannot
    // reason about test-first ordering if `--name-status` (or
    // equivalent) is omitted, because the squashed diff alone shows
    // both files but not the fact that they landed in one commit.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    let workspace = prepare(&remote_url, "agent/40-mega-commit")
        .await
        .unwrap();

    std::fs::create_dir_all(workspace.path().join("tests")).unwrap();
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(
        workspace.path().join("tests").join("foo.rs"),
        "#[test]\nfn foo_returns_42() { assert_eq!(crate::foo(), 42); }\n",
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("src").join("foo.rs"),
        "pub fn foo() -> i32 { 42 }\n",
    )
    .unwrap();
    run_git(workspace.path(), &["add", "tests/foo.rs", "src/foo.rs"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "implement and test the foo flow"],
    );

    let dest = ".bellows-review-commit-log.txt";
    generate_commit_log(&workspace, dest)
        .await
        .expect("generate_commit_log should succeed");

    let log = std::fs::read_to_string(workspace.path().join(dest)).unwrap();

    assert!(
        log.contains("implement and test the foo flow"),
        "mega-commit subject must appear in the log: {log}"
    );
    // Both paths appear under ONE commit's name-status block. The
    // reviewer-claude infers the violation from the combination; the
    // bellows-side contract is that both paths are present.
    assert!(
        log.contains("tests/foo.rs") && log.contains("src/foo.rs"),
        "mega-commit log must include both test-file and source-file paths so the \
         reviewer can detect the single-commit violation: {log}"
    );
    // Sanity: there should be exactly one commit-header line on the
    // agent branch (the mega-commit). `git log` formats commit headers
    // with `commit <sha>` in its default output, which makes the
    // single-commit shape inspectable.
    let commit_headers = log.lines().filter(|l| l.starts_with("commit ")).count();
    assert_eq!(
        commit_headers, 1,
        "mega-commit case must have exactly one commit header in the log: {log}"
    );
}

#[tokio::test]
async fn generate_commit_log_captures_source_before_test_ordering() {
    // The source-before-test violation shape: source files landed in
    // an earlier commit than their corresponding tests. The commit-log
    // artefact must let the reviewer-claude see the chronological
    // order of file additions across commits, so this ordering shows
    // up as a flaggable signal.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    let workspace = prepare(&remote_url, "agent/40-source-first")
        .await
        .unwrap();

    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(
        workspace.path().join("src").join("foo.rs"),
        "pub fn foo() -> i32 { 42 }\n",
    )
    .unwrap();
    run_git(workspace.path(), &["add", "src/foo.rs"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "implement foo() function (no tests yet)"],
    );

    std::fs::create_dir_all(workspace.path().join("tests")).unwrap();
    std::fs::write(
        workspace.path().join("tests").join("foo.rs"),
        "#[test]\nfn foo_returns_42() { assert_eq!(crate::foo(), 42); }\n",
    )
    .unwrap();
    run_git(workspace.path(), &["add", "tests/foo.rs"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "add test for the foo() function we already shipped"],
    );

    let dest = ".bellows-review-commit-log.txt";
    generate_commit_log(&workspace, dest)
        .await
        .expect("generate_commit_log should succeed");

    let log = std::fs::read_to_string(workspace.path().join(dest)).unwrap();

    assert!(log.contains("implement foo() function"));
    assert!(log.contains("add test for the foo() function"));
    assert!(log.contains("src/foo.rs"));
    assert!(log.contains("tests/foo.rs"));
    // Pin the chronological ordering: `git log` defaults to
    // reverse-chronological (newest first), so the test-addition
    // commit appears BEFORE the source-addition commit in the file.
    // The reviewer-claude reading this file sees that the source
    // commit is the older of the two — i.e. tests trailed source.
    let src_pos = log.find("implement foo() function").unwrap();
    let test_pos = log.find("add test for the foo() function").unwrap();
    assert!(
        test_pos < src_pos,
        "reverse-chronological default: the newer test-add commit must appear \
         before the older src-add commit in the log file (so the reviewer sees \
         tests trailed source): test_pos={test_pos}, src_pos={src_pos}, log:\n{log}"
    );
}

#[tokio::test]
async fn generate_commit_log_is_empty_when_branch_is_at_parity_with_base() {
    // Edge case: a workspace with no commits beyond the base branch
    // (e.g. an implement phase that crashed before producing any
    // commits, or a tracer-bullet run) produces an empty commit log.
    // The reviewer sees "no commits to reason about" rather than a
    // missing file or a file containing every commit on master.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    let workspace = prepare(&remote_url, "agent/40-no-commits")
        .await
        .unwrap();

    let dest = ".bellows-review-commit-log.txt";
    generate_commit_log(&workspace, dest)
        .await
        .expect("generate_commit_log should succeed on an empty range");

    let log = std::fs::read_to_string(workspace.path().join(dest)).unwrap();
    assert!(
        log.trim().is_empty(),
        "branch at parity with base must produce an empty commit log, got: {log:?}"
    );
}

// ---- ADR-0004: parsed-or-fallback GateCommands snapshotted at prepare time ----

fn init_remote_with_ci_workflow(path: &Path, ci_yaml: &str) {
    init_remote_repo(path);
    let dir = path.join(".github").join("workflows");
    std::fs::create_dir_all(&dir).expect("create .github/workflows");
    std::fs::write(dir.join("ci.yml"), ci_yaml).expect("write ci.yml");
    run_git(path, &["add", "."]);
    run_git(path, &["commit", "-m", "add ci workflow"]);
}

#[tokio::test]
async fn prepare_with_gates_snapshots_extracted_commands_when_workflow_present() {
    // ADR-0004 acceptance: when the target repo's CI workflow declares
    // a clippy + test posture different to bellows's strict defaults,
    // `prepare_with_gates` extracts them verbatim and snapshots them
    // onto the Workspace. Each gate command carries
    // `ParsedFromWorkflow` provenance so the run-log line is
    // unambiguous about whether bellows mirrored CI or fell back to
    // config.
    let remote_dir = TempDir::new().unwrap();
    init_remote_with_ci_workflow(
        remote_dir.path(),
        r#"
name: CI
on: [push]
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - run: cargo clippy --all-targets -- -D clippy::correctness -D clippy::suspicious
      - run: cargo test --features in-memory
"#,
    );
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = prepare_with_gates(
        &remote_url,
        "agent/90-snapshot-parsed",
        &GatesConfig::default(),
    )
    .await
    .unwrap();
    let gc = workspace.gate_commands();
    assert_eq!(
        gc.clippy,
        "cargo clippy --all-targets -- -D clippy::correctness -D clippy::suspicious",
    );
    assert_eq!(gc.test, "cargo test --features in-memory");
    assert!(
        matches!(gc.clippy_source, Provenance::ParsedFromWorkflow(_)),
        "clippy provenance must report parsed-from-workflow",
    );
    assert!(
        matches!(gc.test_source, Provenance::ParsedFromWorkflow(_)),
        "test provenance must report parsed-from-workflow",
    );
}

#[tokio::test]
async fn prepare_with_gates_falls_back_to_config_when_workflow_absent() {
    // ADR-0004 acceptance: a target repo with NO `.github/workflows/`
    // directory at all falls all the way through to the operator-
    // declared `[gates].*_flags`. The fallback must be applied verbatim
    // and each command's provenance reports FallbackFromConfig so the
    // run-log line documents which path took effect.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let gates = GatesConfig {
        clippy_flags: "--operator-strict --no-deps".to_string(),
        test_flags: "--workspace --no-run".to_string(),
    };
    let workspace = prepare_with_gates(&remote_url, "agent/90-fallback-all", &gates)
        .await
        .unwrap();
    let gc = workspace.gate_commands();
    assert_eq!(gc.clippy, "cargo clippy --operator-strict --no-deps");
    assert_eq!(gc.test, "cargo test --workspace --no-run");
    assert!(matches!(gc.clippy_source, Provenance::FallbackFromConfig));
    assert!(matches!(gc.test_source, Provenance::FallbackFromConfig));
}

#[tokio::test]
async fn prepare_with_gates_per_command_fallback_when_only_one_extracted() {
    // ADR-0004 acceptance: when a CI workflow declares `cargo test` as
    // a literal step but wraps `cargo clippy` in a shell script the
    // parser can't follow, ONLY the unparsed command falls back. The
    // extracted side reports ParsedFromWorkflow; the fallback side
    // reports FallbackFromConfig. This is the operationally important
    // case — partial extraction is the common shape, not all-or-nothing.
    let remote_dir = TempDir::new().unwrap();
    init_remote_with_ci_workflow(
        remote_dir.path(),
        r#"
name: CI
on: [push]
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - run: ./scripts/run-clippy.sh
      - run: cargo test --all-targets --features ci
"#,
    );
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let gates = GatesConfig {
        clippy_flags: "--operator-clippy-default".to_string(),
        test_flags: "--operator-test-default".to_string(),
    };
    let workspace = prepare_with_gates(&remote_url, "agent/90-partial", &gates)
        .await
        .unwrap();
    let gc = workspace.gate_commands();
    assert_eq!(
        gc.clippy, "cargo clippy --operator-clippy-default",
        "clippy must fall back when wrapped in a shell script",
    );
    assert!(matches!(gc.clippy_source, Provenance::FallbackFromConfig));
    assert_eq!(
        gc.test, "cargo test --all-targets --features ci",
        "test must extract verbatim despite clippy falling back",
    );
    assert!(
        matches!(gc.test_source, Provenance::ParsedFromWorkflow(_)),
        "test provenance must remain ParsedFromWorkflow when clippy alone fell back",
    );
}

#[tokio::test]
async fn prepare_with_gates_snapshot_does_not_shift_when_workflow_edited_mid_pipeline() {
    // ADR-0004 acceptance: subsequent gate phases within the same run
    // use the snapshotted commands even if `.github/workflows/ci.yml`
    // is edited mid-pipeline. The snapshot is captured ONCE at
    // prepare time; later phases read the snapshotted value, not the
    // workspace's current file contents. This is the invariant that
    // keeps the in-flight verdict stable when the agent itself edits
    // the workflow file.
    let remote_dir = TempDir::new().unwrap();
    init_remote_with_ci_workflow(
        remote_dir.path(),
        r#"
name: CI
on: [push]
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - run: cargo clippy --pinned-at-prepare
      - run: cargo test --pinned-at-prepare
"#,
    );
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = prepare_with_gates(
        &remote_url,
        "agent/90-snapshot-stability",
        &GatesConfig::default(),
    )
    .await
    .unwrap();
    let initial_clippy = workspace.gate_commands().clippy.clone();
    let initial_test = workspace.gate_commands().test.clone();

    // Mid-pipeline edit (mimics an agent rewriting ci.yml during the
    // run). The post-implement and end-pipeline gates must NOT pick
    // this up — they read from the workspace's snapshot.
    let edited_yaml = r#"
name: CI
on: [push]
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - run: cargo clippy --POST-EDIT
      - run: cargo test --POST-EDIT
"#;
    std::fs::write(
        workspace.path().join(".github").join("workflows").join("ci.yml"),
        edited_yaml,
    )
    .unwrap();

    let gc = workspace.gate_commands();
    assert_eq!(
        gc.clippy, initial_clippy,
        "mid-pipeline edit must not shift snapshotted clippy command",
    );
    assert_eq!(
        gc.test, initial_test,
        "mid-pipeline edit must not shift snapshotted test command",
    );
}

#[tokio::test]
async fn gate_commands_announcement_lines_attribute_parsed_commands_to_workflow_path() {
    // ADR-0004 acceptance: the run-log line at each gate-phase start
    // must state the actual command being run AND its provenance, so
    // an operator reading the log can tell whether bellows mirrored
    // the target's CI or fell back to config. When both commands
    // parsed from `.github/workflows/ci.yml`, both lines tag the
    // workflow path.
    let remote_dir = TempDir::new().unwrap();
    init_remote_with_ci_workflow(
        remote_dir.path(),
        r#"
name: CI
on: [push]
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - run: cargo clippy --all-targets -- -D clippy::correctness
      - run: cargo test --features in-memory
"#,
    );
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = prepare_with_gates(
        &remote_url,
        "agent/90-announce-parsed",
        &GatesConfig::default(),
    )
    .await
    .unwrap();
    let lines = workspace.gate_commands().announcement_lines();
    assert_eq!(lines.len(), 2, "expected one line per check: {lines:?}");
    assert!(lines[0].contains("clippy:"), "first line must label clippy: {:?}", lines[0]);
    assert!(
        lines[0].contains("cargo clippy --all-targets -- -D clippy::correctness"),
        "clippy line must contain the actual command: {:?}",
        lines[0],
    );
    assert!(
        lines[0].contains("parsed from"),
        "clippy line must tag parsed provenance: {:?}",
        lines[0],
    );
    assert!(
        lines[0].contains(".github/workflows/ci.yml"),
        "clippy line must name the workflow path: {:?}",
        lines[0],
    );
    assert!(lines[1].contains("test:"), "second line must label test: {:?}", lines[1]);
    assert!(
        lines[1].contains("cargo test --features in-memory"),
        "test line must contain the actual command: {:?}",
        lines[1],
    );
    assert!(
        lines[1].contains("parsed from"),
        "test line must tag parsed provenance: {:?}",
        lines[1],
    );
}

#[tokio::test]
async fn gate_commands_announcement_lines_attribute_fallback_commands_to_config() {
    // ADR-0004 acceptance: when bellows fell back to the
    // operator-declared `[gates].*_flags`, the run-log line must say
    // so explicitly. An operator reading the log can immediately tell
    // bellows did NOT mirror CI — usually a cue to either fix the
    // workflow shape so bellows can parse it, or update the config
    // fallback to match CI's intended posture.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = prepare_with_gates(
        &remote_url,
        "agent/90-announce-fallback",
        &GatesConfig::default(),
    )
    .await
    .unwrap();
    let lines = workspace.gate_commands().announcement_lines();
    assert_eq!(lines.len(), 2);
    assert!(
        lines[0].contains("fallback"),
        "clippy fallback line must mention 'fallback': {:?}",
        lines[0],
    );
    assert!(
        lines[0].contains("[gates].clippy_flags"),
        "clippy fallback line must name the config knob: {:?}",
        lines[0],
    );
    assert!(
        lines[1].contains("fallback"),
        "test fallback line must mention 'fallback': {:?}",
        lines[1],
    );
    assert!(
        lines[1].contains("[gates].test_flags"),
        "test fallback line must name the config knob: {:?}",
        lines[1],
    );
}

// ---- ADR-0006: auto-merge.yml `agent-noted` filter-support snapshot ----

fn init_remote_with_auto_merge_workflow(path: &Path, body: &str) {
    init_remote_repo(path);
    let dir = path.join(".github").join("workflows");
    std::fs::create_dir_all(&dir).expect("create .github/workflows");
    std::fs::write(dir.join("auto-merge.yml"), body).expect("write auto-merge.yml");
    run_git(path, &["add", "."]);
    run_git(path, &["commit", "-m", "add auto-merge workflow"]);
}

#[tokio::test]
async fn prepare_reports_filter_supported_when_auto_merge_workflow_absent() {
    // ADR-0006 acceptance: a target repo with NO
    // `.github/workflows/auto-merge.yml` has no auto-merge mechanism
    // to defeat in the first place — opening a `SuccessWithNotes` PR
    // non-draft is safe by construction. The flag therefore reports
    // `true` in this case so the runner does not pointlessly fall
    // back to draft when there is no auto-merge to bypass.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = prepare(&remote_url, "agent/96-no-workflow")
        .await
        .unwrap();
    assert!(
        workspace.auto_merge_workflow_supports_agent_noted_filter(),
        "absent .github/workflows/auto-merge.yml must report supported \
         (no auto-merge to defeat; non-draft is safe)",
    );
}

#[tokio::test]
async fn prepare_reports_filter_supported_when_auto_merge_workflow_mentions_agent_noted() {
    // ADR-0006 acceptance: when the target's auto-merge workflow's
    // content contains the literal `agent-noted` string anywhere, the
    // snapshot reports `true`. Substring check rather than structural
    // YAML parse: a future operator using template injection that
    // routes the label string indirectly would get a false negative,
    // which is the acceptable failure direction (the runner opens
    // draft as the safe fallback).
    let remote_dir = TempDir::new().unwrap();
    init_remote_with_auto_merge_workflow(
        remote_dir.path(),
        r#"
name: Auto-merge bellows PRs
on:
  workflow_run:
    workflows: [CI]
    types: [completed]
jobs:
  auto-merge:
    if: ${{ github.event.workflow_run.conclusion == 'success' }}
    runs-on: ubuntu-latest
    steps:
      - uses: actions/github-script@v7
        with:
          script: |
            // Filter: skip PRs labelled `agent-noted` per ADR-0006.
            if (pr.labels && pr.labels.some(l => l.name === 'agent-noted')) {
              core.info(`Skipping PR #${pr.number}: labelled agent-noted.`);
              continue;
            }
"#,
    );
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = prepare(&remote_url, "agent/96-with-filter")
        .await
        .unwrap();
    assert!(
        workspace.auto_merge_workflow_supports_agent_noted_filter(),
        "auto-merge.yml present + mentions `agent-noted` must report \
         supported",
    );
}

#[tokio::test]
async fn prepare_reports_filter_unsupported_when_auto_merge_workflow_omits_agent_noted() {
    // ADR-0006 acceptance: when the target's auto-merge workflow file
    // exists but does NOT contain the literal `agent-noted` string,
    // the snapshot reports `false`. The runner then falls back to
    // opening `SuccessWithNotes` PRs draft so a silent auto-merge
    // cannot bypass the operator's read-the-note step.
    let remote_dir = TempDir::new().unwrap();
    init_remote_with_auto_merge_workflow(
        remote_dir.path(),
        r#"
name: Auto-merge bellows PRs
on:
  workflow_run:
    workflows: [CI]
    types: [completed]
jobs:
  auto-merge:
    if: ${{ github.event.workflow_run.conclusion == 'success' }}
    runs-on: ubuntu-latest
    steps:
      - uses: actions/github-script@v7
        with:
          script: |
            // Legacy auto-merge with no ADR-0006 filter.
            if (pr.draft) { continue; }
            await github.rest.pulls.merge({ pull_number: pr.number, merge_method: 'squash' });
"#,
    );
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = prepare(&remote_url, "agent/96-without-filter")
        .await
        .unwrap();
    assert!(
        !workspace.auto_merge_workflow_supports_agent_noted_filter(),
        "auto-merge.yml present + omits `agent-noted` must report \
         unsupported so the runner can fall back to draft",
    );
}

#[tokio::test]
async fn prepare_with_gates_snapshots_auto_merge_filter_support_alongside_gate_commands() {
    // Acceptance: `prepare_with_gates` (the runner's entry point) also
    // populates the auto-merge filter-support flag in the same
    // snapshot pass that populates the ADR-0004 gate commands. A
    // present-but-no-filter workflow must report unsupported even
    // when the parsed/fallback gates path is exercised.
    let remote_dir = TempDir::new().unwrap();
    init_remote_with_auto_merge_workflow(
        remote_dir.path(),
        "name: Auto-merge\non: push\njobs:\n  noop:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo no-filter\n",
    );
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = prepare_with_gates(
        &remote_url,
        "agent/96-prepare-with-gates",
        &GatesConfig::default(),
    )
    .await
    .unwrap();
    assert!(
        !workspace.auto_merge_workflow_supports_agent_noted_filter(),
        "prepare_with_gates must populate the filter-support snapshot; \
         no-filter workflow must report unsupported",
    );
}

#[tokio::test]
async fn prepare_keeps_existing_default_gates_behaviour_for_callers_without_config() {
    // Back-compat acceptance: the legacy two-arg `prepare(url, branch)`
    // shape is still supported and resolves to the strict-default
    // GatesConfig. This keeps existing test callers and any code path
    // that does not have access to the runtime config working without
    // a forced rewrite, while still exposing a populated
    // `gate_commands` field on the returned Workspace.
    let remote_dir = TempDir::new().unwrap();
    init_remote_repo(remote_dir.path());
    let remote_url = remote_dir.path().to_string_lossy().to_string();
    let workspace = prepare(&remote_url, "agent/90-default-fallback")
        .await
        .unwrap();
    let gc = workspace.gate_commands();
    assert_eq!(
        gc.clippy, "cargo clippy --all-targets --all-features -- -D warnings",
    );
    assert_eq!(gc.test, "cargo test --all-targets --all-features");
    assert!(matches!(gc.clippy_source, Provenance::FallbackFromConfig));
    assert!(matches!(gc.test_source, Provenance::FallbackFromConfig));
}
