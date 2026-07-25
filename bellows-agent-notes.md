Informational notes for the human reviewer of the #161 large-file pre-scan PR.

## Analysis phases could reuse the same section (informational, not widening this PR)

The brief scoped the `## Large files in this repo` kickoff section to the implement phase only — that is where the crash evidence is. The review, security-review and review-fix kickoffs are wrapped through `wrap_phase_prompt_for_engine` (callers around `src/runner.rs`), and those agents also `Read` files in the workspace, so they could hit the same over-cap whole-file-read crash on a large repo.

The plumbing is already reusable: `render_large_files_section(&[LargeFile])` is public and the scan is snapshotted on `Workspace::large_files()`, so threading it into the analysis-phase kickoffs later would be a small follow-up (append the section to each phase body before the engine wrap) rather than new machinery. I deliberately did not widen this PR to those phases, per the brief's out-of-scope note. Flagging it here as informational in case a maintainer wants to pick it up separately.

## Commit shape

Followed test-first authoring per the `tdd` skill: for each acceptance-criterion group a failing-test commit precedes its make-it-pass commit (scanner, policy section, workspace snapshot + announcement). A final commit adjusts one test to satisfy `clippy::cmp_owned`, and a docs commit adds the one-line pointer in `policy-image/CLAUDE.md`.
