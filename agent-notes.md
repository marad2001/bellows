# Agent notes

Informational channel (no escalation). This documents where the strict
test-first commit shape (failing-test commit → make-it-pass commit) was
adapted, and why the adaptation still honours the brief's intent.

## TDD-shape deviations (all sanctioned by the brief's own exceptions)

The brief calls for a failing-test commit before each make-it-pass commit
per acceptance criterion, and explicitly allows two exceptions to be recorded
here rather than escalated: **absence-of-resource ACs** and
**pure-prompt-text ACs**. Every deviation below falls into one of those two
buckets.

1. **Removing the source subsystem (commit 7a3d04c) is a "make it compile"
   step, not a behaviour addition.** The preceding commit 9b69c37 removed the
   tests that exercised `SuccessWithNotes` / the `agent-noted` routing lane
   and the `auto_merge_workflow_supports_agent_noted_filter` snapshot. Deleting
   a variant and its routing arm cannot be driven by a *new* failing test —
   the "test" is the compiler: with the variant gone the old tests would not
   compile, which is why they had to be removed first. This is the
   absence-of-resource shape (AC1: "grep returns no production references"):
   the observable contract is that the symbols are *gone*, verified by the
   negative-assertion tests below plus the AC1 grep itself.

2. **Config-field removal (`RuntimeLabelsConfig.agent_noted`) was driven by
   removing its assertion, not by a new RED test.** `RuntimeLabelsConfig` has
   no `deny_unknown_fields`, so an operator's stale `agent_noted = "..."` line
   is silently ignored — there is no new behaviour to assert, only the absence
   of the field. The defaults test (`tests/config.rs`) dropped its
   `agent_noted` assertion in the test commit; the field removal in the source
   commit makes the suite compile and pass. Absence-of-resource shape.

3. **Kickoff prose (commit 52ea3d6) is a pure-prompt-text AC.** The
   informational-channel paragraph in `base_kickoff_body()` still named the
   removed `SuccessWithNotes` classification and `agent-noted` PR label, which
   is factually wrong post-ADR-0011. This was driven test-first via the
   existing kickoff test (commit 572de21), which now asserts the rendered
   prompt does NOT contain `agent-noted` and DOES describe the informational
   note as *advisory*. Pure-prompt-text shape.

## Negative-assertion tests (deliberate surviving `agent-noted` references)

Three tests intentionally keep the removed strings as *absence* assertions —
these are the "test fixtures ... may legitimately remain" case in AC1:

- `tests/auto_merge.rs::auto_merge_workflow_has_no_agent_noted_filter_after_adr_0011`
  — asserts the workflow body no longer contains `agent-noted`.
- `tests/readme.rs` — asserts the rendered README no longer contains
  `agent-noted`.
- `tests/policy.rs::rendered_kickoff_teaches_informational_vs_escalation_agent_notes_channels`
  — asserts the kickoff prompt no longer contains `agent-noted` and does say
  `advisory`.

## ADR prose left untouched (out of scope)

ADRs 0006 / 0009 / 0010 / 0011 still mention `SuccessWithNotes` / `agent-noted`
in their historical decision text. The brief scoped ADR prose updates OUT
except for *dead cross-references*. These are prose mentions in immutable
decision records, not broken links to removed code symbols, so they were left
as-is. ADR-0011 in particular legitimately documents the removal.


<!-- bellows weak-test guard appended this entry because the implement phase produced changes against the base branch with no new Rust test attributes (#[test], #[tokio::test], etc.) and the issue did not carry the configurable skip-label. The presence of this entry forces the run to agent-self-reported-failure (draft PR + agent-failed label) so a human reviewer sees the gap. -->

## Unaddressed finding: no new tests added

Bellows-synthesised entry. The implement phase produced a diff against the base branch with no new Rust test attributes detected by the slice-8 weak-test guard. A green cargo-checks gate over an unchanged test suite is a poor signal of correctness; the brief's acceptance criteria typically require accompanying tests. The weak-test guard synthesised this entry so the run routes to agent-self-reported-failure for a human reviewer.
