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
    // restricted to alphanumerics + hyphens so the label value doesn't
    // smuggle a "." into prune-side regex matches.
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

// --- Permission-prep invariants for the cache-volume mount points ---
//
// `build_cache_mounts` attaches `/workspace/target` and
// `/usr/local/cargo/registry` as named volumes. Docker creates a fresh
// named volume's `_data` directory as `root:root` mode 0755 whenever
// the mount target does not exist in the image (the workspace bind
// mount shadows the first path; the base image never `mkdir`s the
// second). The bellows user (uid 1000) then cannot write — cargo's
// first write into either path would fail with EACCES on a fresh
// repo / fresh host.
//
// The fix lives in `policy-image/Dockerfile` + `policy-image/entrypoint`:
// the container starts as root, the entrypoint chowns both mount
// points, then drops to bellows via `runuser` before exec'ing the
// per-flow user-mode script. The tests below pin that contract so a
// future edit can't silently reintroduce the regression.

fn read_policy_image_file(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("policy-image")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e))
}

#[test]
fn dockerfile_does_not_set_user_bellows_before_entrypoint() {
    // Setting `USER bellows` in the Dockerfile would make the
    // entrypoint run as bellows, and bellows cannot chown a
    // root-owned named-volume mount point. The policy is that the
    // container starts as root and the entrypoint drops privileges
    // itself after chowning. Pin the absence of `USER bellows` in
    // the Dockerfile to catch a copy-paste revert.
    let dockerfile = read_policy_image_file("Dockerfile");
    for (lineno, line) in dockerfile.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        assert!(
            !trimmed.starts_with("USER bellows"),
            "policy-image/Dockerfile line {}: container must NOT switch to USER bellows before \
             the entrypoint — the entrypoint needs root to chown cache-volume mount points. \
             See entrypoint script for the runtime drop to bellows.",
            lineno + 1,
        );
    }
}

#[test]
fn dockerfile_pre_creates_and_chowns_cargo_registry_path() {
    // `/usr/local/cargo/registry` is not in the rust:1.95-slim base
    // image — cargo creates it lazily on first registry fetch. Docker
    // propagates the image's mount-target permissions into a fresh
    // named volume's _data dir on first attach, so pre-creating this
    // path with `chown bellows:bellows` is what makes the registry
    // volume writable on the very first cargo fetch. The runtime
    // chown in the entrypoint is a belt-and-braces backstop, but the
    // image-level pre-create is the primary mechanism.
    let dockerfile = read_policy_image_file("Dockerfile");
    assert!(
        dockerfile.contains("/usr/local/cargo/registry"),
        "Dockerfile must reference /usr/local/cargo/registry to pre-create + chown it:\n{}",
        dockerfile,
    );
    assert!(
        dockerfile.contains("chown bellows:bellows /workspace/target /usr/local/cargo/registry")
            || (dockerfile.contains("chown") && dockerfile.contains("bellows:bellows")),
        "Dockerfile must chown the pre-created cache paths to bellows:\n{}",
        dockerfile,
    );
}

#[test]
fn entrypoint_chowns_both_cache_volume_mount_points() {
    // The entrypoint runs as root on container start and must chown
    // BOTH cache-volume mount points before dropping to bellows. A
    // partial chown (one path but not the other) would leave one
    // cache silently broken — pin both paths in one assertion.
    let entrypoint = read_policy_image_file("entrypoint");
    assert!(
        entrypoint.contains("chown"),
        "entrypoint must chown the cache-volume mount points:\n{}",
        entrypoint,
    );
    assert!(
        entrypoint.contains("/workspace/target"),
        "entrypoint must chown /workspace/target — Docker creates this named-volume _data dir as \
         root:root, and the workspace bind mount shadows any image-level pre-create so the chown \
         must happen at runtime:\n{}",
        entrypoint,
    );
    assert!(
        entrypoint.contains("/usr/local/cargo/registry"),
        "entrypoint must chown /usr/local/cargo/registry (runtime backstop):\n{}",
        entrypoint,
    );
}

