//! Slice 4 — per-repo cache volumes.
//!
//! Every agent container should mount a per-repo `target/` named volume
//! and a shared cargo-registry named volume so subsequent runs on the
//! same repo are warm-cache rather than cold. These tests pin the
//! pure functions that derive the volume name + slug from a repo URL,
//! plus the README note that flags the cold-cache risk on first run.

use bellows::sandbox::CARGO_REGISTRY_VOLUME_NAME;
use bellows::{repo_slug, repo_target_volume_name};

#[test]
fn repo_slug_for_standard_github_owner_repo() {
    // Happy path: a plain GitHub URL slugifies to `<owner>-<repo>` in
    // lowercase ASCII alphanumerics + hyphens. The slug feeds the
    // `bellows-repo-slug` label and the per-repo volume name; both
    // need a docker-safe identifier with no slashes.
    assert_eq!(
        repo_slug("https://github.com/marad2001/bellows"),
        "marad2001-bellows",
    );
}

#[test]
fn repo_slug_lowercases_owner_and_repo() {
    // Docker volume names are case-sensitive on Linux but a mixed-case
    // input shouldn't produce two distinct cache volumes for the same
    // repo (e.g. an operator typing the URL in two different cases
    // would otherwise blow the warm-cache benefit). Pin lowercase.
    assert_eq!(
        repo_slug("https://github.com/MarAd2001/Bellows"),
        "marad2001-bellows",
    );
}

#[test]
fn repo_slug_replaces_dots_in_repo_name_with_hyphens() {
    // Repo names commonly contain dots (`foo.rs`, `lib.bar`). Docker
    // volume names allow dots, but the brief asks for the slug to be
    // restricted to alphanumerics + hyphens + underscores so the
    // label value doesn't smuggle a "." into prune-side regex matches.
    assert_eq!(
        repo_slug("https://github.com/foo/bar.baz"),
        "foo-bar-baz",
    );
}

#[test]
fn repo_slug_strips_trailing_dot_git_from_clone_url() {
    // `https://github.com/foo/bar.git` is a canonical clone URL form;
    // including ".git" in the slug would produce a different volume
    // for `bar.git` vs `bar` against the same repo. Drop the suffix.
    assert_eq!(repo_slug("https://github.com/foo/bar.git"), "foo-bar");
}

#[test]
fn repo_target_volume_name_prefixes_slug_with_bellows_target() {
    // The volume name convention `bellows-target-<slug>` is the
    // discovery key for the future `bellows prune` command on the
    // per-repo target volume. Pin the prefix.
    assert_eq!(
        repo_target_volume_name("https://github.com/marad2001/bellows"),
        "bellows-target-marad2001-bellows",
    );
}

#[test]
fn cargo_registry_volume_name_is_shared_constant() {
    // The shared cargo registry volume is one volume across every
    // repo bellows manages on this host. Pin the name so the prune
    // tooling has one literal to match against.
    assert_eq!(CARGO_REGISTRY_VOLUME_NAME, "bellows-cargo-registry");
}
