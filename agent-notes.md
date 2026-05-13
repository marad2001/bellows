## Informational note: slice #96 partially landed due to unmerged blocker

The brief for issue #96 explicitly declares **`Blocked by: #95`** at the bottom:

> Blocked by: #95 (classifier must produce `SuccessWithNotes` and apply the
> `agent-noted` label before the auto-merge filter has anything to skip;
> runner must know `SuccessWithNotes` as an `ExitReason` variant before it
> can branch on it for draft routing).

At the time of this run, slice #95 is not yet merged onto `master`. A
codebase-wide grep for `SuccessWithNotes`, `agent-noted`, `agent_noted`,
`NotesShape`, or `classify_agent_notes` finds the strings ONLY in
`docs/adr/0006-agent-notes-informational-vs-escalation.md` — i.e. nowhere
in the Rust source. `ExitReason` (src/policy.rs:29) carries the pre-#95
variants:

```
pub enum ExitReason {
    Success,
    AgentSelfReportedFailure,
    Crash,
    FinalTestsRed,
    WallClockExceeded,
    RateLimited,
    Cancelled,
}
```

The runner draft policy at src/runner.rs:1718 reads
`let draft = !matches!(reason, ExitReason::Success);` — there is no
`SuccessWithNotes` arm to branch on.

### What this slice CAN ship test-first today

The three acceptance criteria that do NOT depend on the slice-#95 classifier
are independently driveable and have been delivered red-then-green:

1. **`.github/workflows/auto-merge.yml` carries the `agent-noted` filter
   block, alongside the existing draft / head.ref / fork-repo / state /
   base-branch filters, with a comment citing ADR-0006.** Pinned by
   `tests/auto_merge.rs::auto_merge_workflow_filters_prs_labelled_agent_noted_per_adr_0006`.
   All existing safety-filter tests continue to pass — the new filter is
   additive.

2. **`Workspace` exposes
   `auto_merge_workflow_supports_agent_noted_filter() -> bool`, snapshotted
   at `prepare`/`prepare_with_gates` time by reading the target's
   `.github/workflows/auto-merge.yml`.** The substring-check contract from
   the brief (file missing → `true`; file present + contains `agent-noted`
   → `true`; file present + omits `agent-noted` → `false`; other I/O errors
   → `false`) is implemented in `detect_auto_merge_filter_support` at
   src/workspace.rs:220.

3. **`tests/workspace.rs` covers the three cases** —
   `prepare_reports_filter_supported_when_auto_merge_workflow_absent`,
   `prepare_reports_filter_supported_when_auto_merge_workflow_mentions_agent_noted`,
   `prepare_reports_filter_unsupported_when_auto_merge_workflow_omits_agent_noted`
   — plus a fourth test pinning that `prepare_with_gates` populates the
   snapshot alongside the ADR-0004 gate commands.

### What this slice CANNOT ship until #95 lands

The remaining brief acceptance criteria reference a Rust type
(`ExitReason::SuccessWithNotes`) that does not yet exist in the source.
Specifically:

- "Runner reads the workspace flag before opening the PR; for
  `SuccessWithNotes`, `draft = !workspace.auto_merge_workflow_supports_agent_noted_filter()`.
  Other variants' draft policy is unchanged."
- "When the draft-fallback path fires for `SuccessWithNotes` ... the run
  log carries a clear announce line citing ADR-0006."
- "When the target supports the filter, `SuccessWithNotes` PRs open
  non-draft with the `agent-noted` label."
- "All slice-#95 acceptance criteria for `SuccessWithNotes` (label,
  PR-body case, classifier routing) continue to hold."

A failing test for the runner-side draft routing cannot be written
test-first today because the variant it would match on is not in
`ExitReason`, so the test would fail to compile for the wrong reason
(missing variant rather than missing behaviour). The bellows kickoff
contract forbids landing source-before-test changes for behavioural
criteria, and the brief's commit-shape rules reiterate that.

The infrastructure this slice does land is exactly what slice #95 (or a
follow-up slice that adds the `SuccessWithNotes` variant) will need:

- `Workspace::auto_merge_workflow_supports_agent_noted_filter()` is the
  accessor a follow-up slice can read at the runner's `draft` decision.
- The auto-merge workflow filter is in place, so once a follow-up opens
  a `SuccessWithNotes` PR non-draft + `agent-noted`-labelled, the
  bellows-on-bellows auto-merge step will skip it.

### Recommendation to the operator

Land slice #95 first (introducing `ExitReason::SuccessWithNotes` and the
classifier routing), then land a follow-up slice that wires the runner's
draft decision to read `workspace.auto_merge_workflow_supports_agent_noted_filter()`
and emit the ADR-0006 fallback announce line. The two pieces of slice #96
that this PR DOES land (workflow filter + workspace snapshot) are stable
behind public APIs that the follow-up can call without further changes
here.

`cargo test` and `cargo clippy --all-targets --all-features -- -D warnings`
are both green on this branch.
