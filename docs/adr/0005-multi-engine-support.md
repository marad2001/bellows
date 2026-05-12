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

## Engine selection model

Engine selection is **per-phase**, not per-run. Each agent-invoking
phase declares its own ordered `cli_chain` in `orchestrator.toml`:

```toml
[phases.implement]
cli_chain = ["claude", "codex"]

[phases.review]
cli_chain = ["codex", "claude"]

[phases.review_fix]
cli_chain = ["claude", "codex"]

[phases.security_review]
cli_chain = ["codex", "claude"]

[phases.security_fix]
cli_chain = ["claude", "codex"]
```

The chain is consulted **at the start of each phase**, not once at
claim time. A run that picks `claude` for implement and then hits
rate-limit before review starts will independently re-walk the
review-phase chain from scratch — there is no run-wide engine
binding to carry forward.

**Soft-diversity picker.** Inside a phase, bellows walks the chain in
a two-pass selection:

1. **First pass** — pick the first chain entry that is both (a) hot
   (i.e. its persisted `cooling_until` is in the past or unset) and
   (b) ≠ the implementer-CLI for this run. This is the diversity-
   preferring pass: the reviewer is not the implementer when both
   are hot.
2. **Second pass** — if the first pass produces nothing (the only
   hot chain entries match the implementer-CLI), pick the first
   chain entry that is just (a) hot, and emit an operator-visible
   warning to the run-log comment that diversity has collapsed for
   this phase. The phase still runs; the win is degraded but the
   throughput contract holds.
3. **Empty chain** — if neither the first pass nor the second pass
   finds a hot entry (every chain member is cooling), terminate the
   run as `RateLimited`. The
   persisted state file (see below) and the next claim's chain-walk
   together make this self-correcting rather than fatal: when any
   engine's `cooling_until` elapses, the next reclaim picks it up.

The diversity-pass-first-then-throughput-pass order encodes "prefer
diversity, but degrade visibly rather than block." Swapping the
passes would silently sacrifice diversity even when an alternative
hot engine was available.

**Per-issue forced-engine label.** Operators can override the chain
on a single issue with one of two labels — `engine:claude` or
`engine:codex` — that act as a **forced-single-engine override** for
that run:

- No chain walk, no fallback, no diversity logic — the named engine
  is the one used in every phase.
- Rate-limit on the named engine terminates the run as `RateLimited`
  (there is no alternative to fall through to, by design).
- This is the escape hatch for "I want claude on this issue
  specifically" — common shapes include reproducing a claude-only
  bug, A/B comparing two engines on the same brief, or pinning a
  workboard issue to a known-stable engine while bellows is still
  hardening codex.

**Both engine labels present → refuse-to-claim.** When both
`engine:claude` and `engine:codex` appear on the same issue, bellows
refuses to claim it (parallel to the existing `MissingAgentBrief`
shape: the pre-claim check surfaces a clear "ambiguous engine
selection" verdict rather than silently picking one). The
forced-single-engine override is meaningful only when it names one
engine; two labels is operator error and bellows says so.

## Auth model

Codex authenticates against the ChatGPT subscription via an
interactive OAuth flow on the host, parallel to today's claude
`bellows setup-auth` interactive volume-seeding flow. The persisted
login state lives in `$CODEX_HOME` on disk (default `~/.codex/`),
and the spike on issue #79 confirmed empirically that mounting the
codex auth directory as a Docker named volume preserves credentials
across container restarts identically to today's
`bellows-claude-credentials` volume — same pattern, different
volume.

Bellows therefore keeps **per-engine credentials volumes**, declared
under a per-engine subtable in `orchestrator.toml`:

```toml
[auth.claude]
credentials_volume = "bellows-claude-credentials"

[auth.codex]
credentials_volume = "bellows-codex-credentials"
```

The keys are `auth.claude.credentials_volume` and
`auth.codex.credentials_volume`. The previous flat key
`auth.credentials_volume` continues to work — bellows rewrites it
to `auth.claude.credentials_volume` at config-load time for
backwards compatibility with the v1 single-engine
orchestrator.toml shape. An operator who never touches codex sees
no change.

**Lazy validation.** Only the engine about to be dispatched to has
its credentials volume validated. An operator who has configured
`cli_chain = ["claude", "codex"]` for `phases.implement` but has
not yet completed `bellows setup-auth --engine codex` is not
blocked from claiming an issue — bellows uses claude (the hot
chain entry) and only fails over to codex if claude rate-limits.
At that fallback point, if `auth.codex.credentials_volume` is
missing or empty, the run terminates with an auth-error that names
codex specifically (see operator UX, below). The alternative —
eager validation at startup — would force the operator to seed
both engines before they could use either, which defeats the
"start with one engine, add the other later" rollout the design
supports.

## Policy image

A **single** policy image carries both CLIs **baked** and **pinned**
to specific versions — `claude-code` at the version bellows v1
already pins, and `codex` at `rust-v0.130.0` (the version covered
by the #79 spike findings). The image stays a single docker tag so
operators do not have to coordinate two image lifecycles, and
pinning both CLIs together keeps a known-good combination — bumping
either CLI is a deliberate image rebuild, not a runtime surprise.

Per-phase engine choice is passed into the container via the
`BELLOWS_ENGINE` env var, set at each container start by bellows.
Each phase's `cli_chain` walk produces an engine name (`claude` or
`codex`); bellows passes that name through to docker as
`-e BELLOWS_ENGINE=<name>` on the spawned container. The variable
is set per-phase, not once at run start: the implement phase may
launch with `BELLOWS_ENGINE=claude` and the review phase with
`BELLOWS_ENGINE=codex` in the same pipeline. Inside the container
the agent never sees the chain — only the single resolved engine
choice for this phase.

The image's entrypoint scripts — today's `run-agent` (the
implement-phase wrapper) plus the analogous wrappers for the
review and security-review phases — each branch on
`BELLOWS_ENGINE` to invoke the right CLI with the right
flags. The codex branch passes
`codex exec --dangerously-bypass-approvals-and-sandbox
--skip-git-repo-check` with explicit stdin closure (the #79 spike
confirmed `</dev/null` is load-bearing — without it codex hangs on
EOF). The claude branch keeps today's `claude -p
--dangerously-skip-permissions` invocation unchanged. A missing or
unrecognised `BELLOWS_ENGINE` is a hard error from the entrypoint
— the bellows-side dispatcher always sets it, so an unset value
indicates a regression rather than a degraded mode worth running.

## Considered alternatives

(Placeholder — populated by subsequent acceptance criteria.)

## Consequences

(Placeholder — populated by subsequent acceptance criteria.)