#[test]
fn entrypoint_drops_privileges_to_bellows_after_chown() {
    // After the root-mode chown the entrypoint must drop to the
    // bellows user before exec'ing the user-mode continuation —
    // Claude Code refuses to honour --dangerously-skip-permissions
    // when running as root, so a failure to drop here would surface
    // as a runtime refusal from claude rather than a chown error.
    // Pin both the drop mechanism (`runuser -u bellows`) and the
    // ordering (chown line precedes runuser line).
    let entrypoint = read_policy_image_file("entrypoint");
    let chown_idx = entrypoint
        .find("chown")
        .expect("entrypoint must contain a chown step");
    let runuser_idx = entrypoint
        .find("runuser -u bellows")
        .unwrap_or_else(|| {
            panic!(
                "entrypoint must drop privileges to bellows via `runuser -u bellows`:\n{}",
                entrypoint,
            )
        });
    assert!(
        chown_idx < runuser_idx,
        "entrypoint must chown BEFORE dropping to bellows (otherwise bellows can't chown):\n{}",
        entrypoint,
    );
}

#[test]
fn cargo_checks_user_mode_script_does_not_chown() {
    // `run-cargo-checks` runs as bellows (post-runuser). It must not
    // try to chown anything itself — bellows lacks the privilege and
    // the operation would fail. The entrypoint owns the chown step.
    let script = read_policy_image_file("run-cargo-checks");
    assert!(
        !script.contains("chown"),
        "run-cargo-checks runs as bellows and must not contain a chown — chowning belongs in the \
         root-mode entrypoint:\n{}",
        script,
    );
}

#[test]
fn dockerfile_installs_openssh_client_for_setup_deploy_keys() {
    // Issue #71: `bellows setup-deploy-keys add` runs a script in a
    // one-shot policy-image container that calls `ssh-keyscan` (to
    // seed `/sshvol/known_hosts`) and `ssh-keygen -F` (to check whether
    // the host is already seeded). Both come from the `openssh-client`
    // apt package; `rust:1.95-slim` does NOT ship them. Without the
    // package the script fails with exit 127 ("command not found")
    // inside the container — observed live on 2026-05-12, immediately
    // after the issue-#69 SSH-deploy-keys feature shipped. Pin the
    // install here so a future Dockerfile edit can't silently drop the
    // package and re-break `setup-deploy-keys add` on a fresh image.
    let dockerfile = read_policy_image_file("Dockerfile");
    assert!(
        dockerfile.contains("openssh-client"),
        "policy-image/Dockerfile must install `openssh-client` for the `setup-deploy-keys add` \
         script's `ssh-keyscan` / `ssh-keygen -F` calls. See issue #71. Current Dockerfile:\n{}",
        dockerfile,
    );
}

#[test]
fn dockerfile_enables_cargo_net_git_fetch_with_cli() {
    // Issue #77: cargo's default git client is libgit2+libssh2, which
    // does NOT honour the `IdentityFile` directive in the per-host SSH
    // config bellows seeds at `/home/bellows/.ssh/config`. libgit2 only
    // tries ssh-agent (not running in the container) and aborts. So a
    // `cargo fetch` against a private ssh:// dep fails inside the
    // sandbox even when the deploy key, ssh config, and known_hosts are
    // all correct — observed live on 2026-05-12 against
    // `marad2001/workboard-financial-advice` issue #16. The cargo error
    // itself names the fix: "if the git CLI succeeds then
    // `net.git-fetch-with-cli` may help here".
    //
    // The fix is a system-wide cargo config at
    // `/usr/local/cargo/config.toml` (CARGO_HOME for the rust:1.95-slim
    // base image) containing `git-fetch-with-cli = true` under `[net]`,
    // which makes cargo shell out to `/usr/bin/git` (installed by the
    // existing openssh-client/git apt line) for git fetches — and the
    // system git binary DOES honour the SSH config.
    //
    // Pin the literal `git-fetch-with-cli = true` in the Dockerfile so
    // a future edit can't silently drop the setting and re-break the
    // cargo-checks gate on every private-SSH-dep workspace. Match the
    // string only (not a specific filename / heredoc shape) so the
    // assertion stays robust to formatting choices.
    //
    // See: https://doc.rust-lang.org/cargo/reference/config.html#netgit-fetch-with-cli
    let dockerfile = read_policy_image_file("Dockerfile");
    assert!(
        dockerfile.contains("git-fetch-with-cli = true"),
        "policy-image/Dockerfile must enable `net.git-fetch-with-cli = true` in a system-wide \
         cargo config so cargo shells out to the system git binary (which honours the SSH \
         config) for private ssh:// deps. See issue #77. Current Dockerfile:\n{}",
        dockerfile,
    );
}
