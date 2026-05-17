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


<!-- bellows weak-test guard appended this entry because the implement phase produced changes against the base branch with no new Rust test attributes (#[test], #[tokio::test], etc.) and the issue did not carry the configurable skip-label. The presence of this entry forces the run to agent-self-reported-failure (draft PR + agent-failed label) so a human reviewer sees the gap. -->

## Unaddressed finding: no new tests added

Bellows-synthesised entry. The implement phase produced a diff against the base branch with no new Rust test attributes detected by the slice-8 weak-test guard. A green cargo-checks gate over an unchanged test suite is a poor signal of correctness; the brief's acceptance criteria typically require accompanying tests. The weak-test guard synthesised this entry so the run routes to agent-self-reported-failure for a human reviewer.
