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

## Operator UX

Two operator-facing surfaces gain engine-awareness:

**`bellows setup-auth` and `bellows refresh-auth` accept
`--engine <name>`.** Both subcommands take an optional
`--engine claude` / `--engine codex` flag that selects which
engine's credentials volume the interactive flow targets. When
the flag is omitted the **default** is the **first entry of
`phases.implement.cli_chain`** — the engine the operator's
implement phase is most likely to dispatch to, and therefore the
one most likely to need credentials seeded first. Operators with
only one engine configured never need to pass the flag; operators
running both engines pass `--engine codex` once after the v1 →
multi-engine upgrade to seed the new volume, and thereafter only
when re-seeding the non-default engine.

**Auth-error callout in the run-log comment names the engine.**
When a phase exits matching an auth-error stderr signature, the
runner produces a run-log comment with a callout that names
**which** engine returned the error — for example, "Codex's API
returned 401 Unauthorized — refresh your subscription auth with
`bellows refresh-auth --engine codex`." The codex auth-error
substring match — the composite of `401 Unauthorized` AND
"Missing bearer or basic authentication" — comes from the #79
spike findings, so this callout fires reliably. Naming the engine
is load-bearing under the multi-engine design: a generic "auth
error" callout in a two-engine config leaves the operator guessing
which volume to re-seed.

## Empirical findings from spike #79

The decisions above lean on findings captured by the spike on
issue #79 against codex-cli 0.130.0 (full transcript on the
comment thread of #80). The load-bearing facts:

- **Pinned version.** Codex is pinned at **`rust-v0.130.0`** in the
  policy image (direct Linux binary
  `codex-x86_64-unknown-linux-musl.tar.gz` from the release tag,
  not the npm wrapper).
- **Headless invocation.** `codex exec
  --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check
  "$PROMPT" </dev/null` is the equivalent of `claude -p
  --dangerously-skip-permissions`. The `</dev/null` stdin closure
  is **load-bearing**: without it codex hangs forever waiting for
  stdin EOF.
- **Auth state path.** Codex auth state lives in `$CODEX_HOME`
  (default `~/.codex/`). Mounting it as a Docker named volume
  preserves credentials across container starts identically to
  today's `bellows-claude-credentials` volume — confirmed
  empirically by running codex inside a fresh `$CODEX_HOME` seeded
  from a host copy of `auth.json` + `cap_sid` + `config.toml`.
- **Rate-limit stderr signatures** for `is_rate_limit_signature`:
  `quota exceeded` (subscription users, primary path) and
  `rate limit:` (Platform-API users, secondary path). Source-code
  derived from `codex-rs/codex-api/src/error.rs` rather than from
  a deliberate quota burn — substring matches still hold.
