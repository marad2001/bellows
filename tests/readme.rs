//! Integration tests for the operator-facing README.md.
//!
//! Issue #14 (slice 13) makes the README the single source of truth for
//! operators installing and driving bellows. These tests pin each
//! acceptance criterion from the brief to a checkable assertion so the
//! README can evolve without losing the load-bearing content (label
//! vocabularies, troubleshooting remedies, TOS posture, v1 scope).

use std::fs;
use std::path::PathBuf;

fn readme_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("README.md")
}

fn read_readme() -> String {
    fs::read_to_string(readme_path())
        .unwrap_or_else(|e| panic!("README.md must exist at repo root: {}", e))
}

/// Assert that every needle appears in the README. Failure messages
/// name the needle and the missing section so a regression points at
/// what was lost, not at "test failed: line 99".
fn assert_contains_all(body: &str, needles: &[&str], context: &str) {
    let mut missing = Vec::new();
    for needle in needles {
        if !body.contains(needle) {
            missing.push(*needle);
        }
    }
    assert!(
        missing.is_empty(),
        "README is missing {} in section {:?}: {:?}",
        if missing.len() == 1 { "value" } else { "values" },
        context,
        missing,
    );
}

#[test]
fn readme_exists_at_repo_root() {
    let path = readme_path();
    assert!(
        path.is_file(),
        "README.md must exist at repo root (looked at {})",
        path.display(),
    );
    let body = read_readme();
    assert!(
        !body.trim().is_empty(),
        "README.md must not be empty",
    );
}

#[test]
fn readme_links_to_existing_repo_docs_for_deeper_reading() {
    let body = read_readme();
    // Brief: README should reference (with relative links) the
    // existing docs in the repo. We test that the link targets that
    // *exist on disk* are referenced — broken-on-the-future-day
    // links to docs that haven't been added (CONTEXT.md) are not
    // required.
    let docs = [
        "RESEARCH.md",
        "orchestrator.example.toml",
        "CLAUDE.md",
        "docs/agents/triage-labels.md",
        "docs/agents/issue-tracker.md",
    ];
    assert_contains_all(&body, &docs, "doc cross-references");
}

#[test]
fn readme_link_targets_exist_on_disk() {
    let body = read_readme();
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // Every relative path listed below must (a) appear in the
    // README and (b) exist on disk. Together these catch typos and
    // dead links without parsing markdown.
    let must_exist = [
        "RESEARCH.md",
        "PRD.md",
        "CLAUDE.md",
        "orchestrator.example.toml",
        "docs/agents/triage-labels.md",
        "docs/agents/issue-tracker.md",
        "src/auth.rs",
        "policy-image/skills/",
    ];
    let mut broken = Vec::new();
    for path in must_exist {
        assert!(
            body.contains(path),
            "README should reference {} (so its existence is tested)",
            path,
        );
        let full = repo_root.join(path);
        let exists = full.is_file() || (path.ends_with('/') && full.is_dir());
        if !exists {
            broken.push(path);
        }
    }
    assert!(
        broken.is_empty(),
        "README links to paths that do not exist on disk: {:?}",
        broken,
    );
}

#[test]
fn readme_enumerates_all_canonical_labels() {
    let body = read_readme();
    // Brief acceptance criteria: all five triage labels + the two
    // category labels + all five runtime labels appear by name. The
    // acceptance criteria explicitly call this out twice; pin both
    // sets in one test so the README cannot drop one silently.
    let triage = [
        "`needs-triage`",
        "`needs-info`",
        "`ready-for-agent`",
        "`ready-for-human`",
        "`wontfix`",
    ];
    let categories = ["`bug`", "`enhancement`"];
    let runtime = [
        "`agent-in-progress`",
        "`agent-done`",
        "`agent-failed`",
        "`agent-rate-limited`",
        "`agent-cancelled`",
    ];
    assert_contains_all(&body, &triage, "labels / triage");
    assert_contains_all(&body, &categories, "labels / category");
    assert_contains_all(&body, &runtime, "labels / runtime");
}

