# Merge gating is mechanical-only; the merger phase is demoted to advisory

Bellows gated auto-merge on a stack of *subjective and heuristic*
signals: the phase-8 merger's `HOLD-NOTED` / `HOLD-DRAFT` verdicts
(ADR-0009), unaddressed review / security findings, the weak-test
guard and parser-as-backstop synth `## Unaddressed finding:` headings,
and informational agent notes routed to `agent-noted` for manual merge
(ADR-0006). Each of these could send an otherwise-shippable run to a
draft or a human-merge lane.

For the operator whose human-in-the-loop contribution has moved
entirely to up-front architecture and design guidance — who no longer
edits implementation code in review, only shapes intent and structure
at the outset — that stack meant nearly every overnight run still
waited for a human at merge time. The weak-test guard in particular
fired on exactly the architecture-led work the operator now does
(restructuring without adding a test in the shape the scanner wants),
and it sat *above* the merger as a hard override the merger could not
vote past. The net effect defeated the AFK contract: the point of
away-from-keyboard agents is that the operator is away, and a merge
gate that demands a human is the thing being away makes impossible.

## Decision

Only **mechanical, objective** failures gate a merge:

- end-of-pipeline CI red (the target repo's `auto-merge.yml` is the
  CI gate per ADR-0001),
- either bellows cargo-checks gate red (`post_implement_gate` or
  `end_pipeline_gate`),
- an agent crash (non-zero exit) in any pipeline phase whose work
  affects the shipped code — implement, review, review-fix,
  security-review, or security-fix. A crashed reviewer (for example a
  mis-typed codex model pin) therefore drafts the PR rather than
  auto-merging with the review silently skipped. The phase-8 merger is
  excluded: it is advisory, so its own failure never gates.
- wall-clock budget exceeded,
- subscription rate-limit / auth error.

Every *subjective or heuristic* outcome auto-merges on green CI.
Leftover review / security findings, weak-test-guard trips, and agent
notes are surfaced as **advisory PR comments**, never as a draft or a
hold. The posture is deliberate: the operator prefers a cheap morning
revert of the rare bad merge over a gate on every run.

The phase-8 merger (ADR-0009) is **retained but demoted to advisory**.
It still runs read-only over the diff, ACs, notes, and CI status, and
still posts its full prose as a `## Merge verdict` PR comment — the
operator's holistic morning read of *what shipped and whether it looked
right*. But its verdict token no longer routes the run. This keeps a
single end-of-pipeline holistic judgement in the loop (the reason the
merger was invented) while removing its power to gate — which matters
because the alternative independent-judgement source considered here, a
second reviewer, was not adopted.

## What this supersedes

- **ADR-0006 (gating half).** The `SuccessWithNotes` / `agent-noted`
  human-merge lane is removed; informational notes no longer gate.
  The agent-notes-as-advisory-PR-comment affordance ADR-0006
  introduced is *kept* — notes still surface to the operator — but
  they route to `Success`, not to a manual-merge label. ADR-0006's
  ephemeral-file lifecycle (issue #85) is unchanged.
- **ADR-0009 (routing half).** The merger phase survives; its *routing
  authority* does not. `classify_exit` stops consuming the verdict;
  the (β) synth-provenance and (γ) coverage-backstop hard overrides
  are removed. ADR-0009's container-presence concurrency gate and
  ADR-0010's pre-claim non-draft-PR gate are **unaffected** — those
  are throughput / concurrency controls, not human-review gates, and
  the brief-window serialisation they impose still applies (more runs
  are now non-draft, but they auto-merge within minutes on green CI).

## Considered alternatives

- **Drop the merger phase entirely (gate = CI only, no holistic
  read).** Rejected. With a second reviewer also declined, nothing
  else provides an end-of-pipeline "did this meet the ACs and is it
  sound as a whole" judgement — CI proves only that it compiles and
  tests pass. The advisory merger fills that hole for one read-only
  Opus invocation per issue.
- **A-with-one-valve: an unaddressed `blocker`-severity finding the
  fix agent explicitly could not resolve still drafts.** Rejected for
  simplicity. It reintroduces a subjective branch (severity is a
  judgement call) for a case rare enough that a morning revert is the
  cheaper contract.
- **Add a second reviewer to strengthen the quality bar before
  removing gates.** Deferred. Single review + security review +
  advisory merger is judged sufficient at the operator's current
  issue volume; a second reviewer adds subscription / rate-limit
  pressure for a marginal catch rate. A clean follow-up if drift
  measurements later justify it.
- **Keep the merger as a hard gate but relax its calibration.**
  Rejected. The weak-test-guard and parser-as-backstop hard overrides
  sit *above* the merger, so tuning the merger cannot remove them; and
  any subjective gate at all recreates the bottleneck this ADR exists
  to remove.
- **Keep the weak-test guard / parser-as-backstop as gates.**
  Rejected. These are the exact heuristics that fired on the
  operator's architecture-led work. As advisory comments they retain
  their signal without blocking a merge.

## Consequences

- `policy::classify_exit` drops its `notes: NotesShape` and
  `merger_verdict` parameters; after the mechanical checks it returns
  `ExitReason::Success`. `NotesShape` no longer drives routing.
- `ExitReason::SuccessWithNotes` and the `agent-noted` runtime label
  lane become unreachable and are removed. The target-repo
  `auto-merge.yml` `agent-noted` filter is removed with them.
- The merger phase, its `[phases.merge]` config and `posting` toggle,
  its prompt, its verdict parser, and its `## Merge verdict` comment
  posting are all **retained** — only the verdict's routing effect is
  removed.
- The weak-test guard, parser-as-backstop, and implement-crash synth
  still write `agent-notes.md` for the advisory PR comment, but no
  longer affect routing. An implement crash still drafts, via the
  mechanical non-zero-exit check, not via the notes.
- Drafts occur only on mechanical failure. The blast radius of a
  miscalibrated overnight run is a single morning revert, not a
  blocked queue or a stack of held PRs.
