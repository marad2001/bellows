# Agent notes — issue #127 (`bellows triage --repo` flag)

## TDD-shape deviation: implementation already exists from #115

The brief for #127 says: "Issue #115 attempted a `--repo` flag and landed
`agent-failed` — the binary's `--help` shows no `--repo` option today."

That statement is no longer accurate on `main`. The work for #115 has
since landed (commit `8b8e845`, "Bellows agent run for issue #115
(#121)"). The current `main` branch source already contains:

- the `repo: Option<String>` field on `Command::Triage` in
  `src/main.rs:107-108`;
- the `--issue` repeated flag in `src/main.rs:115-121`;
- the `resolve_triage_filter` pure helper in `src/main.rs:950-1032` that
  does `--repo` slug validation, multi-repo bare-ref disambiguation,
  unknown-repo error reporting, and silent dedup of repeated `--issue`;
- `triage_dispatch_plan` in `src/main.rs:934-942` that uses the
  selected non-first repo for workspace, cache slug, and deploy keys;
- the per-issue / drain / explicit-issues dispatch wiring in
  `triage_cmd` (`src/main.rs:246-326`).

Running `bellows triage --help` on the current binary already shows
`--repo <OWNER/NAME>`, so the AC1 surface ("`bellows triage --help`
shows a `--repo <owner>/<name>` option") is already met.

All six acceptance criteria in #127's brief are therefore already
satisfied by source code that landed in #121, not by any new source
change on this branch.

## What this branch actually does

Because the implementation is already in place, I could not honestly
drive the source changes test-first — there were no source changes
to drive. To honour the brief's "tests cover …" expectation without
fabricating a red-green-refactor loop that would have created bogus
commits (e.g. temporarily breaking working code just so a test could
go red against it), I added regression tests only:

- `cli_triage_help_surfaces_the_repo_flag_with_owner_name_value_name`
  pins AC1 (`--help` surfaces `--repo` with an `OWNER/NAME` value
  placeholder).
- `resolve_triage_filter_positional_with_repo_flag_resolves_to_named_repo`
  pins AC3 (`bellows triage 42 --repo X` resolves to `X`, not the
  first-configured repo) — this scenario was not previously covered
  by a dedicated test; the closest existing test
  (`resolve_triage_filter_intersects_repo_and_explicit_issues_in_multi_repo_config`)
  uses `--issue` not the positional form.
- `resolve_triage_filter_no_repo_flag_in_multi_repo_drain_falls_back_to_first_repo`
  pins AC5 (no-`--repo` bare-drain in multi-repo config still
  falls back to first `[[repo]]`) — also not previously covered as a
  named test in isolation.

These three tests pass on unchanged source (the impl already exists)
and would have failed on pre-#115 source. They are regression
coverage, not driving tests. This is a deliberate TDD deviation in
the informational channel — the ACs are satisfied, and a human
reviewer should know why the commit shape on this branch is
"tests only, no source change".

## Commit shape

Because there is no source change, there is no "red → green" commit
pair to produce. This branch has a single commit adding the three
regression tests, plus this agent-notes file.

## Verification

`cargo test` is green: 78 passed in the binary suite (up from 75 on
`main`), all other test binaries unchanged.
