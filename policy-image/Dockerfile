# Bellows policy image (slice 3 - real Claude agent).
#
# Layers on top of slice 2's stub by adding Node + Claude Code, the
# vendored skills (tdd, diagnose), a Bellows-authored baseline CLAUDE.md,
# and an entrypoint script that copies the baked skills + CLAUDE.md into
# Claude Code's home (where the credentials volume mounts) before exec'ing
# run-agent.
#
# Pinned to a specific Rust minor for reproducibility.
#
# TODO(slice 3.5): pin @anthropic-ai/claude-code to a specific version
# once we've smoke-tested which release works against the volume layout
# below. Slice 3 MVP uses @latest; the image's content hash still flips
# on Dockerfile/entrypoint/run-agent/skills/CLAUDE.md changes, but the
# claude version itself can drift between rebuilds.

FROM rust:1.95-slim

# Install Node (required for the Claude Code npm package) plus the
# minimum runtime tooling the agent needs alongside cargo.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        gnupg \
        git \
        openssh-client \
    && curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

# Claude Code CLI (installed system-wide as root so any user can invoke it).
RUN npm install -g @anthropic-ai/claude-code@latest

# rust:1.95-slim ships rustc + cargo but not clippy. Slice X1's cargo
# checks gate runs `cargo clippy --all-targets --all-features -- -D
# warnings` so the component must be present. Install via rustup so it
# stays in lock-step with the toolchain.
RUN rustup component add clippy

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

# Pre-create the bellows user's claude home with the right ownership so
# the named-volume mount inherits these permissions on first attach.
RUN mkdir -p /home/bellows/.claude/skills \
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
