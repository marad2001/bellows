# agent-notes.md distinguishes informational notes from structured failure

Bellows keeps `agent-notes.md` as the agent-to-operator handoff surface, but
the file is no longer a binary failure signal. The classifier splits its
content into two channels by heading shape.

**Informational notes** are freeform agent-authored prose: TDD exceptions,
brief trade-offs, scope judgments, or other useful context that should stop
silent auto-merge but should not accuse the agent of failure. A run that is
otherwise successful and leaves only informational notes classifies as the new
`ExitReason::SuccessWithNotes`. The PR opens non-draft with the `agent-noted`
label, and a human merges it manually after reading the note. This preserves
epistemic honesty for cases like PR #63's Cognito Terraform slice and the
issue #27 precedent, where the agent needed to explain that an absence-style
acceptance criterion could not be driven through the normal red/green test
loop.

**Escalation notes** are structured failure. The escalation marker is the
existing `## Unaddressed finding:` heading used by the slice-9.6 review-fix
loop, the slice-8 weak-test guard, and the parser-as-backstop. If the
agent-authored note contains that heading, or Bellows deliberately synthesises
that heading for an address-or-explain violation, the run continues to route as
`AgentSelfReportedFailure`: draft PR plus `agent-failed`.

Before applying that heading rule, the classifier separates Bellows-authored
synth material from agent-authored prose using structured pipeline state
recorded at the synth write site. The synth helper that appends markdown to
`agent-notes.md` must also return or persist an out-of-band provenance record
with the synth cause and the exact byte boundaries of the appended entry.
Those records travel through the runner state that later captures
`agent-notes.md`; they are not reconstructed by parsing the workspace file.
HTML comments are human-readable provenance only: the existing
`<!-- bellows ... -->` comments help an operator understand the posted note,
but they are not trusted for routing and cannot make later text
Bellows-authored on their own.

If the classifier still strips Bellows synth text from the agent-authored
prose stream before checking heading shape, it only strips spans whose
Bellows provenance is known outside the file text. The block boundaries are
the exact appended-entry byte ranges from the structured provenance records,
not a search for comment markers or headings. A copied `<!-- bellows ... -->`
comment in an agent note remains ordinary agent-authored text unless it
matches a recorded Bellows synth span. A run whose only note is the issue #49
implement-crash synth has no agent-authored escalation heading and routes on
its actual crash signal, subsuming the bespoke "has notes but ignore them"
shim. A run whose only note is a weak-test or parser-as-backstop synth still
routes to failure because the structured synth cause deliberately emits
`## Unaddressed finding:` escalation.

This ADR does not change the issue #85 lifecycle: `agent-notes.md` remains
ephemeral. Bellows captures it for classification and for the PR comment, then
committed-deletes it before pushing the final branch tip so the file cannot
leak into `master`.

The `agent-noted` path depends on target-repo merge policy. Per ADR-0001, the
target repository's `auto-merge.yml` workflow is the CI gate and merge policy,
not a Bellows-native merge switch. Therefore each target repo must update its
workflow to exclude or otherwise hold PRs labelled `agent-noted`. Bellows can
extend the ADR-0004 snapshot pattern by reading the target workflow at
`workspace::prepare` time; if the snapshot does not prove the label filter is
present, Bellows opens `SuccessWithNotes` PRs as draft rather than risk
immediate auto-merge.

## Considered alternatives

- **Accept silent auto-merge of noted runs.** Rejected because an informational
  note exists precisely when a human should read a judgment call before merge;
  letting the current ADR-0001 workflow merge it silently would recreate the
  same operator surprise in a quieter form.
- **Pure prompt relaxation.** Rejected because telling agents "only write
  notes for real failures" does not give the classifier enough information to
  distinguish a useful TDD exception from an escalation. The routing decision
  must be mechanical after the run completes.
- **Closed informational heading vocabulary.** Rejected because it makes benign
  notes syntax-heavy and brittle. The only heading that carries routing meaning
  should be the failure marker, `## Unaddressed finding:`; all other freeform
  content should stay informational.
- **Implement-phase parser-as-backstop.** Rejected because implement-phase
  notes are not a review-finding coverage contract. The parser-as-backstop is
  useful for checking whether known blocker/important findings were addressed
  or explicitly escalated; using it as the general implement-phase router would
  conflate note parsing, coverage enforcement, and exit classification.
- **Bellows-driven cross-repo workflow updater.** Rejected because target repos
  own their merge policy under ADR-0001. Bellows should detect a missing
  `agent-noted` filter and fail closed with a draft fallback, not mutate every
  target repository's workflow on the operator's behalf.
- **HTML comment marker as provenance.** Rejected because `agent-notes.md` is
  workspace content an agent can write or append to. A copied or forged
  `<!-- bellows ... -->` comment must not affect routing-sensitive
  classification; Bellows-authored synth provenance has to come from the
  Bellows write path and pipeline state instead.

## Consequences

- `ExitReason` gains `SuccessWithNotes`, representing an otherwise-successful
  run with informational `agent-notes.md` content.
- `classify_exit` stops accepting a bare `has_agent_notes: bool` and instead
  consumes a structured note classification that can distinguish no notes,
  informational notes, structured escalation, and Bellows-authored synth
  provenance known outside the file text.
- Bellows synth write sites return or store structured provenance alongside
  the markdown they append to `agent-notes.md`. Any text stripping uses those
  provenance records' exact byte boundaries, while HTML comments remain
  human-readable provenance only.
- `render_kickoff` teaches agents the two channels: freeform notes for
  informational context, and `## Unaddressed finding:` only for true
  escalation.
- Each target repo pays a rollout tax in `.github/workflows/auto-merge.yml`:
  the workflow must hold or exclude PRs labelled `agent-noted` so
  `SuccessWithNotes` PRs do not auto-merge.
- Bellows extends the ADR-0004 workflow-snapshot pattern to this merge-policy
  check. If the target's auto-merge workflow snapshot lacks the `agent-noted`
  filter, `SuccessWithNotes` falls back to draft so a human must merge
  manually.
- The issue #49 `implement_crash_synthesised` special case is subsumed by
  out-of-band synth provenance before agent-authored heading classification.
- The issue #85 ephemeral-file contract stays unchanged: `agent-notes.md` is
  captured, posted as a PR comment when present, removed from the workspace,
  and committed-deleted before the final push.
