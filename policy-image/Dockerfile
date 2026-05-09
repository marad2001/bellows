# Bellows policy image (slice 2 - stub).
#
# Slice 2 is the smallest sandbox shape: Rust toolchain + a stub agent script
# that writes a marker file. Slice 3 (#5) will add the Claude Code binary,
# the curated skills, and the baseline ~/.claude/CLAUDE.md.
#
# Pinned to a specific Rust minor for reproducibility; the brief explicitly
# disallows the `latest` tag.

FROM rust:1.95-slim

# Stub agent: writes /workspace/.bellows-stub-marker and exits 0.
COPY run-agent /usr/local/bin/run-agent
RUN chmod +x /usr/local/bin/run-agent

WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/run-agent"]
