# Agent notes — informational channel for issue #126

These are TDD-deviation and review-aid notes that don't belong in the PR
description proper. Bellows will post `agent-notes.md` as a PR comment so the
reviewer can see them without scrolling through the commit body.

## Probe trait shape

`AgentContainerProbe` uses Rust 2024's native `async fn` in trait position
(returns `impl Future + Send`) rather than the `async-trait` crate. This keeps
the dependency surface unchanged. The trait's only method is `&self`-borrowed so
the production `BollardAgentContainerProbe<'a>` can hold a `&'a bollard::Docker`
without forcing ownership at the call site.

## Docker connection lifetime in main.rs

`main.rs` connects to Docker once before the polling loop and wraps the client in
an `Option<Docker>`. Each loop iteration constructs a fresh
`BollardAgentContainerProbe` borrow (or falls back to `NoAgentContainer` if the
initial connect failed). The fallback path is logged once at startup, not on
every tick, so a missing daemon does not flood the log.

## What's NOT in this PR (out of scope per the brief)

- Phase-8 merger / draft-PR semantics — covered by ADR-0009 but the merger
  itself is its own issue.
- Replacing `BlockReason::StaleAgentBranchDeletionFailed` — that variant is
  preserved as-is; only `OpenAgentPrs` is replaced.
- `cleanup_orphan_containers` — unchanged; it already uses the same
  `bellows-managed=true` label scheme.
