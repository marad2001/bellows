//! Canonical source for the opencode install snippet baked into the
//! policy image. AC13 of issue #120 / ADR-0008: the policy image's
//! Dockerfile must pin opencode at a known version and verify the
//! tarball/package against a known sha256 so the image build fails
//! closed on a tampered or drifted install.
//!
//! Treating this as a Rust constant (rather than a free-form
//! Dockerfile string) lets AC14's Dockerfile content-test mechanically
//! verify that the rendered snippet appears verbatim in
//! `policy-image/Dockerfile`. Bumping opencode's version is a
//! lock-step edit of `OPENCODE_VERSION` and `OPENCODE_SHA256` here
//! plus the matching `RUN` line in the Dockerfile.

/// Pinned opencode CLI version. Bump in lock-step with
/// [`OPENCODE_SHA256`].
pub const OPENCODE_VERSION: &str = "1.15.3";

/// sha256 of the upstream opencode-ai npm tarball at
/// [`OPENCODE_VERSION`]. Recompute via
/// `npm pack opencode-ai@<version> && sha256sum opencode-ai-*.tgz`
/// when bumping the version.
pub const OPENCODE_SHA256: &str =
    "f8ae8678c9bccdbaf99777f36ff2d5efe689d473384f2e94b84d6cda256d2540";

/// Render the canonical Dockerfile `RUN` snippet that installs
/// opencode and verifies its tarball against the pinned sha256. AC14's
/// Dockerfile pastes the body of this snippet verbatim.
pub fn opencode_install_snippet() -> String {
    format!(
        "# OpenCode CLI, pinned (issue #120 / ADR-0008). Installed via\n\
         # `npm install` against a sha256-verified tarball so a tampered\n\
         # registry response fails the image build closed rather than\n\
         # silently installing a binary that would inherit the agent's\n\
         # container privileges at every opencode-engine phase.\n\
         #\n\
         # Bump workflow: when raising OPENCODE_VERSION, recompute\n\
         # OPENCODE_SHA256 from `npm pack opencode-ai@<version> &&\n\
         # sha256sum opencode-ai-*.tgz`; the two ARGs must move\n\
         # together.\n\
         ARG OPENCODE_VERSION={OPENCODE_VERSION}\n\
         ARG OPENCODE_SHA256={OPENCODE_SHA256}\n\
         RUN set -eux; \\\n\
             npm pack opencode-ai@${{OPENCODE_VERSION}} --pack-destination /tmp; \\\n\
             echo \"${{OPENCODE_SHA256}}  /tmp/opencode-ai-${{OPENCODE_VERSION}}.tgz\" | sha256sum -c -; \\\n\
             npm install -g opencode-ai@${{OPENCODE_VERSION}}; \\\n\
             rm -f /tmp/opencode-ai-${{OPENCODE_VERSION}}.tgz\n"
    )
}
