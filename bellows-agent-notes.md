# Agent notes (informational)

Two things worth a reviewer's attention. Neither is an unsatisfied
acceptance criterion.

## Glossary gap for `/grill-with-docs`

`CONTEXT.md` has no term for "returning a claimed issue to the pickup
queue". The code, tests and log line all use **release** (`run-abort
release:`, `release_claim_after_run_error`), chosen to sit alongside the
existing `claim` / `finalise` vocabulary. Per `docs/agents/domain.md` I
have not added the entry myself — noting it here as a real gap rather than
invented language, since the concept now exists in two places (this path
and the startup reconcile sweep) and wants one agreed name.
