# Gates are judged by whether they summon an absent human, not by whether they are mechanical

ADR-0011 restricted merge gating to "mechanical, objective" failures. Two
later proposals — an **Oscillation** detector that abandons a wedged
implement attempt (#164), and a model-scored quality bar on the agent
brief at claim time (#166) — forced the question of what that ADR was
actually asserting, because both are *heuristic* and a literal reading of
"mechanical only" rejects both.

A literal reading is wrong. ADR-0011's load-bearing sentence is:

> "a merge gate that demands a human is the thing being away makes
> impossible"

That is not an epistemological claim about signal reliability. It is a
claim about **who a gate summons**. Mechanical-vs-subjective correlated
with that in the cases ADR-0011 had in front of it — the heuristic gates
were the ones parking PRs until morning — but the correlation is
incidental, not the principle.

## Decision

A control is legitimate if it does **not** create a state that only an
absent human can clear. Whether it is mechanical or heuristic is
irrelevant on its own.

Applied to the three places a control can sit:

- **Inside the run pipeline** (claim → implement → … → merge): a control
  may not park work awaiting a human. This is ADR-0011's rule, now stated
  in its general form and extended backwards from merge to claim.
- **A control that changes *how* a run proceeds** without parking it is
  not a gate at all and is unconstrained by this rule. An **Oscillation**
  triggering an **Advance** reaches the same terminal states the run could
  already reach; it reallocates budget, it does not summon anyone.
- **Inside triage**: unconstrained. Triage's *purpose* is deciding whether
  a human is needed, and `needs-info` is that decision expressed as
  output. A quality bar there is the phase working, not a violation of it.

The corollary that decides most cases: **quality judgements about intent
belong in triage, where a human is expected; the run pipeline may only
act on what it can resolve alone.**

## Consequences

- **#164 is legitimate.** Oscillation-triggered **Advance** ships. It is
  heuristic and it discards work, but it summons nobody — a wedged run's
  terminal state is a draft PR either way, and the advance merely reaches
  it via a second engine instead of via the wall-clock.
- **#166 is not, and is closed `wontfix`** with the precedent recorded in
  `.out-of-scope/brief-quality-claim-gate.md`. A refuse-to-claim on brief
  quality is *worse* than the merge gates ADR-0011 removed: a parked PR at
  least holds finished work, whereas a refused claim holds nothing.
- **The brief-quality bar already exists in the right place.** The triage
  agent's decision tree routes vague issues to `needs-info` before a brief
  is ever written. The residue worth closing is that the triage agent does
  not verify the brief *it just authored*; that is a `TRIAGE_PROMPT`
  change, not a new control.
- **Hand-written briefs remain unchecked, deliberately.** An operator
  applying `ready-for-agent` by hand has made an explicit judgement.
  Gating it would both second-guess a deliberate human decision and
  summon that human back to argue with it.
- The existing pre-claim refusals (`AgentContainerRunning`,
  `StaleAgentBranchDeletionFailed`, `MissingAgentBrief`,
  `AmbiguousEngineLabels`, `blocked-by`, ADR-0010's non-draft-PR gate)
  are unaffected. All of them either self-clear on a later tick or
  reflect a human decision already made, so none summons anyone.
- **Not to be confused with the transient-outage fallback (#170).**
  `runner::run_analysis_agent_with_fallback` re-picks the next hot chain
  entry when a read-only analysis phase hits a 503/500/504, bounded by
  chain length. That is a resilience retry, not an **Advance**: the phase
  had produced nothing yet, so nothing is discarded and the max-1
  `advances_used` cap does not apply. Oscillation must not be wired into
  it — the two mechanisms cover different phases and answer different
  questions.

## Considered alternatives

- **Read ADR-0011 literally ("mechanical only") and reject both
  proposals.** Rejected. It rejects #164 on a technicality while giving no
  account of *why* the existing pre-claim gates are acceptable, and it
  would forbid any future heuristic that improves a run without blocking
  it.
- **Have Oscillation abort the run to a draft PR rather than advance.**
  Rejected, and this was the decisive trade. Under abort-to-draft a false
  positive destroys a run that would have succeeded, so the threshold must
  be set so conservatively that the detector never fires and never earns
  its keep. Under advance, a false positive costs the elapsed minutes and
  still yields a real attempt from another engine — which is what makes an
  imperfect detector worth building at all.
- **Treat Idleness (a workspace unchanged for a long stretch) as
  actionable too.** Rejected. A still workspace is indistinguishable from
  an engine mid-thought or one about to exit cleanly, so acting on it
  risks discarding a run that was about to succeed — invisibly, since only
  the replacement's result is ever seen. Idleness is recorded, never
  acted on. Only **Oscillation** is unambiguous.
- **Give Oscillation its own advance allowance, separate from the
  rate-limit one.** Rejected. `CONTEXT.md` defines both triggers as
  meaning the same thing — no further progress is available from this
  engine — so they draw on one allowance. Separate counters would assert
  they are different concepts. Practically, each advance discards an
  attempt, and the draft PR of a failed run then contains only the *last*
  and least-progressed one; capping at one bounds the worst case at a
  single traded attempt.

## Relationship to ADR-0011

This generalises ADR-0011; it does not overturn it. Every routing decision
ADR-0011 made stands. What changes is the stated reason: the rule is about
what a control does to an absent operator, and "mechanical" was a proxy
that held for the cases then in view.
