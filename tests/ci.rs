//! Integration tests for the GitHub Actions CI workflow.
//!
//! Issue #36 adds a CI gate that runs the same `cargo test` and
//! `cargo clippy` checks bellows runs internally, so human-authored
//! fix-up commits and direct pushes get the same gate agent commits do.
//! These tests pin the load-bearing content of the workflow file so it
//! cannot silently drift from the bellows-internal gate (which would
//! defeat the whole point — the two verdicts must agree by
//! construction).

use std::fs;
use std::path::PathBuf;

fn workflow_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".github")
        .join("workflows")
        .join("ci.yml")
}

fn read_workflow() -> String {
    fs::read_to_string(workflow_path()).unwrap_or_else(|e| {
        panic!(
            "CI workflow must exist at .github/workflows/ci.yml: {}",
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
        "CI workflow is missing {} in section {:?}: {:?}",
        if missing.len() == 1 { "value" } else { "values" },
        context,
        missing,
    );
}

#[test]
fn ci_workflow_file_exists_at_canonical_path() {
    let path = workflow_path();
    assert!(
        path.is_file(),
        "CI workflow must exist at {} (GitHub Actions only picks up \
         workflows under .github/workflows/)",
        path.display(),
    );
    let body = read_workflow();
    assert!(
        !body.trim().is_empty(),
        "CI workflow must not be empty",
    );
}

#[test]
fn ci_workflow_runs_cargo_test_with_all_targets_and_features() {
    let body = read_workflow();
    // Brief: the CI verdict and the bellows-internal gate verdict must
    // agree by construction. Bellows runs `cargo test
    // --all-targets --all-features` internally; CI must use the
    // exact same args. The substring assertion catches drift in
    // either flag.
    assert!(
        body.contains("cargo test --all-targets --all-features"),
        "CI workflow must run `cargo test --all-targets --all-features` \
         verbatim (matches the bellows-internal gate). Got:\n{}",
        body,
    );
}

#[test]
fn ci_workflow_runs_cargo_clippy_with_exact_bellows_internal_args() {
    let body = read_workflow();
    // Brief: this is the **load-bearing** correspondence. The
    // bellows-internal cargo checks gate runs
    // `cargo clippy --all-targets --all-features -- -D warnings`.
    // CI must invoke clippy with **exactly** the same args — any
    // drift (missing `--all-features`, missing `-D warnings`,
    // dropping `--all-targets`) produces a future bug where CI says
    // green but the bellows-internal gate says red (or vice versa)
    // on the same code. The brief calls this out as the single
    // most important invariant in the slice.
    assert!(
        body.contains(
            "cargo clippy --all-targets --all-features -- -D warnings"
        ),
        "CI workflow must run clippy with EXACTLY the same args as the \
         bellows-internal gate (`cargo clippy --all-targets \
         --all-features -- -D warnings`). Drift here produces \
         CI/internal-gate disagreement, which defeats the slice. \
         Got:\n{}",
        body,
    );
}

#[test]
fn ci_workflow_uses_pinned_supporting_actions() {
    let body = read_workflow();
    // Brief: checkout via actions/checkout@v4, Rust via
    // dtolnay/rust-toolchain@stable, cargo cache via
    // Swatinem/rust-cache@v2. Pinning the action versions explicitly
    // (not @main) is the standard supply-chain-hygiene pattern, and
    // rust-cache is the load-bearing piece that takes cold-cache CI
    // from 4-8 min down to ~1 min on warm runs — the brief calls this
    // out explicitly.
    assert_contains_all(
        &body,
        &[
            "actions/checkout@v4",
            "dtolnay/rust-toolchain@stable",
            "Swatinem/rust-cache@v2",
        ],
        "supporting-actions",
    );
}

#[test]
fn ci_workflow_runs_on_ubuntu_latest() {
    let body = read_workflow();
    // Brief: Linux-only matrix in v1; Windows/macOS deferred. Pin the
    // runner string so a future drive-by-edit cannot silently switch
    // hosts (changing OS would change which clippy lints trip, which
    // is the kind of thing CI exists to catch).
    assert!(
        body.contains("runs-on: ubuntu-latest"),
        "CI workflow must run on `ubuntu-latest` (v1 is Linux-only). \
         Got:\n{}",
        body,
    );
}

#[test]
fn ci_workflow_triggers_on_pr_and_push_to_master() {
    let body = read_workflow();
    // Brief: workflow runs on `pull_request` (against any branch) and
    // on `push` to `master`. The pull_request trigger gates merge-button
    // green; the push-to-master trigger is what surfaces the `ci` check
    // in branch-protection UI on the first run (so operators can then
    // tick it as required).
    assert_contains_all(
        &body,
        &["pull_request", "push", "master"],
        "triggers",
    );
}

#[test]
fn ci_workflow_declares_a_job_named_exactly_ci() {
    let body = read_workflow();
    // The job key MUST be exactly `ci` so the README's
    // "required status check: ci" instructions match the actual check
    // name GitHub surfaces in the branch-protection UI. Drift here
    // turns the README into a lie.
    assert!(
        body.contains("\n  ci:") || body.contains("\nci:"),
        "CI workflow must declare a job keyed exactly `ci` (so the \
         status check surfaces under that name in branch protection). \
         Got:\n{}",
        body,
    );
}
