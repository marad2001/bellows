//! Integration tests for the `policy-image-build` CI job (issue #132).
//!
//! Issue #132 adds a CI gate that runs `docker build` against
//! `policy-image/Dockerfile` on every pull request and every push to
//! master, so a sha256 drift between any pinned engine const and what
//! the upstream registry/release currently serves fails at PR time
//! instead of in `bellows run` on the operator's laptop.
//!
//! The existing unit tests in `tests/engine_opencode_dockerfile.rs` and
//! `tests/engine_opencode_policy_image_gen.rs` verify the const-vs-
//! Dockerfile consistency. They catch a manual Dockerfile edit that
//! drifts away from `policy_image_gen::OPENCODE_SHA256`, but they
//! cannot catch the case where the const itself is wrong relative to
//! what the npm registry currently serves. Only an actual `docker
//! build` of the policy image catches that — because only `docker
//! build` runs the `sha256sum -c -` step against the live tarball.
//!
//! These tests pin the load-bearing content of the new job so the CI
//! addition cannot silently drift away from the brief's acceptance
//! criteria (parallel to `ci`, sha-verifying every engine pin,
//! GHA-cached, distinguishably named).

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

/// Locate the YAML block that defines the `policy-image-build` job and
/// return it as a substring of the workflow. The block runs from the
/// `policy-image-build:` key down to the next top-level `jobs:` entry
/// (which would be indented at two spaces) or to end-of-file.
///
/// Returns `None` if the job is not declared.
fn policy_image_build_job_block(body: &str) -> Option<String> {
    let key = "\n  policy-image-build:";
    let start = body.find(key)?;
    // Skip the leading newline so the returned block begins at the
    // `  policy-image-build:` line itself.
    let block_start = start + 1;
    // Find the next sibling job key (two-space indent + name + colon)
    // or end-of-file. A sibling key looks like `\n  <name>:` where
    // `<name>` does not start with whitespace and the line is not
    // indented further than two spaces. The simplest heuristic that
    // matches the actual workflow shape is "next occurrence of `\n  `
    // followed by a non-space char that introduces another job key".
    let rest = &body[block_start + key.len() - 1..];
    let mut cursor = 0usize;
    let end_relative = loop {
        let Some(idx) = rest[cursor..].find("\n  ") else {
            break rest.len();
        };
        let abs = cursor + idx + 3; // position after "\n  "
        // Peek the character at `abs`: if it's a space, we're inside a
        // deeper-indented step/key — keep scanning. If it's a non-space
        // and we can find a `:` before the next newline, that's the
        // next sibling job key — stop here.
        let next_char = rest.as_bytes().get(abs).copied();
        match next_char {
            Some(b' ') | Some(b'\t') => {
                cursor = abs;
                continue;
            }
            Some(_) => {
                // Confirm a `:` appears on this line before a newline.
                let line_end = rest[abs..]
                    .find('\n')
                    .map(|n| abs + n)
                    .unwrap_or(rest.len());
                if rest[abs..line_end].contains(':') {
                    break cursor + idx;
                }
                cursor = abs;
            }
            None => break rest.len(),
        }
    };
    Some(rest[..end_relative].to_string())
}

#[test]
fn policy_image_build_job_is_declared_in_workflow() {
    let body = read_workflow();
    // AC: "A new job in `.github/workflows/ci.yml` runs `docker build`
    // against `policy-image/Dockerfile`."
    // AC: "The new job's name is distinguishable from `ci` in the PR
    // check list (e.g. `policy-image-build`)."
    assert!(
        body.contains("\n  policy-image-build:"),
        "CI workflow must declare a top-level job keyed exactly \
         `policy-image-build` so the status check surfaces under that \
         name in branch protection (distinct from the cargo `ci` job). \
         Got:\n{}",
        body,
    );
}

