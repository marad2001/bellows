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

## Operating context

The kickoff prompt the agent reads on phase start is rendered by
`policy::render_kickoff`. Today it is engine-agnostic — the same
prompt body is fed into `claude -p`. With two engines, the
renderer becomes **engine-aware**: it dispatches on the engine name
the dispatcher passed in, and produces the shape that engine
expects.

The two engines differ in how they discover operating context.
Claude reads `CLAUDE.md` files from the current and parent
directories at session start; the bellows policy image already
seeds the global `CLAUDE.md` at `/home/bellows/.claude/CLAUDE.md`
plus a per-repo `CLAUDE.md` at the cloned workspace root, and the
agent picks them up automatically. Claude also auto-loads skill
bodies on demand from its skills directory.

Codex's analogue (`AGENTS.md` plus on-demand skill discovery) is
shaped differently enough that maintaining two parallel context
trees — one under `CLAUDE.md` for claude, one under `AGENTS.md`
for codex — would create a permanent lockstep-maintenance tax: any
operating-context edit would have to be applied twice or drift
class would re-emerge (exactly the failure mode ADR-0004 documents
for the bellows-vs-CI gate spec). To avoid that, the codex path
in `policy::render_kickoff` **inlines** the operating-context body
plus the bodies of all baked skills directly into the kickoff
prompt text itself. There is one source of truth (the existing
operating-context + skills directory baked into the policy image
at build time); the codex kickoff is a function of that one source,
not a parallel maintained file. Skill bodies are inlined verbatim
into the kickoff so codex sees the same operating instructions
claude would discover via on-demand file reads.

The trade-off is prompt length: codex's kickoff is materially
longer than claude's. The empirical findings from #79 confirm
codex's headless invocation accepts long prompts on the command
line (no per-prompt token cap below what we'd actually inline), and
the cost of a longer kickoff is paid once per phase — well below
the cost of drifting operating-context maintained in two places.

## Persisted rate-limit state

Engine selection at phase-start consults a persisted state file
named `bellows-state.json`, written alongside `bellows.log` in the
operator's bellows working directory (same parent path as the
existing log; same lifecycle owner). The file records per-engine
`cooling_until` timestamps and nothing else — the structure is
small enough that the entire file is rewritten on each update, no
schema migration story needed yet.

```json
{
  "engines": {
    "claude":  { "cooling_until": "2026-05-12T17:42:00Z" },
    "codex":   { "cooling_until": null }
  }
}
```

**Write path.** When a phase exits non-zero and the captured stderr
matches a known rate-limit signature for the engine that ran, the
runner derives a `cooling_until` timestamp from the signature and
writes it to the state file before terminating or advancing the
chain. The cooldown is parsed from the CLI's stderr signature
itself when the engine surfaces one — claude's existing rate-limit
messages already include a parseable retry-after duration; codex's
default-text stderr does not include a parseable reset-at
timestamp (per the #79 spike findings — reset timestamps live in
HTTP response headers, not in stderr), so the codex match
substrings (`quota exceeded`, `rate limit:`) trigger a conservative
5-minute default cooldown. A future enhancement can capture
codex's `--json` event stream for accurate `RateLimitSnapshot`
reset-at timestamps if the 5-minute default proves too coarse in
practice.

**Read path.** Every phase-start reads `bellows-state.json` before
the chain walk. An engine whose `cooling_until` is in the future is
treated as cold (skipped in pass 1 and pass 2 of the soft-diversity
picker); an engine whose `cooling_until` is in the past or `null`
is hot. The read is a single point-in-time snapshot at the start
of the phase — bellows does not re-read mid-phase.

**Self-correcting.** If a cooldown is wrong (the CLI under-promised
the reset window, or the persisted state is stale for any reason),
the next phase that picks that engine off the chain hits a fresh
rate-limit. The fresh stderr signature updates the state file with
the new cooldown, and the chain advances. The mistake costs one
phase invocation; nothing about the design assumes the CLI's reset
timestamp is correct.

**Single pass per phase prevents thrashing.** A phase that picks an
engine, hits rate-limit, updates state, and re-enters the chain
walk could in principle pick the next engine, hit rate-limit
again, and loop. Bellows enforces a single pass per phase as an
invariant: within one phase invocation, the chain walk runs once
and produces one engine choice. Subsequent rate-limit signatures
within that phase invocation terminate the run (or, for the
implement phase, trigger at most one in-place chain advancement —
see below). The state file is the persisted memory that lets the
*next* claim consult cooldowns without thrashing this one.

## Rate-limit behaviour: implement phase vs the rest

The throughput win operates across all agent-invoking phases, but
the mid-execution rate-limit response splits in two:

**Implement phase, workspace at base SHA** — when the implement
phase rate-limits before the agent has made any commits beyond the
base SHA (the cloned tip of `master` at claim time), bellows
performs an **in-place chain advancement**: drop workspace, swap
to the next hot chain entry, and re-run the implement phase from
base. The work that was lost is zero (no commits), so an
in-place retry is strictly cheaper than terminating and reclaiming.
Bellows caps this at **max 1 in-place advancement per phase
invocation** — if the second engine also rate-limits before
committing anything, the run terminates as `RateLimited` rather
than walking deeper into the chain in-place. The cap exists to
preserve the single-pass-per-phase invariant from the persisted-
state section above: bellows tolerates exactly one in-place retry
for the "wasted nothing yet" case, then defers to the next-claim
chain walk for further engine rotation.

The in-place advancement is gated on the workspace being at base
SHA specifically because once the agent commits, the cost of
dropping the workspace is non-zero — that work has to be redone
from scratch, possibly burning the wall-clock budget on the
re-run. The clean restart from base is only free in the very
narrow window before the agent has produced output.

**All other agent-invoking phases** (review, review-fix,
security-review, security-fix) — mid-execution rate-limit
terminates the run as `RateLimited`. The state file is updated
with the engine's `cooling_until`. The current run does not
attempt an in-place engine swap for these phases: review-fix and
security-fix in particular operate on a workspace that already
carries committed implement-phase work, so a workspace drop would
defeat the point. Terminate-and-defer is the safer shape: the
operator's `agent-rate-limited` label appears on the issue.
Next claim consults `bellows-state.json`, walks the chain
**afresh** for each phase under the freshly-read cooldowns, and
resumes from the implement-phase commit history that's already on
the branch.

The split is asymmetric on purpose: implement-phase in-place
advancement is a cheap throughput win; review-phase in-place
advancement is a workspace-throwing-out tax that the
terminate-and-defer path avoids.

## Considered alternatives

(Placeholder — populated by subsequent acceptance criteria.)

## Consequences

(Placeholder — populated by subsequent acceptance criteria.)
