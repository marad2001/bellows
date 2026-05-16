//! AC13 of issue #120: the `policy-image-gen` binary is the canonical
//! source for the opencode install commands the policy-image Dockerfile
//! must contain (version pin + sha256 pin + npm install line).
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
        "f8ae8678c9bccdbaf99777f36ff2d5efe689d473384f2e94b84d6cda256d2540",
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
fn opencode_install_snippet_uses_npm_install_g_with_pinned_version() {
    // opencode ships as an npm package (opencode-ai). The snippet
    // must install it globally pinned to OPENCODE_VERSION rather
    // than @latest so day-to-day image rebuilds are reproducible.
    let snippet = opencode_install_snippet();
    assert!(
        snippet.contains("npm install -g opencode-ai@"),
        "snippet must use `npm install -g opencode-ai@<version>`: {snippet}",
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
