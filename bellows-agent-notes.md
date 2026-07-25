## Unaddressed finding: implementation and tests are bundled in a single mega-commit

Addressing this finding would require rewriting the branch history so that the new
behavior tests (`tests/engine_dispatch.rs`, `tests/policy.rs`) land in a failing-test
commit *before* a separate make-it-pass commit touching `src/policy.rs` and
`src/workspace.rs`. That is not possible from inside this run because the Bellows
pipeline owns commit creation, not the agent: the flagged commit
`37d5be8e6bb1438e124a721cdf8ca70b7980c153` is authored by `Bellows <bellows@local>`
with the message "Bellows agent run" — it is the orchestrator's own squashed commit,
produced by Bellows' single `git add -A && git commit` step after the implementing
agent exited. An agent in this review-fix invocation cannot split that already-created
commit into a test-first / make-it-pass pair: any `git rebase` or history rewrite I
performed would be rewriting published branch history I do not own, and Bellows re-runs
`git add -A && git commit` after I exit, so it would simply re-squash the working tree
into another single commit regardless. The finding also has no code-level root cause to
fix — it concerns commit *shape*, not any behavior, logic gap, or invariant in the
source, so there is nothing in `src/policy.rs`, `src/workspace.rs`, or the tests to
change that would satisfy the test-first commit-shape contract. This is exactly the
case the finding's own suggestion anticipates ("record this exact finding as
unaddressed … if the pipeline's commit ownership makes that rewrite impossible"). A
human maintainer who wants test-first history here must either adjust the Bellows
commit-ownership model to emit two commits per run (a failing-test commit and a
subsequent implementation commit) or exempt orchestrator-squashed commits from the
test-first commit-shape heuristic, since that heuristic assumes an agent that authors
its own commit boundaries.
