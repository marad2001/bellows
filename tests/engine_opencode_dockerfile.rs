//! AC14 of issue #120: the policy-image Dockerfile installs the
//! opencode CLI (pinned version + sha256-verified tarball) so the
//! image-level dispatch in AC15 can invoke `opencode run` against
//! the DeepSeek backend.
//!
//! The Dockerfile must paste the canonical install snippet rendered
//! by AC13's `bellows::policy_image_gen::opencode_install_snippet()`
//! so the install commands have a single source of truth. A drive-by
//! Dockerfile edit that desyncs from the snippet flips these tests
//! red, and a version bump in the snippet flips them red until the
//! Dockerfile is updated in lock-step.

use bellows::policy_image_gen::{
    opencode_install_snippet, OPENCODE_SHA256, OPENCODE_VERSION,
};

fn read_dockerfile() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("policy-image")
        .join("Dockerfile");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e))
}

#[test]
fn dockerfile_pins_opencode_version_via_arg() {
    let dockerfile = read_dockerfile();
    let expected = format!("ARG OPENCODE_VERSION={OPENCODE_VERSION}");
    assert!(
        dockerfile.contains(&expected),
        "Dockerfile must declare `{expected}` so the install is reproducible: {dockerfile}",
    );
}

#[test]
fn dockerfile_pins_opencode_sha256_via_arg() {
    let dockerfile = read_dockerfile();
    let expected = format!("ARG OPENCODE_SHA256={OPENCODE_SHA256}");
    assert!(
        dockerfile.contains(&expected),
        "Dockerfile must declare `{expected}` so the npm tarball is sha-pinned: {dockerfile}",
    );
}

#[test]
fn dockerfile_verifies_opencode_tarball_via_sha256sum() {
    let dockerfile = read_dockerfile();
    assert!(
        dockerfile.contains("sha256sum -c -"),
        "Dockerfile must verify the opencode tarball via `sha256sum -c -`: {dockerfile}",
    );
}

#[test]
fn dockerfile_installs_opencode_from_the_verified_tarball() {
    let dockerfile = read_dockerfile();
    assert!(
        dockerfile.contains("npm install -g /tmp/opencode-ai-${OPENCODE_VERSION}.tgz"),
        "Dockerfile must install the sha256-verified opencode tarball: {dockerfile}",
    );
    assert!(
        !dockerfile.contains("npm install -g opencode-ai@"),
        "Dockerfile must not fetch opencode from the registry after verification: {dockerfile}",
    );
    assert!(
        !dockerfile.contains("opencode-ai@latest"),
        "opencode must be pinned (not @latest): {dockerfile}",
    );
}

#[test]
fn dockerfile_contains_the_full_snippet_from_policy_image_gen() {
    // The snippet rendered by policy_image_gen is the canonical
    // source. The Dockerfile must contain it verbatim so a drift
    // between the snippet and the Dockerfile fails the build.
    let dockerfile = read_dockerfile();
    let snippet = opencode_install_snippet();
    assert!(
        dockerfile.contains(&snippet),
        "Dockerfile must paste the policy-image-gen snippet verbatim. \
         Run `cargo run --bin policy-image-gen` and paste the output \
         into policy-image/Dockerfile.\n--- expected snippet ---\n{snippet}\n--- dockerfile ---\n{dockerfile}",
    );
}
