# Pre-claim PR-open gate reinstated for non-draft `agent/*` PRs (amendment to ADR-0009)

ADR-0009 dropped the pre-claim PR-open gate from `runner::run_once`,
replacing it with a Docker container-presence probe. The rationale at
the time was that the container probe enforces the actual
concurrency=1 invariant directly, that conflicts from a stale-base
next claim are "the same trade-off humans on a multi-PR team face
every day," and that dropping the gate capped the blast radius of a
single `HOLD-DRAFT` verdict so a miscalibrated overnight verdict no
longer halted the queue.

Operationally that second clause turns out to be wrong for the
AFK-overnight pattern bellows targets. A multi-PR human team
resolves conflicts within minutes of the conflict appearing, by the
person who wrote the code while it is still in their head. Bellows
running AFK does the opposite: it cuts five branches off
ever-staler `main` overnight, lands five non-conflicting individual
runs as far as each agent could see in isolation, and the operator
arrives in the morning to a stack of PRs that need to be rebased
serially in PR-author-of-record order — including the ones whose
substance the operator never planned to keep. The rebase tax
overwhelmed the throughput gain the gate-drop was supposed to
deliver; an operator measured "forever" on a recent run that
produced four agent/* PRs against the same target repo overnight.

The fix is to reinstate the pre-claim PR-open gate, but **only for
non-draft `agent/*` PRs**, and **per-repo** rather than globally.
The container probe from ADR-0009 stays — that gate enforces the
local-Docker invariant which the PR-open gate cannot substitute
for. The two gates compose: container probe stops a second container
from starting on the same host; PR gate stops a new branch being cut
from a stale base on the same target repo.

## Gate shape

The gate sits in `runner::run_once`'s per-repo candidate-collection
loop, before the `tracker::find_next_issue` call for each repo:

1. List open PRs in `owner/repo` via
   `tracker::list_open_non_draft_agent_pr_numbers`.
2. Filter to head branches starting `agent/` AND `draft = false`.
3. If non-empty, log
   `bellows: skipping repo <slug> this tick: waiting on open
   non-draft agent/* PR(s): #N[, #M…]` and `continue` to the next
   configured repo — do NOT add the repo to `candidates` or to
   `cleared_repos`.
4. If empty (or the API call failed; soft-fail logged), proceed
   into the existing `find_next_issue` path.

The skip is per-repo so an open PR on repo A does not stall claims
on repo B. The container probe remains the global gate.

## Why drafts are excluded

`HOLD-DRAFT` / `agent-failed` verdicts open draft PRs whose next
step is human triage — read the `<details>` log, decide whether to
re-label `ready-for-agent`, fix the brief, or close. The triage
window is unbounded in wall-clock terms; an operator may not look
at a failure PR until the next morning's standup, or later. If the
gate held on drafts, a single overnight `HOLD-DRAFT` would stall
the queue until manual intervention — exactly the regression
ADR-0009 was solving.

Non-draft PRs (`Success` → green-CI auto-merge, or
`SuccessWithNotes` → human-merge after reading the informational
note) merge within a bounded window: minutes for clean Success,
hours-to-overnight for `agent-noted`. The gate trades
no-rebase-tax against that bounded delay. The operator-visible
contract is "non-draft PR sits open → next claim waits"; HOLD-DRAFT
backlog does not stall the queue.

## Wake-up latency and throughput ceiling

`interval_seconds` (default 45, often 30 in operator configs) caps
the wake-up latency after a PR merges. With a clean-Success run
that takes ~20 minutes end-to-end (implement + gates + review +
merger + CI + auto-merge), each issue effectively reserves ~25–35
minutes of wall-clock. Over an 8-hour AFK window the ceiling is
~14–19 issues; for the workloads this v1 of bellows targets,
that is well above the actual rate of well-specified
`ready-for-agent` issues an operator produces.

If the ceiling ever bites:

- Lowering `interval_seconds` shaves wake-up latency but cannot
  go below the auto-merge.yml runtime itself.
- A webhook receiver (PR-merged event) would replace polling with
  near-zero wake-up latency. The ngrok / static-IP / signing cost
  is not currently justified at one-operator scale; documented
  here as the v2 lever.
- A per-issue opt-out label (e.g. `independent`) would let
  declared non-conflicting issues parallelise past the gate. Not
  built; would be a clean follow-up if the ceiling matters.

## What ADR-0009 still owns

ADR-0009's two other contributions stand unchanged:

- **The merger phase (phase 8)** remains the end-of-pipeline
  verdict authority. Reinstating the PR-open gate does not change
  what the merger reads, what verdict tokens it emits, or how the
  classifier consumes them.
- **The container-presence probe** remains the global concurrency=1
  gate. It runs BEFORE the per-repo loop, so a running agent
  container short-circuits the tick before the new PR-open gate is
  consulted for any repo.

The new gate is additive on the existing pipeline; no phase logic
or verdict logic changes.

## Considered alternatives

- **Status quo (ADR-0009 unchanged)**. Rejected on the lived
  evidence: rebase tax under AFK dominates and recurs every time
  multiple agents run overnight against the same target repo.
- **Global gate (any open `agent/*` PR anywhere blocks all
  claims)**. Rejected as too restrictive for multi-repo setups: an
  open PR on repo A blocking work on independent repo B has no
  basis in the conflict story. Per-repo is the natural scope.
- **Block on drafts too (literal pre-ADR-0009 behaviour)**.
  Rejected for the same reason ADR-0009 dropped the gate in the
  first place: one stuck draft halts the queue indefinitely. The
  non-draft-only refinement preserves ADR-0009's overnight-halt
  fix while solving the rebase-tax problem.
- **Block only on clean Success (exclude `agent-noted`)**.
  Rejected because `agent-noted` PRs ALSO merge eventually and
  carry the same stale-base risk for whatever bellows would have
  claimed next. The non-draft scope (Success + Noted) is the
  right grain.
- **Per-issue `independent` opt-out for parallelisable work**.
  Deferred to v1.5. Operationally clean but adds a label and a
  config field the current scale does not need.
- **Webhook receiver for PR-merged events instead of polling**.
  Deferred. The wake-up latency ceiling on polling is acceptable
  at one-operator scale; the receiver's setup cost
  (ngrok / signing / static IP) is not justified yet.

## Consequences

- The polling loop's per-repo throughput on any repo with an open
  non-draft `agent/*` PR is paused until that PR merges, is
  closed, or is converted to draft. Wake-up latency on the
  unblock is bounded by `interval_seconds`.
- HOLD-DRAFT / `agent-failed` backlog does NOT pause the queue;
  the AFK contract survives an overnight failure exactly as
  ADR-0009 intended.
- Multi-repo throughput is independent: an open PR on
  `workboard-financial-advice` does not pause claims on
  `workboard-aviation`.
- The container probe stays as the global concurrency gate; the
  new PR gate composes with it rather than replacing it.
- Operators get a per-tick log line naming the skipped repo and
  the blocking PR number(s), so the polling loop's quiet ticks
  are still legible.
- The list-open-PRs API call is one paginated GET per configured
  repo per polling tick. At the default 45s interval and typical
  per-repo PR counts (≤2 pages), this is well within GitHub's
  5000/hour rate limit.
- The ADR-0009 rationale clause *"conflicts surface at merge time
  and are resolved there, the same trade-off humans on a
  multi-PR team face every day"* is explicitly retracted by this
  ADR for the AFK-orchestrator context. The clause holds for
  synchronous human teams; it does not hold for an orchestrator
  whose operator is asleep.
