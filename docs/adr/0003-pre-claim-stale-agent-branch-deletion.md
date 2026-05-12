# Pre-claim deletion of stale agent/* branches on origin

Bellows runs that fail mid-pipeline can leave behind a remote
`agent/<N>-<slug>` branch with no open PR (operator closed it without
`--delete-branch`, or bellows crashed before opening the PR at all).
When the operator re-applies `ready-for-agent` and bellows reclaims
the issue, the local agent branch is created fresh from the default
branch and diverges from the stale remote tip — the subsequent
`git push -u origin agent/<N>-<slug>` fails non-fast-forward, the
pipeline crashes after the agent has already spent its work, and the
issue ends up stuck at `agent-in-progress` with no PR. The AFK
contract breaks on every recurrence.

We add a pre-claim sweep to the polling loop (extending slice b's
existing pre-claim PR check): before claiming an issue, list refs on
origin matching the anchored prefix `agent/<N>-*` and DELETE every
match via `DELETE /repos/.../git/refs/heads/...`. 404 (branch already
gone) is treated as success. Any other deletion failure surfaces as
a `RunOutcome::Blocked` outcome via slice b's existing block shape —
bellows refuses to claim, the next poll tick retries, the operator
sees the error if it persists. `agent/*` is bellows-owned by
convention (the same convention slice b's pre-claim PR check, the
auto-merge workflow's filter, and `agent_branch_name` already
enforce), so deleting `agent/*` refs is bellows exercising ownership
of its own namespace rather than touching foreign state.

## Considered alternatives

- **Pre-push reactive recovery: detect non-fast-forward at push time and react.** Rejected: defers the destructive operation until after the agent has already run for ~30 minutes and spent the tokens. Self-healing should fire before the work, not after it.
- **Refuse to claim and surface a block: operator deletes the stale branch manually.** Rejected: same shape as slice b's pre-claim PR block, but defeats the AFK goal — every recurrence requires operator intervention. Acceptable as a fallback when the deletion call itself fails, not as the default.
- **Force-push instead of delete + recreate.** Rejected at this timing: equivalent on the remote but defers the destructive op to push time (same problem as the reactive option). Force-push as a recovery primitive at push time is a separable design — this slice resolves before claim.
- **Mount operator's personal SSH key / use a different auth path for branch deletion.** Rejected: the existing `BELLOWS_GITHUB_TOKEN` already has `Contents: write` which covers branch deletion. No new credential surface needed.
- **Exact-slug matching (`agent/<N>-<current-derived-slug>` only) instead of anchored prefix.** Rejected: leaks orphan branches when the issue's title is edited between runs (different slug → no match → orphan). Anchored-prefix sweeps the whole `agent/<N>-*` namespace clean on every reclaim.
- **Opt-in via config flag (default off = refuse-to-claim).** Rejected: the AFK contract is the design intent for the current operator. Operators with unusual control requirements can ask for opt-out if it ever becomes a need.

## Consequences

- Closing a bellows PR signals "abandon — bellows can reclaim and overwrite." Operators who want to preserve an `agent/*` branch's work must rebase it onto a non-`agent/*` branch first.
- The pre-claim phase gains one `list refs` API call per poll tick per configured `[[repo]]` (multi-repo). Under expected bellows load this is negligible against the GitHub 5000/hr PAT budget.
- Branch protection on `agent/*` (e.g. an operator configuring "no deletions") would permanently block bellows. Documented in the README as a known interaction.
- The ADR-0001 auto-merge workflow's branch filter (`head.ref` starts with `agent/`) and slice b's pre-claim PR check, together with this slice, formalise the `agent/*` namespace as bellows-owned: bellows creates, force-overwrites, and deletes branches there as a matter of routine.
- Failure mode for the deletion API call composes with slice b's existing `RunOutcome::Blocked` shape — no new variant needed, status-file + log integration is automatic.
