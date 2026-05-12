# Multi-engine support: per-phase CLI chain for diversity and throughput

Bellows v1 ships with a single engine (`claude`) per pipeline. Two
operational pressures push that towards being a runtime choice rather
than a compile-time one. **Diversity**: when the same engine writes
and then reviews a change, the reviewer is grading its own homework
— a class of mistakes (idiom blind spots, over-confident patterns,
prompt-shaped reasoning) survives both phases unchallenged. Letting
the implement-phase engine and the review/security-review-phase
engine differ shrinks that class. **Throughput**: when a subscription
rate-limits mid-pipeline, parking the whole run on a single-engine
posture wastes both the work the agent has done and the remaining
wall-clock budget — bellows should reach for the other CLI rather
than wait. The throughput win operates across every agent-invoking
phase (implement, review, review-fix, security-review, security-fix)
not just implement: a rate-limit during review is just as expensive
as one during implement.

Both wins live behind a single mechanism — a per-phase ordered
`cli_chain` of engines that bellows walks at each phase's start —
but the design choices they pull on differ. The diversity win is
soft: bellows prefers the chain entries that differ from the
implementer-CLI, and degrades visibly (a warning in the run-log
comment) when rate-limit state forces the soft preference to
collapse. The throughput win is the chain-walk itself plus the
persisted per-engine `cooling_until` state file that lets a later
phase skip an engine that just rate-limited without a fresh probe.
The diversity-collapse warning is load-bearing for diversity (silent
collapse defeats the win); the chain-walk + state file is
load-bearing for throughput (a one-shot probe per phase, without
state, would thrash on every retry).

## Considered alternatives

(Placeholder — populated by subsequent acceptance criteria.)

## Consequences

(Placeholder — populated by subsequent acceptance criteria.)
