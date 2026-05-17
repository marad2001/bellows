//! AC13 of issue #120: the `policy-image-gen` binary is the canonical
//! source for the opencode install commands the policy-image Dockerfile
//! must contain (version pin + sha256 pin + verified local tarball
//! install line).
//!
//! AC13's purpose: lock the opencode install snippet in Rust code so
//! the Dockerfile in AC14 has a mechanically-verifiable target to
//! match. A drive-by Dockerfile edit that bumps the opencode version
//! without also updating the binary's pinned constants will flip an
//! AC14 file-content test red on the next `cargo test` run.
//!
//! The binary itself is intentionally a thin wrapper around a library
//! helper so the helper is unit-testable from integration tests without
//! shelling out.

use bellows::policy_image_gen::{
    opencode_install_snippet, OPENCODE_SHA256, OPENCODE_VERSION,
};

#[test]
fn opencode_version_is_pinned_at_v1_15_3() {
    // AC13 pins the opencode CLI at v1.15.3 (the version this slice
    // targets). Bumping it requires bumping OPENCODE_SHA256 in
    // lock-step — see AC14's matching Dockerfile test.
    assert_eq!(
        OPENCODE_VERSION, "1.15.3",
        "opencode pin must be v1.15.3 per ADR-0008",
    );
}

#[test]
fn opencode_sha256_is_pinned_to_the_canonical_upstream_digest() {
    // The upstream tarball digest for opencode v1.15.3. The image
    // build fails closed on a mismatch (sha256sum -c -), so a
    // drive-by version bump that forgets the digest is caught at
    // image-build time rather than silently installing a tampered
    // binary.
    assert_eq!(
        OPENCODE_SHA256,
        "be282c09f6d4fe2889b2566b48f0507c52151528490c2a67efeccbe57a7fe317",
    );
}

#[test]
fn opencode_install_snippet_mentions_pinned_version_and_sha256() {
    // The snippet is what the Dockerfile pastes verbatim. Both pins
    // must appear in the rendered text so a Dockerfile that drifts
    // away from the snippet fails AC14's text-content test.
    let snippet = opencode_install_snippet();
    assert!(
        snippet.contains(OPENCODE_VERSION),
        "snippet must name the pinned opencode version: {snippet}",
    );
    assert!(
        snippet.contains(OPENCODE_SHA256),
        "snippet must name the pinned opencode sha256: {snippet}",
    );
}

#[test]
fn opencode_install_snippet_installs_the_verified_tarball() {
    // opencode ships as an npm package (opencode-ai), but the image
    // build must install the sha256-checked tarball from /tmp rather
    // than making a second registry request after verification.
    let snippet = opencode_install_snippet();
    assert!(
        snippet.contains("npm install -g /tmp/opencode-ai-${OPENCODE_VERSION}.tgz"),
        "snippet must install the verified local tarball: {snippet}",
    );
    assert!(
        !snippet.contains("npm install -g opencode-ai@"),
        "snippet must not fetch opencode from the registry after verification: {snippet}",
    );
    assert!(
        !snippet.contains("opencode-ai@latest"),
        "opencode must be pinned (not @latest): {snippet}",
    );
}

#[test]
fn opencode_install_snippet_pins_via_sha256sum_check() {
    // Defence-in-depth: the snippet must call `sha256sum -c -` (or
    // equivalent) against OPENCODE_SHA256 so a tampered npm tarball
    // fails the image build closed, matching the codex pin style.
    let snippet = opencode_install_snippet();
    assert!(
        snippet.contains("sha256sum"),
        "snippet must verify the tarball with sha256sum: {snippet}",
    );
}
