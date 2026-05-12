## Unaddressed finding: ba99f13 mega-commit mixes runner src changes with test fixture updates

**What would be required to address this.** Split commit `ba99f13` into two commits:
(1) a fixture-prep commit containing only the `tests/runner.rs` additions —
the two matching-refs mock blocks (one for repo-a/#10 around line 239, one
for repo-b/#50 around line 318) that return an empty `[]` so the new sweep
proceeds — and (2) a pure-source commit containing the `src/main.rs`,
`src/runner.rs`, `src/status.rs`, and `src/tracker.rs` changes that wire the
sweep into `run_once` and add `BlockReason` / `DeleteStaleBranchError`. The
fixture commit is inert against the unchanged `eb299ae` impl (the sweep call
site does not yet exist there) so it would land cleanly between `eb299ae`
and a slimmed-down `ba99f13'`. Mechanically this would be a non-interactive
reconstruction: tag the current tip, `git reset --hard eb299ae`,
`git checkout ba99f13 -- tests/runner.rs` then commit the fixture, then
`git checkout ba99f13 -- src/...` and commit with the original ba99f13
message body, then `git cherry-pick b28cc84 949ffd7` to restore the README
ADR-0003 commits.

**Why I am not addressing it in this run.** The branch is already pushed to
`origin/agent/76-pre-claim-deletion-of-stale-agent-n-branches-on-or`
(`git status` reports "up to date with origin"), so any reshape of
`ba99f13` produces a divergent history that requires a force-push to land.
Bellows's post-agent step runs `git add -A && git commit && git push` — it
is not documented as force-pushing, and silently rewriting four-deep
history under the push step is the kind of change I should not make without
a human signing off. The underlying code is correct and all tests are
green; this finding is purely about commit shape (the test-first
red-to-green boundary was crossed by bundling matching-refs mock additions
to existing slice-b tests into the impl commit rather than pre-adding them
in `eb299ae` where they would have been inert). The operator should decide
whether the cleaner history is worth the force-push, and if so can perform
the split locally before merging the PR.
