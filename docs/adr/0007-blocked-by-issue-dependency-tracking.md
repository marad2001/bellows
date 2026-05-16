# Blocked-by issue dependency tracking

Bellows's polling loop today lists every open issue carrying
`ready-for-agent` across configured `[[repo]]`s and claims the
globally-oldest by GitHub `created_at`. Dependent issues — whose
agent brief carries a `**Blocked by:** #N` line because the work
needs another change to land first — get claimed anyway. The agent
then either fails (the blocker hasn't landed) or produces a
half-correct attempt that has to be unwound. Operators currently
defer dependents manually by stripping `ready-for-agent` and
re-adding it once the blocker lands; the `**Blocked by:**` line in
the brief is informational only.

We introduce a single label, `blocked-by`, that triage applies to
any dependent. The normal polling pass filters the candidate set to
`ready-for-agent AND NOT blocked-by` and, from the filtered set,
claims the lowest-`issue.number` candidate (with `created_at` as the
cross-repo tie-breaker and `[[repo]]` declared order as the final
tie-breaker). When the filtered set is empty AND at least one
`blocked-by`-labelled issue exists, the same polling tick runs a
re-loop sweep: for each `blocked-by` issue it fetches the brief,
parses the `**Blocked by:**` line, queries each named blocker's
state, and strips the `blocked-by` label iff every named blocker is
`CLOSED`. A cleared dependent becomes claimable on the next tick's
normal pass. The re-loop logs a single summary line per sweep
(`bellows: re-loop swept N blocked-by issues, cleared M`) with
per-issue detail relegated to the debug-level log file so an
operator can diagnose stuck dependents without spamming the
foreground.

## Considered alternatives

- **Per-blocker label shape (`blocked-by:95`, `blocked-by:96`).** Rejected: the label-set explodes as a project's dependency graph grows, every blocker landing requires bellows to delete one label and possibly add others, and the polling-loop filter has to substring-match label names rather than do a set-membership check. The single-label shape costs one extra API call per `blocked-by` issue at re-loop time to fetch the brief, but it keeps the label set bounded and the filter a clean equality check. The trade-off favours the single-label shape: re-loop is the cold path (runs only when the unblocked queue is dry), the brief is already fetched for every claim anyway so the parsing logic exists, and operators reason about one label rather than a per-issue set.
- **Separate timer for the re-loop pass (every Nth tick regardless of queue state).** Rejected: doubles the polling cadence's surface area (the operator now has two intervals to reason about and tune), and burns API quota on a sweep that is wasteful when there's claimable work waiting. Re-loop-on-empty-queue lets bellows do useful work whenever it can and only spends the API budget on dependency reconciliation when it would otherwise be idle. The trade-off is that a `blocked-by` issue whose blocker lands during a long busy stretch waits one extra tick after the queue drains before it gets considered — acceptable given the polling cadence (45s default) and the fact that the operator can manually strip `blocked-by` to short-circuit if the wait matters.
- **Distinguishing closure reasons (PR-merged vs wontfix vs manual close).** Rejected: trusts the operator's intent. If a blocker is `wontfix`, the dependent's brief premise may no longer hold — but that's an editorial question the operator is better positioned to answer than bellows. The simpler "any CLOSED state clears" rule means bellows doesn't have to special-case GitHub's three closure reasons (merged-via-PR, manual close, not-planned/wontfix), and the operator who closed the blocker as `wontfix` has the option of re-applying `blocked-by` manually if they want bellows to wait. Sweeping on any-CLOSED is one branch in the logic instead of three.
- **Cross-repo blocker references (`owner/name#N` grammar in the brief parser).** Out of scope for v1 — adds a GitHub API call per cross-repo reference (to a repo the same PAT may not have read access to), and the multi-repo dependency-graph semantics need their own design pass (e.g. does a wontfix in repo A still clear a dependent in repo B?). v1 logs cross-repo references as a warning and treats them as if absent. Listed as a known limitation; future enhancement if it becomes operationally common.
- **Cycle detection between `blocked-by` issues.** Out of scope for v1 — cycles (A blocks B, B blocks A) need the operator's editorial input to resolve (one of the two blockers is wrong), so bellows detecting the cycle would still hand it back to the operator. v1 leaves both labelled `blocked-by` indefinitely; the operator notices the stuck pair on the next time they check the backlog and resolves manually. Listed as a known limitation; future enhancement if cycles become common operationally.
- **Re-applying `blocked-by` when a previously-cleared blocker is reopened.** Out of scope. Once the `blocked-by` label is stripped, the dependent is treated as a normal `ready-for-agent` issue. If the blocker reopens, the operator notices and re-applies `blocked-by` manually. Re-applying automatically would require bellows to track which issues it has previously cleared (extra state, edge cases around state loss) and would still race the polling loop in any case.

## Why re-loop only when the unblocked queue is empty

The re-loop sweep is the cold path. The normal pass should do its
work and exit; only when there is no normal-pass work to do does it
make sense to spend API quota reconciling dependency state. This
keeps the bellows-on-bellows API budget within the PAT's hourly
cap (5000/hr) on a busy repo: a steady stream of unblocked work
costs one list-issues call per tick per repo, and the re-loop
sweep's extra brief-fetch + blocker-state-check calls only kick in
when there's nothing else to claim. The operator-visible cost is
one extra tick of latency for a `blocked-by` issue whose blocker
landed during a busy stretch: the queue has to drain before the
sweep runs and clears the label, and the cleared dependent then
waits one more tick for the next normal pass to pick it up.
Acceptable at the 45s default cadence; tunable downward if it ever
matters.

## Known limitations

- **Cross-repo blocker references** (`owner/name#N`): not supported in v1. Parser logs a warning and treats the reference as absent. A dependent whose `**Blocked by:**` line contains only cross-repo references is treated as having no parseable blockers and gets its label stripped on the next sweep.
- **Cycles between `blocked-by` issues**: not detected in v1. Both dependents remain labelled indefinitely; the operator resolves manually.
- **Re-blocking after reopen**: a dependent whose blocker has been closed-then-reopened is not automatically re-blocked. The operator must re-apply `blocked-by` manually.