- **Auth-error stderr signature** for `is_auth_error_signature`:
  composite match of `401 Unauthorized` AND the verbatim string
  "Missing bearer or basic authentication" (a bare `401
  Unauthorized` could be a false positive from an unrelated HTTP
  401 in the agent's web-fetched content).
- **No parseable reset-at in stderr.** Codex's structured reset
  timestamps come from HTTP response headers
  (`x-codex-primary-reset-at`, `x-codex-secondary-reset-at`), not
  from default-text stderr. The state-file write path therefore
  falls back to a conservative **5-minute** default cooldown when
  a codex rate-limit substring matches — the self-correcting
  behaviour in the persisted-state section covers the case where
  the actual reset window is longer.
- **Line-oriented plain-text stderr** with no ANSI control
  sequences — grep-friendly substring matching is sufficient (no
  separate parser needed).

These findings are summarised here rather than linked from the
spike comment because they are load-bearing for slices #81/#82 and
should be visible in the ADR body without requiring a round-trip
to GitHub.

## Considered alternatives

- **Single engine, drain-on-rate-limit** (today's posture). Rejected:
  rate-limited runs park the issue and waste the operator's
  remaining wall-clock budget; the throughput win is unreachable
  by definition. Slow drift back towards the brief's listed
  problem.
- **One engine for the whole run, picked at claim time.** Rejected:
  collapses the diversity win (same engine implements AND reviews)
  and limits the throughput win to "fail the whole run early"
  rather than "swap the next phase." Per-phase selection is the
  only shape that wins on both axes.
- **Hard-diversity preference (refuse-to-run if implementer-CLI is
  the only hot engine).** Rejected: a degraded review is strictly
  better than no review at all, especially when the implement
  phase already produced a useful diff. The visible-collapse
  warning in the run-log preserves the operator's ability to
  reproduce the win on a re-run while still extracting value
  from the current run.
- **In-place chain advancement for every phase.** Rejected: review-
  fix and security-fix operate on a workspace that carries
  implement-phase commits; dropping that workspace mid-phase
  destroys the work and forces a clean implement-phase re-run.
  Limiting in-place advancement to implement-at-base-SHA preserves
  the "no work lost" property the in-place shape exists for.
- **Walk the chain mid-phase as many times as needed.** Rejected:
  the single-pass-per-phase invariant exists to prevent thrash. A
  multi-pass walker that re-enters chain selection after every
  rate-limit signature would loop forever on a config where every
  engine is cooling but bellows didn't observe it yet.
- **Two policy images, one per engine.** Rejected: doubles the
  image-build lifecycle, doubles the operator's pull cost, and
  introduces a class of "wrong image for this phase" dispatch
  bugs. The single image with both CLIs baked is one moving part
  fewer.
- **Parallel `AGENTS.md` next to `CLAUDE.md` in every repo.**
  Rejected: creates a permanent lockstep-maintenance tax for
  operating-context edits (the exact failure mode ADR-0004
  documents for the bellows-vs-CI spec). Inlining at kickoff time
  is one source of truth.
- **Per-engine rate-limit state in volatile memory.** Rejected:
  loses the cooldown across runs, so each fresh claim re-probes
  every engine and burns one phase invocation on the cool-down
  rediscovery. The state file's only job is to skip that
  rediscovery.
- **Eager validation of every engine's credentials volume at
  bellows startup.** Rejected: forces operators to seed both
  engines before they can use either, which defeats the
  "incremental rollout" path. Lazy validation at dispatch time
  preserves the v1 → multi-engine upgrade story.
- **Flag the engine via a per-issue config block in
  `orchestrator.toml`.** Rejected: per-issue config is not
  per-issue *labels*; it's a different artifact with a separate
  lifecycle. The `engine:claude` / `engine:codex` label sits next
  to the other per-issue labels already in the triage state
  machine and reuses the existing pre-claim refusal shape
  (parallel to `MissingAgentBrief`).

## Consequences

- **Diversity by default.** Two-engine operators see implementer ≠
  reviewer on every run where both engines are hot. The win
  degrades visibly (a run-log warning) when forced; silent
  collapse is a regression.
- **Throughput across every phase.** A rate-limit during review or
  security-review is no longer a run-killer when the alternative
  engine is hot. The state file makes the cooldown durable across
  claims — the next reclaim picks up where this run left off
  rather than re-probing.
- **Operators with one engine see no change.** A
  `cli_chain = ["claude"]` on every phase, `auth.credentials_volume`
  (rewritten to `auth.claude.credentials_volume` at config-load),
  an unset `auth.codex.credentials_volume`, and an empty codex
  volume produces the v1 single-engine behaviour. The
  backwards-compatibility rewrite + lazy-validation rule together
  make multi-engine opt-in.
- **The `agent/*` namespace gains an engine dimension that's
  invisible at the branch level.** Slices #81/#82 can persist the
  engine choice for each phase in the run-log comment for
  post-hoc auditing, but the branch name itself does not encode
  which engine produced the diff. Operators who need that signal
  can read the run-log; the per-issue `engine:<name>` label
  remains the explicit channel.
- **The `bellows-state.json` file becomes operator-visible state.**
  An operator inspecting it sees per-engine cooldowns. Manual
  edits work — clearing the file or zeroing a `cooling_until` is
  the supported way to force a re-probe of an engine bellows
  believes is cooling. The file is JSON not TOML on purpose: it's
  written by bellows, read by bellows, only occasionally
  inspected by humans, and the JSON shape composes more cleanly
  with future structured fields.
- **The `engine:claude` / `engine:codex` labels join the runtime-
  label vocabulary.** They are pre-claim-only (consulted at
  claim time), unlike the runtime labels bellows applies during a
  run. The README's label-vocabulary table gains two entries.
- **A future third engine fits without a schema change.** The
  `cli_chain` is an ordered array; `auth.<engine>.credentials_volume`
  is a nested table keyed by engine name; the state file's
  `engines` map is keyed by engine name. Adding a third entry is
  data, not code shape.
- **The single-pass-per-phase invariant gives an upper bound on
  cost per phase invocation.** At most one engine probe (plus, for
  the implement phase only, at most one in-place chain
  advancement) per phase invocation. Operators can reason about
  worst-case wall-clock without consulting the chain depth.
