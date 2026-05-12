# Multi-engine support: per-phase CLI chain for diversity and throughput

Bellows v1 ships with a single engine (`claude`) per pipeline. Two
operational pressures push that towards being a runtime choice rather
than a compile-time one. The design captured here makes engine
selection a per-phase decision against an operator-configured ordered
chain, with persisted rate-limit state, soft-diversity preference,
and a per-issue forced-engine label override.

## Considered alternatives

(Placeholder — populated by subsequent acceptance criteria.)

## Consequences

(Placeholder — populated by subsequent acceptance criteria.)
