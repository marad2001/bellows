# Agent notes (informational)

Two things worth a reviewer's attention. Neither is an unsatisfied
acceptance criterion.

## AC3's test is a regression guard, not a strict red-then-green cycle

"A run that errors after its PR exists is not released" is an
absence-of-resource criterion: the natural test asserts that *nothing
happened*, which can pass against unchanged source by accident. It is also
not reachable end-to-end in an integration test — getting `run_once` past
`open_pr` needs Docker, a real remote and a full agent pipeline.

So `run_error_after_the_pr_exists_releases_nothing` tests at the seam
instead: `run_once` empties the claim slot the instant `open_pr` returns,
so "the PR exists" *is* "the slot is empty", and the test reproduces that
transition through the same public API `run_once` uses, then asserts the
release issues no HTTP request whatsoever. The other three tests were
written red-first in the normal way.

What this does not cover: if someone deleted the `slot.clear()` call that
follows `open_pr`, this test would still pass. The three pre-PR tests
would stay green too, since they never reach a PR.

## Glossary gap for `/grill-with-docs`

`CONTEXT.md` has no term for "returning a claimed issue to the pickup
queue". The code, tests and log line all use **release** (`run-abort
release:`, `release_claim_after_run_error`), chosen to sit alongside the
existing `claim` / `finalise` vocabulary. Per `docs/agents/domain.md` I
have not added the entry myself — noting it here as a real gap rather than
invented language, since the concept now exists in two places (this path
and the startup reconcile sweep) and wants one agreed name.
