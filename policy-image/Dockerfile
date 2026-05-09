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
    && curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

# Claude Code CLI.
RUN npm install -g @anthropic-ai/claude-code@latest

# Baked policy: skills + CLAUDE.md live here. The entrypoint copies them
# into /root/.claude/ at container start so they layer correctly with the
# OAuth state in the credentials volume.
RUN mkdir -p /opt/bellows-policy/skills
COPY skills/ /opt/bellows-policy/skills/
COPY CLAUDE.md /opt/bellows-policy/CLAUDE.md

COPY entrypoint /usr/local/bin/entrypoint
COPY run-agent /usr/local/bin/run-agent
RUN chmod +x /usr/local/bin/entrypoint /usr/local/bin/run-agent

WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/entrypoint"]