#[test]
fn readme_documents_v1_scope_in_and_out() {
    let body = read_readme();
    // Brief 9: two short lists, in-v1 vs not-in-v1.
    // In: single repo, single in-flight issue, subscription, headless
    // Claude Code in docker, clippy + cargo test gate, automated code
    // review + fix loop, draft-PR-on-failure, GitHub label state
    // machine.
    // Out: multi-repo, multi-host, parallelism, sccache / build
    // caches, web dashboard, push notifications, auto-retry, GitHub
    // App auth, per-repo policy, cost/token tracking, automated
    // triage (#21/#22), security review (#20).
    assert_contains_all(
        &body,
        &[
            "## v1 scope",
            "### In v1",
            "single repo",
            "single in-flight issue",
            "subscription",
            "headless",
            "code review",
            "draft PR",
            "label state machine",
            "### Not in v1",
            "multi-repo",
            "parallel",
            "sccache",
            "web dashboard",
            "push notification",
            "auto-retry",
            "GitHub App",
            "per-repo policy",
            "cost",
            "automated triage",
            "security review",
        ],
        "v1-scope",
    );
}

#[test]
fn readme_documents_tos_posture_and_api_key_fallback() {
    let body = read_readme();
    // Brief 8: TOS posture. Subscription auth via Claude Code's
    // headless mode, concurrency=1, and the Auth::ApiKey fallback
    // (currently a `todo!()` enum variant in src/auth.rs).
    assert_contains_all(
        &body,
        &[
            "## TOS",
            "subscription",
            "headless",
            "Concurrency",
            "Auth::ApiKey",
            "src/auth.rs",
        ],
        "tos / api-key-fallback",
    );
}

#[test]
fn readme_troubleshooting_documents_cold_cache_with_issue_6_reference() {
    let body = read_readme();
    // Brief 7: cold-cache build cost is the single biggest Rust-
    // specific operational risk. README must document the
    // manual-prewarm workaround and reference issue #6 (cache
    // volumes) as the future fix.
    assert_contains_all(
        &body,
        &[
            "## Troubleshooting",
            "cold",
            "cargo build",
            "#6",
        ],
        "troubleshooting / cold-cache",
    );
}

#[test]
fn readme_troubleshooting_documents_refresh_auth_remedy_with_issue_10_reference() {
    let body = read_readme();
    // Brief 7: expired refresh-token symptom + remedy. Slice 12
    // (issue #10) is referenced as the auto-detection work.
    assert_contains_all(
        &body,
        &[
            "bellows refresh-auth",
            "401",
            "#10",
        ],
        "troubleshooting / refresh-auth",
    );
}

#[test]
fn readme_troubleshooting_covers_orphan_stuck_label_and_wall_clock() {
    let body = read_readme();
    // Brief 7: three more troubleshooting items. Orphan containers
    // are auto-handled by slice 7; stuck `agent-in-progress` needs
    // operator re-label; wall-clock cap is bumpable via config.
    assert_contains_all(
        &body,
        &[
            "orphan",
            "agent-in-progress",
            "wall_clock_minutes",
        ],
        "troubleshooting / orphan-stuck-wallclock",
    );
}

#[test]
fn readme_walks_through_issue_lifecycle() {
    let body = read_readme();
    // Brief 6: lifecycle. File issue → triage → ready-for-agent
    // with brief → bellows picks up at next poll → agent runs → PR
    // opens → human merges or rejects. Plus: no auto-retry, you
    // re-label to retry.
    assert_contains_all(
        &body,
        &[
            "## Issue lifecycle",
            "ready-for-agent",
            "agent brief",
            "auto-retry",
            "re-label",
        ],
        "issue-lifecycle",
    );
}

#[test]
fn readme_documents_daily_use_operational_toolkit() {
    let body = read_readme();
    // Brief 5: daily use. Document each subcommand operators reach
    // for: run / status / kill / refresh-auth. The brief also calls
    // out the log file path and the phase-boundary console output.
    assert_contains_all(
        &body,
        &[
            "## Daily use",
            "bellows run",
            "bellows status",
            "bellows kill",
            "bellows refresh-auth",
            "bellows.log",
        ],
        "daily-use",
    );
}

#[test]
fn readme_documents_one_time_setup_config_and_credentials() {
    let body = read_readme();
    // Brief 4: one-time setup. Document every section of
    // orchestrator.example.toml, mention the PAT env var, the
    // `bellows setup-auth` flow, and label-vocabulary creation.
    assert_contains_all(
        &body,
        &[
            "## One-time setup",
            "orchestrator.example.toml",
            "orchestrator.toml",
            "[repo]",
            "[github]",
            "[polling]",
            "[runtime_labels]",
            "[logging]",
            "[auth]",
            "[agent]",
            "wall_clock_minutes",
            "BELLOWS_GITHUB_TOKEN",
            "bellows setup-auth",
            "/login",
        ],
        "one-time-setup",
    );
}

