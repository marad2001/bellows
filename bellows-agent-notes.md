# Agent notes — #171 (brief self-check in the triage skill)

All four acceptance criteria are satisfied and the full CI gate is green. Two things a reviewer may want to know.

## Test-first sequencing

The change is pure skill-document text, so "red" here means an assertion over `SKILL.md` content rather than over behaviour. Sequencing was:

- `triage_skill_has_a_brief_self_check_section` — written first, ran red, then the section was added. Genuine red → green.
- `decision_tree_reaches_the_brief_self_check_rather_than_stranding_it_as_an_appendix` — written before the decision-tree cross-reference existed, ran red, then the cross-reference was added. Genuine red → green.
- `brief_self_check_ships_both_worked_examples_of_the_falsifiability_bar` and `brief_self_check_prescribes_rewrite_first_and_needs_info_as_the_bounded_fallback` — written *after* the section body already contained the examples and the rewrite/`needs-info` wording, so they passed on first run. To confirm they are load-bearing rather than vacuous, each was verified against a mutated copy of the skill (examples reworded; section body replaced with a stub) and observed to fail, then the skill was restored and they went green. This is the pure-prompt-text deviation the kickoff describes, recorded here as advisory context — the ACs themselves are met.

Adding the decision-tree cross-reference initially broke the two section-scoped tests, because `skill_section()` matched the inline `` `## Brief self-check` `` mention inside the tree instead of the real heading. Fixed by anchoring the header match to a line start. Worth keeping in mind if anyone adds further cross-references.

## Stale reference in the brief

The brief states the skill reaches the codex engine "inlined via `CODEX_INLINED_SKILL_TRIAGE` (`src/policy.rs:1965`)". That constant was removed by issue #169 and `src/policy.rs:2444` now carries a comment explaining why (the triage subcommand never calls `wrap_phase_prompt_for_engine`). The brief's conclusion still holds — the skill is the right home for the heuristic, and it reaches the container via the Dockerfile's `COPY skills/` bake — so no work followed from this, but the brief's stated mechanism is out of date and may propagate into future briefs if left uncorrected.
