# Agent notes

Informational only — no escalation. All acceptance criteria are met:
`cargo clippy --all-targets --all-features -- -D warnings` is clean and
`cargo test` is green (836 tests, 0 failed).

## TDD commit-shape deviations

The brief mandates a failing-test commit before each make-it-pass commit.
That shape was followed for every AC that could be driven test-first. Three
sub-tasks are inherently *absence-of-resource* or *pure-prompt-text* work,
which the brief explicitly permits recording here rather than forcing an
artificial failing test:

- **AC1a/AC1b — remove `ExitReason::SuccessWithNotes` + runner routing.**
  Driven test-first: `tests/runner.rs` was rewritten to the new 2-arg
  routing signature first (RED = compile error), committed
  (`3bb1cfe`), then the variant + routing were removed (`58364ff`).

- **AC1c — remove the `auto_merge_workflow_supports_agent_noted_filter`
  snapshot + plumbing (`b8ebb95`).** Absence-of-resource: the change is
  the *deletion* of a struct field, accessor, snapshot init, and the
  `detect_auto_merge_filter_support` helper, together with the four
  ADR-0006 workspace tests that exercised them. There is no observable
  behaviour left to assert once the field is gone (nothing reads it after
  the SuccessWithNotes arm is removed), so a standalone failing test would
  only have asserted "the field does not exist" — a compile-time fact, not
  a behaviour. The removal is covered by the whole suite continuing to
  compile and pass.

- **AC1d — remove `agent_noted` from `RuntimeLabelsConfig` (`14dcbd9`).**
  Absence-of-resource, same rationale: deleting a config field, its
  `Default`, and its `default_agent_noted()` helper. The `tests/config.rs`
  assertion that pinned the old default was removed in the same commit;
  `RuntimeLabelsConfig` has no `deny_unknown_fields`, so a stray
  `agent_noted = "..."` in an operator TOML is now silently ignored rather
  than rejected — this matches ADR-0011's "the label lane is removed"
  intent (no behaviour depends on the key any more).

- **AC1f — kickoff prompt text (`0867890`).** Pure-prompt-text, but it
  *was* driven test-first: `tests/policy.rs`'s kickoff assertion was
  inverted to require the prompt NOT name `agent-noted` /
  `SuccessWithNotes` first (RED, `97d5abc`), then the prompt string was
  rewritten to describe the advisory note lane (GREEN).

## Scope decisions

- **Historical ADRs 0006 / 0009 / 0010 left intact.** They record design
  decisions as made at the time and still contain `SuccessWithNotes` /
  `agent-noted` in their bodies. The brief scopes out "updating ADR prose
  beyond removing dead cross-references." These are the substance of those
  ADRs, not dead cross-references, so rewriting them would rewrite history.
  ADR-0011 is the superseding record and correctly documents the removal.

- **`tracker::add_issue_labels` kept.** Its only production caller was the
  removed SuccessWithNotes routing arm, but it is a `pub` generic helper on
  the library crate (so it triggers no `dead_code` lint) and retains direct
  test coverage in `tests/tracker.rs`. The test's fixture label was
  re-pointed off the removed `agent-noted` onto `agent-done`.

- **AC1 grep target.** `grep -rn "SuccessWithNotes\|agent-noted\|agent_noted"`
  returns no *production* references. Remaining hits are: (a) test
  assertions that pin the *absence* of the strings after ADR-0011
  (`tests/auto_merge.rs`, `tests/policy.rs`, `tests/readme.rs`), and (b)
  the historical ADR bodies described above.