#[test]
fn readme_documents_install_from_source() {
    let body = read_readme();
    // Brief 3: install. `cargo install --path .` from a clone is the
    // only supported path in v1 — no crates.io publish target. The
    // brief also accepts `cargo build --release`-then-run-the-binary
    // as an alternative.
    assert_contains_all(
        &body,
        &[
            "## Install",
            "cargo install --path .",
        ],
        "install",
    );
}

#[test]
fn readme_lists_prerequisites_for_running_bellows() {
    let body = read_readme();
    // Brief 2: prereqs. Docker, Rust toolchain, a fine-grained PAT
    // with the right scopes, target repo's label vocabulary in
    // place. Test for the load-bearing nouns; let the prose around
    // them flex.
    assert_contains_all(
        &body,
        &[
            "## Prerequisites",
            "Docker",
            "Rust",
            "Personal Access Token",
            "Issues",
            "Pull requests",
            "Contents",
        ],
        "prerequisites",
    );
}

#[test]
fn readme_shows_ci_status_badge_pointing_at_the_workflow() {
    let body = read_readme();
    // Issue #36: a CI status badge near the top of the README is the
    // quickest way for a visitor to see master's gate status. The
    // badge.svg + workflow link pattern is the GitHub-standard shape;
    // assert the workflow path appears (so a future rename of the
    // workflow file would also need to update the badge target).
    assert_contains_all(
        &body,
        &[
            "actions/workflows/ci.yml/badge.svg",
            "actions/workflows/ci.yml",
        ],
        "ci-badge",
    );
}

#[test]
fn readme_documents_branch_protection_setup_with_ui_quirk_note() {
    let body = read_readme();
    // Issue #36 acceptance criteria:
    //  - a `## Branch protection setup` (or matching) subsection under
    //    one-time setup,
    //  - covering: require PR, require status check `ci`, require
    //    linear history, block force-push, block deletions, admin
    //    bypass allowed,
    //  - explicitly naming the GitHub-UI quirk: the `ci` status check
    //    only becomes selectable in the protection rule AFTER the
    //    workflow has run at least once on master (so the post-merge
    //    ordering is: merge → workflow fires → reopen settings →
    //    tick the now-visible `ci` check).
    assert_contains_all(
        &body,
        &[
            "Branch protection",
            "linear history",
            "force push",
            "deletion",
            "admin",
            "`ci`",
            // The UI-quirk note: the status check only appears after
            // the first workflow run on master. We check for the
            // load-bearing fragments rather than a verbatim sentence
            // so prose can flex.
            "first",
            "after",
            "master",
        ],
        "branch-protection-setup",
    );
}

#[test]
fn readme_v1_scope_lists_ci_gate_as_in_scope() {
    let body = read_readme();
    // Issue #36: the v1 in-scope list gains the GitHub Actions CI
    // gate. The brief calls this out as a required README change so
    // the scope-list stays the canonical "what does v1 do" answer.
    // We check for a near-verbatim phrase rather than just "CI"
    // (which is too generic) to make this assertion actually pin
    // the change.
    let candidates = [
        "GitHub Actions CI",
        "GitHub Actions",
    ];
    let has_any = candidates.iter().any(|c| body.contains(c));
    assert!(
        has_any,
        "README v1 scope must mention the new GitHub Actions CI gate \
         (issue #36). Looked for any of: {:?}",
        candidates,
    );
}

#[test]
fn readme_opens_with_overview_explaining_what_bellows_is() {
    let body = read_readme();
    // The README must answer "what is this thing?" before anything
    // else. Brief 1: two-paragraph overview naming the moving parts
    // (GitHub issue tracker, sandboxed Claude Code in Docker,
    // clippy + tests + code review, opens a PR).
    assert_contains_all(
        &body,
        &[
            "AFK",
            "Claude Code",
            "ready-for-agent",
            "Docker",
            "clippy",
            "cargo test",
            "pull request",
        ],
        "overview / what-is-bellows",
    );
}
