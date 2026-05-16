# Bellows policy image (issue #81 / ADR-0005 - multi-engine).
#
# Layers on top of slice 2's stub by adding Node + Claude Code and the
# Codex CLI, the vendored skills (tdd, diagnose, triage), a Bellows-
# authored baseline CLAUDE.md, and an entrypoint script that copies the
# baked skills + CLAUDE.md into the CLIs' homes (where the credentials
# volumes mount) before exec'ing run-agent.
#
# Both CLIs are pinned (issue #81 / ADR-0005 AC: "Policy image: bakes
# both @anthropic-ai/claude-code and the Codex CLI, both pinned"):
#   - claude-code: pinned npm version, bumped deliberately as image
#     rebuilds.
#   - codex: pinned to `rust-v0.130.0`, the version covered by the
#     #79 spike findings. Installed directly from the Linux musl
#     release tarball rather than `npm install -g @openai/codex` per
#     the spike's recommendation.

FROM rust:1.95-slim

# Install Node (required for the Claude Code npm package) plus the
# minimum runtime tooling the agent needs alongside cargo. `xz-utils`
# is needed below for the codex release tarball.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        gnupg \
        git \
        openssh-client \
        xz-utils \
    && curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

# Claude Code CLI, pinned. Bumped deliberately as part of an image
# rebuild rather than drifting under `@latest` between rebuilds (the
# previous slice-3.5 TODO).
ARG CLAUDE_CODE_VERSION=2.1.0
RUN npm install -g @anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}

# Codex CLI, pinned (issue #79 spike). The Linux musl binary is
# preferred over `npm install -g @openai/codex` per the spike's
# recommendation. The release tarball is named
# `codex-x86_64-unknown-linux-musl.tar.gz` and unpacks a single
# `codex` binary; install it system-wide so any user can invoke it.
#
# The download is pinned by BOTH a version tag (CODEX_VERSION) and a
# tarball SHA-256 (CODEX_SHA256). The version tag alone is not a trust
# anchor — GitHub release assets can in principle be replaced by a
# maintainer (or anyone who compromises a maintainer account / GitHub
# release infrastructure), and the resulting binary would inherit the
# agent's container privileges at every codex-engine phase: write
# access to `/workspace`, read access to the credentials volume at
# `/home/bellows/.codex`, and the container's egress. Pinning the
# tarball hash means the image build fails closed on a mismatch
# instead of silently installing a tampered binary.
#
# Bump workflow: when raising CODEX_VERSION, also recompute
# CODEX_SHA256 from the upstream release's published asset digest
# (`gh api repos/openai/codex/releases/tags/<tag>` exposes a
# `digest: sha256:...` field per asset) or from a freshly downloaded
# tarball; the two ARGs must move together.
ARG CODEX_VERSION=rust-v0.130.0
ARG CODEX_SHA256=16779e7b7857508a768a36d7d4e084eec336ec23946ed70a9b09489b8f861190
RUN set -eux; \
    curl -fsSL -o /tmp/codex.tar.gz \
        "https://github.com/openai/codex/releases/download/${CODEX_VERSION}/codex-x86_64-unknown-linux-musl.tar.gz"; \
    echo "${CODEX_SHA256}  /tmp/codex.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/codex.tar.gz -C /tmp; \
    install -m 0755 /tmp/codex-x86_64-unknown-linux-musl /usr/local/bin/codex \
        || install -m 0755 /tmp/codex /usr/local/bin/codex; \
    rm -rf /tmp/codex.tar.gz /tmp/codex /tmp/codex-x86_64-unknown-linux-musl

# OpenCode CLI, pinned (issue #120 / ADR-0008). Installed via
# `npm install` against a sha256-verified tarball so a tampered
# registry response fails the image build closed rather than
# silently installing a binary that would inherit the agent's
# container privileges at every opencode-engine phase.
#
# Bump workflow: when raising OPENCODE_VERSION, recompute
# OPENCODE_SHA256 from `npm pack opencode-ai@<version> &&
# sha256sum opencode-ai-*.tgz`; the two ARGs must move
# together.
ARG OPENCODE_VERSION=1.15.3
ARG OPENCODE_SHA256=f8ae8678c9bccdbaf99777f36ff2d5efe689d473384f2e94b84d6cda256d2540
RUN set -eux; \
    npm pack opencode-ai@${OPENCODE_VERSION} --pack-destination /tmp; \
    echo "${OPENCODE_SHA256}  /tmp/opencode-ai-${OPENCODE_VERSION}.tgz" | sha256sum -c -; \
    npm install -g opencode-ai@${OPENCODE_VERSION}; \
    rm -f /tmp/opencode-ai-${OPENCODE_VERSION}.tgz