#[test]
fn policy_image_build_job_is_top_level_parallel_to_ci() {
    let body = read_workflow();
    // AC: "The new job runs in parallel with the existing `ci` job
    // (separate `jobs:` entry, not added as a step to `ci`)."
    //
    // In GitHub Actions, jobs at the same indentation level under
    // `jobs:` run in parallel by default (no `needs:` dependency).
    // The brief explicitly forbids adding it as a step under `ci`.
    //
    // Pin both: (1) the new job is a sibling of `ci` (two-space
    // indent), and (2) it does NOT declare `needs: ci` (which would
    // serialise them).
    let job_block = policy_image_build_job_block(&body).unwrap_or_else(|| {
        panic!(
            "policy-image-build job must be a top-level entry under \
             `jobs:` (sibling of `ci`). Got:\n{}",
            body,
        )
    });
    assert!(
        !job_block.contains("needs: ci")
            && !job_block.contains("needs: [ci]")
            && !job_block.contains("needs:\n      - ci"),
        "policy-image-build must NOT declare `needs: ci` (the brief \
         requires parallel execution; a cargo-test failure should not \
         prevent the docker build from running). Got job block:\n{}",
        job_block,
    );
}

#[test]
fn policy_image_build_job_triggers_match_existing_job() {
    let body = read_workflow();
    // AC: "Triggers match the existing job: `pull_request` and `push`
    // to `master`."
    //
    // Triggers are workflow-level in this file, so both jobs share
    // them by construction. Pin the existing `on:` block continues
    // to carry both triggers (regression-guard against a drive-by
    // edit that scopes triggers per-job and forgets one).
    assert_contains_all(
        &body,
        &["pull_request:", "push:", "- master"],
        "workflow-level triggers (shared by ci and policy-image-build)",
    );
}

#[test]
fn policy_image_build_job_runs_docker_build_against_policy_image_dockerfile() {
    let body = read_workflow();
    let job_block = policy_image_build_job_block(&body).unwrap_or_else(|| {
        panic!("policy-image-build job must exist. Got:\n{}", body)
    });
    // AC: "A new job in `.github/workflows/ci.yml` runs `docker build`
    // against `policy-image/Dockerfile`."
    //
    // Brief: "The build's `context` is `policy-image/` and the `file`
    // is `policy-image/Dockerfile` (matching the existing operator-side
    // invocation in `bellows run`)."
    //
    // Allow either the `docker/build-push-action@v5` pattern (with
    // `context:` and `file:` keys) or a direct `docker buildx build`
    // invocation. Pin only the load-bearing path strings so the
    // workflow can use whichever idiom.
    assert!(
        job_block.contains("policy-image/Dockerfile"),
        "policy-image-build must reference `policy-image/Dockerfile` \
         explicitly (the build's `file:` or `-f` argument). Got job \
         block:\n{}",
        job_block,
    );
    assert!(
        job_block.contains("policy-image/")
            || job_block.contains("policy-image\n"),
        "policy-image-build must use `policy-image/` as the build \
         context (matching the operator-side invocation in `bellows \
         run`). Got job block:\n{}",
        job_block,
    );
}

#[test]
fn policy_image_build_job_uses_gha_layer_cache() {
    let body = read_workflow();
    let job_block = policy_image_build_job_block(&body).unwrap_or_else(|| {
        panic!("policy-image-build job must exist. Got:\n{}", body)
    });
    // AC: "Docker layer caching uses `type=gha` (cold-start hits only
    // when base images or pinned versions change)."
    //
    // Brief: "Standard GitHub Actions docker-build patterns:
    // `docker/setup-buildx-action@v3` + `docker/build-push-action@v5`
    // with `cache-from: type=gha` and `cache-to: type=gha,mode=max`,
    // or equivalent direct `docker buildx build` invocation with the
    // same cache flags."
    //
    // Buildx is required for GHA cache to work; pin both pieces.
    assert!(
        job_block.contains("docker/setup-buildx-action"),
        "policy-image-build must set up buildx (required for \
         `type=gha` cache). Got job block:\n{}",
        job_block,
    );
    assert!(
        job_block.contains("type=gha"),
        "policy-image-build must use `type=gha` layer caching (brief \
         AC). Got job block:\n{}",
        job_block,
    );
    // `cache-to: type=gha,mode=max` is the load-bearing direction —
    // `mode=max` writes intermediate layers, not just the final
    // image, so a future PR that only touches one RUN step still
    // benefits from the earlier layers.
    assert!(
        job_block.contains("mode=max"),
        "policy-image-build must use `cache-to: type=gha,mode=max` so \
         intermediate layers are cached (brief: warm runs ~2-3 min). \
         Got job block:\n{}",
        job_block,
    );
}

