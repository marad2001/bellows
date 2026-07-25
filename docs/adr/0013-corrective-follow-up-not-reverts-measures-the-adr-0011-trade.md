# Corrective follow-up, not reverts, measures the ADR-0011 trade

ADR-0011 removed every subjective merge gate on the stated basis that
"the operator prefers a cheap morning revert of the rare bad merge over
a gate on every run." That makes the rate of bad merges the number which
tells you whether the trade is paying, and the ADR's wording points at
reverts as the way to count it.

Reverts do not measure it, because they do not happen. At the time of
writing, **51 agent runs have merged to `master` and not one has been
reverted.** The single revert anywhere in history is buried inside PR
#45's development (`Revert "Bellows agent run" — drop spurious synthetic
agent-notes entry`) and squash-on-merge erased it from `master`.

The mechanism is not the one in use. The observed response to bad merged
work is to **file a follow-up issue and fix it forward** — #154 and #161
are both exactly that. That is cheaper than reverting, it keeps the
parts of the change that were fine, and it is the natural move when
there is an AFK agent available to point at the follow-up overnight.

## Decision

The signal is **corrective follow-up**, recorded as a **Correction** —
an issue filed to fix something a prior run already shipped.

A raw count of Corrections would be misleading, so each carries exactly
one **Attribution**:

- **`agent-fault`** — the brief was clear and the harness behaved; the
  shipped work still did not do the job.
- **`harness-fault`** — a Bellows prompt, gate, or policy misbehaved,
  and would misbehave identically under any **Engine**.
- **`brief-fault`** — the **Engine** faithfully built what the brief
  asked for, and the asking was wrong.

**Only `agent-fault` is evidence about the ADR-0011 trade.**
`harness-fault` measures Bellows' own defect rate; `brief-fault`
measures triage quality. Summing the three does not produce a quality
number, and treating the total as one would misread our own bugs as
engine unreliability at exactly the moment someone is deciding whether
to reinstate the gates.

This is not hypothetical. Both Corrections that existed when this was
written — #154 (the mega-commit check fires on essentially every run,
because `commit_all` makes the red→green commit shape it demands
architecturally impossible) and #161 (a policy-layer prompt nudge that
proved insufficient in the field) — are `harness-fault`. A naive count
would have read 100% of observed Corrections as agent failure.

Attribution is assigned by the operator when the Correction is filed,
because working out the answer is a by-product of writing the issue.

## Consequences

- Labels only. No tooling: at this volume a `bellows postmortem`
  subcommand would spend most of its life printing zeroes, and
  `gh issue list --label agent-fault` answers the question today.
  Revisit when eyeballing the query stops working.
- `agent-fault` and `brief-fault` carry `Corrects: #<PR>` — the PR, not
  the issue, because one issue can produce several runs and the PR is
  what identifies a run. `harness-fault` carries an optional
  `Observed in: #<PR>` instead: the fault is in Bellows, so there is
  often no run honestly at fault and the pointer is evidence rather
  than attribution.
- The `agent-fault` set is the answer key #163 needs for eval fixtures.
  Each entry names a PR, and a PR rehydrates into a fixture (diff,
  brief, verdict) from GitHub.
- **Do not build a revert counter.** ADR-0011's wording invites one; it
  would report zero indefinitely.
- A convention with no tooling behind it can quietly lapse, leaving a
  half-populated dataset that looks complete. Accepted deliberately —
  the marginal cost is one label on an issue already being filed, and
  building tooling for data that does not yet exist is the worse
  failure.

## Considered alternatives

- **Count `git revert` commits on `master`.** Rejected on the evidence
  above: zero in 51 merges, and squash-on-merge erases in-branch
  reverts anyway.
- **Heuristic detection — "did these files change again within N
  days?"** Rejected. On a young repo under active development that is
  most of the codebase, so it would drown in false positives, and a
  noisy quality metric is worse than none because it invites action on
  noise.
- **Reopened issues as the signal.** Rejected as too narrow. Neither
  #154 nor #161 reopened anything; both are new issues about work that
  shipped and closed correctly.
- **A `bellows postmortem` subcommand doing scanning and clustering.**
  Deferred, not rejected. It is the right shape at a volume we are
  nowhere near.
- **A single `correction` label with no attribution.** Rejected — it is
  the failure mode the two real examples demonstrate, since it would
  have reported our own prompt bugs as agent unreliability.

## Relationship to ADR-0011

This does not revisit ADR-0011's decision; it supplies the measurement
that decision implied and names the signal correctly. ADR-0012
generalised ADR-0011's *rule*; this records how to tell whether the
resulting posture is working.
