# Merger phase decides routing; pre-claim PR-open gate dropped

Bellows currently routes a run's exit state via `policy::classify_exit`,
a mechanical reader of the final `agent-notes.md` that branches on
heading shape: any `## Unaddressed finding:` → `AgentSelfReportedFailure`
(draft + `agent-failed`); freeform prose only → `SuccessWithNotes`
(non-draft + `agent-noted`); empty → silent auto-merge. That classifier
solves the (β) synth-provenance and (γ) coverage-backstop problems well
— bellows-authored synth headings and parser-as-backstop synthesised
headings both route deterministically to draft — but it solves the (α)
agent-authored routing decision poorly. The classifier has no view of
the diff, the brief's acceptance criteria, or whether the unaddressed
finding's substance is still load-bearing after the full seven-phase
pipeline has run. Concretely, when phase 4 (review-fix) writes
`## Unaddressed finding: AC1 implementation commit is a mega-commit`
because the only other option is destructive force-push on already-pushed
history, the classifier routes to draft even though the substantive
code state has every AC met and CI green. Combined with the pre-claim
PR-open gate in `runner::run_once` (slice-b / #42), one such hold
halts the entire repo's polling loop until an operator intervenes,
which defeats the AFK contract overnight.

The fix is a new **phase 8 merger** that reads the end-of-pipeline state
holistically and emits a structured verdict. The verdict replaces the
classifier's (α) decision for agent-authored heading cases. The (β)
synth-provenance and (γ) coverage-backstop paths remain hard overrides
the merger cannot vote past. The pre-claim PR-open gate is dropped in
the same slice and replaced with a container-presence check, so a single
`HOLD-DRAFT` no longer halts the queue.

## Phase shape

Phase 8 runs after the end-of-pipeline cargo-checks gate (phase 7).
It is **hard-required**: there is no per-repo opt-out flag. The engine
is configurable via `[phases.merge] cli_chain` in `orchestrator.toml`,
matching the existing per-phase pattern; the default is
`claude:claude-opus-4-7`. The cross-family choice is deliberate —
the agent that most commonly originates the agent-authored
`## Unaddressed finding:` heading is phase 4 (codex), and the merger
re-evaluating those holds from a different model family is a stronger
independence check than re-using codex for the merger.

The merger reads:

- the full diff vs the target branch,
- the brief's acceptance-criteria list, taken verbatim from the brief
  rather than from the reviewer phase's paraphrase,
- the final `agent-notes.md` with synth-provenance markers attached
  (so the merger can distinguish "the agent authored this heading"
  from "bellows synthesised this heading via weak-test guard /
  parser-as-backstop / implement-crash recovery"),
- CI status and the phase-7 cargo-checks result.

It does **not** read the full phase logs from earlier phases, nor the
review-findings / security-findings files themselves. Phase logs are
mostly redundant with the diff (any fix that landed is in the diff;
any agent-reasoning that didn't land doesn't matter at merge time).
Re-including the findings files invites the merger to second-guess
the upstream reviewer's severity calibration, which is not the
merger's job; its job is "is this ready to ship," not "was every
finding really a problem."

The merger's prompt explicitly anchors judgement on the diff and the
AC list. The notes are presented as the agent's stated reasoning, not
as evidence the code is correct. If the agent's stated reason for an
unaddressed finding does not match what the diff shows, the merger
votes `HOLD-DRAFT`. This anchor is the v1 defence against agents
gaming the merger by writing self-serving notes; the structural
defence is that the merger reads the diff at all.

## Verdict format

The merger emits a natural-language prose review of the PR, ending
with one final line of the form `VERDICT: <token>` where `<token>`
is one of `MERGE`, `HOLD-NOTED`, `HOLD-DRAFT`. The verdict parser
greps the last lines for that pattern; if missing, ambiguous, or
off-vocabulary, the parser returns `None` and the run falls back to
the existing classifier (see "Failure mode" below).

The three verdict tokens map to the three terminal exit states that
already exist:

- `MERGE` → non-draft, no special label, auto-merge workflow fires
  on green CI.
- `HOLD-NOTED` → non-draft + `agent-noted` label, auto-merge workflow
  skips, operator merges manually after reading the merger's
  rationale. This preserves the ADR-0006 informational-channel niche
  for judgement calls the operator should see before merge.
- `HOLD-DRAFT` → draft + `agent-failed` label, operator reviews
  substance.

Bellows posts the merger's full prose as a "Merge verdict" comment
on the PR. The surfacing behaviour is configurable in
`orchestrator.toml`: default `post-always` for a complete audit
trail; operators who find the noise unhelpful on clean MERGE runs
can switch to `post-on-hold-only`. Audit-trail-by-default is the
recommendation because the merger's most valuable output for an
operator is *why* it voted the way it did, and that record needs to
survive the squash-merge.

## Interaction with the existing classifier

`policy::classify_exit` still runs. Its job changes from "decide the
exit state" to "decide the exit state when the merger is silent."
Specifically:

1. **(β) Synth-provenance is a hard override.** If the agent-notes
   classification includes a recorded Bellows synth span whose cause
   emits `## Unaddressed finding:` (weak-test guard, parser-as-
   backstop, or implement-crash recovery), the run routes to
   `AgentSelfReportedFailure` regardless of the merger's verdict.
   The merger sees the synth-provenance markers in its input and
   is told it cannot vote `MERGE` when any are present.
2. **(γ) Coverage backstop is a hard override.** If the
   parser-as-backstop synthesises `## Unaddressed finding:` because
   phase 4 or 6 silently skipped a blocker / important finding,
   that hold survives the merger.
3. **(α) Agent-authored routing is delegated.** When the only
   unaddressed-finding headings are agent-authored, the merger's
   verdict overrides the classifier's three-state output.

The merger is therefore additive on the throughput axis (rescuing
over-holds where the agent-authored heading is not load-bearing)
and silent on the safety axis (the mechanical signals that
ADR-0006 carved out remain mechanical signals).

## Failure mode

If the merger phase fails to produce a usable verdict — sandbox
crash, wall-clock timeout, garbage output that the parser cannot
match, or subscription rate-limit — the run falls back to the
existing classifier's verdict for that pipeline. The fall-back
path is the conservative, current-system behaviour: an
agent-authored `## Unaddressed finding:` heading routes to
`AgentSelfReportedFailure` exactly as today.

This makes the merger phase **strictly additive on the throughput
axis**: a working merger raises throughput; a failing merger is
throughput-neutral to today's status quo. There is no regression
mode. The cost of this choice is that a calibration drift the
merger could have rescued goes unrescued during merger outages,
but that is the same cost the system pays today and is acceptable.

Rate-limit handling uses the existing non-implement-phase
disposition (`handle_non_implement_rate_limit`), which skips the
phase rather than burning the whole run on a retry.

## Pre-claim PR-open gate dropped

`runner::run_once`'s pre-claim PR check (`src/runner.rs:226`,
slice-b / #42) is removed in the same slice. The check was
enforcing two invariants through one mechanism:

- **Container concurrency = 1.** The real subscription-terms
  constraint: bellows runs at most one agent container at any
  moment. This invariant is preserved as a direct check on
  container presence (the actual constraint), not by proxy through
  open `agent/*` PRs.
- **Sequential merge of agent PRs into master.** The justification
  in the original comment ("prior agent's PR may still gate
  master"). This concern is mostly redundant with the
  auto-merge workflow's SHA-pin (ADR-0001), the cargo-checks gate
  on every commit, branch protection's required `ci` check, and
  the merge-API's 409-on-head-moved behaviour. A subsequent agent
  run starting from stale master is the same trade-off humans
  on a multi-PR team face every day; conflicts surface at merge
  time and are resolved there, not avoided by serialising claims.

Dropping the gate caps the blast radius of any single `HOLD-DRAFT`
verdict: instead of halting the repo's queue until an operator
intervenes, the queue keeps flowing, drafts accumulate as a
side-channel for morning triage, and the AFK contract survives a
miscalibrated overnight verdict.

## Considered alternatives

- **Demote test-shape findings from `important` to `nit` in the
  review rubric (Option D from the design discussion).** Rejected as
  the primary fix because it solves the routing problem upstream
  rather than holistically. The merger's job is the end-of-pipeline
  view; demoting the rubric merely changes which findings *enter*
  the review-fix loop with which severity. The merger handles
  test-shape findings correctly from outside the rubric: when a
  test-shape unaddressed-finding heading is the only thing holding
  a PR with ACs met and CI green, the merger sees the diff and
  votes `MERGE`. A future ADR can demote the rubric if drift
  measurements show the merger is being called on too often to
  rescue test-shape holds; for v1 the rubric stays as-is.
- **Maximalist merger context (read all phase logs).** Rejected as
  prompt bloat. The diff is the ground truth at merge time. Any
  fix that landed is in the diff; any reasoning that didn't land
  doesn't change the merge decision. Including phase logs adds
  tokens without proportional accuracy gain.
- **Verdict-only output (no rationale).** Rejected. The audit trail
  is the merger's most valuable artefact for the operator. A bare
  verdict line gives no recourse when a verdict later turns out
  wrong.
- **Asymmetric trust: merger can only vote toward `MERGE`, never
  toward hold.** Rejected. The merger has the holistic view; if we
  trust it to merge, we should also trust it to flag. Asymmetric
  trust adds branching complexity for no real safety win, and it
  also forfeits the merger's ability to catch cases where the
  classifier would have voted `MERGE` but the diff is in fact poor.
- **Two-state verdict vocabulary (`MERGE` / `HOLD` only).**
  Rejected. Collapsing `HOLD-NOTED` into `HOLD-DRAFT` discards
  ADR-0006's earned middle-ground for judgement-call PRs (the
  Cognito Terraform slice, the issue #27 absence-style AC). The
  cost of three states is one extra token in the verdict line and
  one extra branch in the routing code.
- **Structured JSON / YAML verdict.** Rejected. Adds parser
  fragility (invalid JSON → fall back → same throughput tax as a
  crashed merger) for no win over a trailing verdict line.
- **Configurable / opt-out merger phase.** Rejected. A pipeline
  whose merge decision can be silently disabled per repo is harder
  to reason about and harder to keep calibrated. The phase is hard-
  required; the engine within the phase is configurable.
- **Codex (`codex:gpt-5.5`) as the default merger engine.**
  Rejected as the default, retained as a configurable choice. Opus
  is the better fit for the first-look-judgement role this phase
  occupies; codex is already used for the iterate-and-fix phases
  the merger needs cross-family independence from.
- **Keep the pre-claim PR-open gate, rely on merger calibration to
  drive the held-PR rate near zero.** Rejected. The merger will
  occasionally be wrong; designing for that means making the
  failure cheap, which means caps on blast radius, which means
  dropping the gate. The gate's original justification is mostly
  redundant with existing infrastructure (see "Pre-claim PR-open
  gate dropped" above).
- **Narrow the gate to draft PRs only.** Rejected as a half-step.
  Non-draft `agent-noted` PRs also wait for human merge; gating
  on them or not gating on them changes nothing about throughput.
- **Adversarial verdict-audit pipeline shipped in the same slice.**
  Deferred. The audit is premature before any merger verdicts
  exist. Ship the merger, run a week of overnight AFK, then assess
  whether observed drift warrants an audit subagent.

## Consequences

- A new phase 8 is added to the implement → cargo-checks → review →
  review-fix → security-review → security-fix → cargo-checks
  pipeline. Phase numbering, log lines, and wall-clock budget
  accounting all shift to 8/8.
- `src/policy.rs` gains a `render_merger_prompt` (mirroring the
  existing `render_*_prompt` shape), a `MergerVerdict` enum
  (`Merge` / `HoldNoted` / `HoldDraft`), and a `parse_merger_verdict`
  helper that greps the final lines of the agent's output.
- `src/policy.rs::classify_exit` accepts an `Option<MergerVerdict>`
  alongside its existing inputs. When `Some` and the run does not
  trigger a (β) / (γ) hard override, the merger verdict drives the
  routing. When `None`, the existing heading-classifier path runs
  unchanged.
- `src/config.rs` gains a `[phases.merge]` schema entry and a
  `[phases.merge].posting` toggle (default `post-always`,
  alternative `post-on-hold-only`).
- `src/runner.rs::run_once`'s pre-claim PR-open check is replaced
  with a container-presence check. The `OpenAgentPrs` block reason
  variant in `BlockReason` is retired or repurposed (depending on
  whether the container-presence check needs its own reason
  variant — likely a new `AgentContainerRunning` variant).
- The ADR-0006 target-repo `auto-merge.yml` filter on the
  `agent-noted` label remains required. `HOLD-NOTED` runs continue
  to need that filter so the workflow does not auto-merge them.
- The merger phase adds one Opus invocation per issue at default
  configuration. The phase is read-only on the workspace, has a
  bounded input (single diff + brief + notes + CI status), and
  contributes only a short wall-clock fraction relative to phase
  1.
- Overnight AFK throughput improves on two axes: the merger reduces
  the rate of false holds, and dropping the pre-claim gate caps
  the blast radius of any residual hold. A worst-night outcome is
  N-1 merged PRs and one draft to triage, not one draft and a
  dead queue.
- A subsequent agent run may start from a `master` that does not
  yet contain a still-open prior `agent/*` PR's changes. Conflicts,
  if any, surface as auto-merge SHA-pin 409s and sit the second
  PR open for manual handling. This is the same trade-off humans
  on multi-PR teams already make.
- ADR-0006's two-channel design (informational vs escalation
  agent-notes) is preserved in the underlying classifier; the
  merger sits above it and overrides the (α) routing decision
  only.