#[test]
fn policy_image_build_job_has_cold_start_timeout() {
    let body = read_workflow();
    let job_block = policy_image_build_job_block(&body).unwrap_or_else(|| {
        panic!("policy-image-build job must exist. Got:\n{}", body)
    });
    // AC: "Job-level timeout accommodates cold-start (~15 min) without
    // being so generous it masks a hanging build."
    //
    // Brief gives ~8 min cold-start (npm pack + GitHub release
    // download + rustup + apt-get install) and ~2-3 min warm. A 15
    // min timeout leaves headroom for slow GHA runners without
    // letting a hung build burn an hour of CI minutes.
    let timeout_line = job_block
        .lines()
        .find(|line| line.contains("timeout-minutes:"))
        .unwrap_or_else(|| {
            panic!(
                "policy-image-build must declare `timeout-minutes:` \
                 (brief AC: cold-start ~15 min). Got job block:\n{}",
                job_block,
            )
        });
    let value: u32 = timeout_line
        .split(':')
        .nth(1)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(|| {
            panic!(
                "policy-image-build timeout-minutes must be an integer. \
                 Got line: {:?}",
                timeout_line,
            )
        });
    assert!(
        (10..=30).contains(&value),
        "policy-image-build timeout-minutes must be in [10, 30] (brief: \
         ~15 min, generous enough for cold-start but tight enough to \
         catch a hang). Got: {}",
        value,
    );
}

#[test]
fn policy_image_build_job_runs_on_ubuntu_latest() {
    let body = read_workflow();
    let job_block = policy_image_build_job_block(&body).unwrap_or_else(|| {
        panic!("policy-image-build job must exist. Got:\n{}", body)
    });
    // The Dockerfile pins linux/amd64 binaries (codex tarball is
    // `x86_64-unknown-linux-musl`), so the job MUST run on a Linux
    // runner. ubuntu-latest matches the existing `ci` job and the
    // brief's "no multi-platform builds" out-of-scope note.
    assert!(
        job_block.contains("runs-on: ubuntu-latest"),
        "policy-image-build must run on `ubuntu-latest` (Linux/amd64 \
         matches the Dockerfile's pinned codex binary and the existing \
         `ci` job). Got job block:\n{}",
        job_block,
    );
}

#[test]
fn policy_image_build_job_does_not_push_image() {
    let body = read_workflow();
    let job_block = policy_image_build_job_block(&body).unwrap_or_else(|| {
        panic!("policy-image-build job must exist. Got:\n{}", body)
    });
    // Brief out-of-scope: "Pushing the built image anywhere (registry,
    // artifact storage). Integrity verification is the goal; the
    // production image rebuild remains `bellows run`'s job."
    //
    // `docker/build-push-action@v5` defaults to `push: false`, but
    // pin the absence of `push: true` explicitly so a drive-by edit
    // does not silently start publishing.
    assert!(
        !job_block.contains("push: true"),
        "policy-image-build must NOT push the image (brief out-of-scope: \
         integrity verification only, no registry publish). Got job \
         block:\n{}",
        job_block,
    );
}

#[test]
fn policy_image_build_job_checks_out_repository() {
    let body = read_workflow();
    let job_block = policy_image_build_job_block(&body).unwrap_or_else(|| {
        panic!("policy-image-build job must exist. Got:\n{}", body)
    });
    // The build context is `policy-image/`, which only exists in the
    // repo, so the job must `actions/checkout` first.
    assert!(
        job_block.contains("actions/checkout@v4"),
        "policy-image-build must check out the repository (pinned to \
         @v4 for supply-chain hygiene, matching the existing `ci` \
         job). Got job block:\n{}",
        job_block,
    );
}