# rust:1.95-slim ships rustc + cargo but not clippy. Slice X1's cargo
# checks gate runs `cargo clippy --all-targets --all-features -- -D
# warnings` so the component must be present. Install via rustup so it
# stays in lock-step with the toolchain.
RUN rustup component add clippy

# Issue #77: tell cargo to use the system `git` binary for git fetches
# instead of its bundled libgit2 + libssh2. libgit2 only tries
# ssh-agent for ssh:// URLs and does NOT honour the `IdentityFile`
# directive in the per-host SSH config bellows seeds at
# `/home/bellows/.ssh/config` for private cross-repo Rust deps (see
# ADR-0002 and issues #69 / #71 / #73). The system `git` (installed
# alongside `openssh-client` above) uses `/usr/bin/ssh`, which DOES
# honour the config — so `cargo fetch` against a private ssh:// dep
# succeeds in the sandbox once `net.git-fetch-with-cli = true` is set
# system-wide. /usr/local/cargo is CARGO_HOME on rust:1.95-slim, so a
# `config.toml` there applies to every cargo invocation in every
# container spawned from this image (agent + cargo-checks gate).
#
# No-op for repos with no private SSH deps: cargo's HTTPS registry
# fetches continue to use libgit2 as before.
#
# Docs: https://doc.rust-lang.org/cargo/reference/config.html#netgit-fetch-with-cli
RUN printf '[net]\ngit-fetch-with-cli = true\n' > /usr/local/cargo/config.toml

# Non-root user for the agent run. Claude Code refuses to honour
# --dangerously-skip-permissions when running as root (and we need that
# flag for headless runs); USER bellows below switches to a constrained
# account before the entrypoint executes.
RUN useradd --create-home --shell /bin/bash --uid 1000 bellows

# Baked policy: skills + CLAUDE.md live here. The entrypoint copies them
# into the bellows user's claude home at container start so they layer
# with the OAuth state in the credentials volume.
#
# Skills baked into the image (each its own subdirectory under skills/):
#   - tdd       — red/green/refactor discipline; referenced by every
#                 bellows agent kickoff.
#   - diagnose  — debug-loop discipline for hard bugs and perf regressions.
#   - triage    — canonical source for the role taxonomy, brief templates,
#                 AI-disclaimer wording, and decision heuristics consumed
#                 by `bellows triage`. The triage kickoff prompt in
#                 `src/triage.rs` defers to this skill rather than
#                 re-implementing the taxonomy, so edits propagate on
#                 the next policy-image rebuild.
RUN mkdir -p /opt/bellows-policy/skills
COPY skills/ /opt/bellows-policy/skills/
COPY CLAUDE.md /opt/bellows-policy/CLAUDE.md

COPY entrypoint /usr/local/bin/entrypoint
COPY entrypoint-user /usr/local/bin/entrypoint-user
COPY run-agent /usr/local/bin/run-agent
COPY run-cargo-checks /usr/local/bin/run-cargo-checks
RUN chmod +x /usr/local/bin/entrypoint /usr/local/bin/entrypoint-user /usr/local/bin/run-agent /usr/local/bin/run-cargo-checks

# Pre-create the bellows user's claude AND codex homes with the right
# ownership so the per-engine named-volume mounts inherit these
# permissions on first attach (issue #81 / ADR-0005: per-engine
# credentials volumes mount at /home/bellows/.claude and
# /home/bellows/.codex respectively).
RUN mkdir -p /home/bellows/.claude/skills /home/bellows/.codex \
    && chown -R bellows:bellows /home/bellows

# Pre-create the cache-volume mount points and chown them to bellows.
# /usr/local/cargo/registry benefits directly: it's not in the base
# image, so Docker propagates the image's permissions to the volume on
# first attach. /workspace/target gets shadowed by the workspace bind
# mount at runtime so this `mkdir` doesn't reach the live mount point —
# the entrypoint's runtime `chown` is what actually fixes that one —
# but creating it here keeps the two paths visible side-by-side and
# matches what an operator inspecting the image would expect to see.
RUN mkdir -p /workspace/target /usr/local/cargo/registry \
    && chown bellows:bellows /workspace/target /usr/local/cargo/registry

# NOTE: no `USER bellows` here. The entrypoint runs as root so it can
# chown the named-volume mount points (which Docker creates as
# root:root) before dropping privileges via `runuser -u bellows`.
WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/entrypoint"]
