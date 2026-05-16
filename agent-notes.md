# Agent notes — issue #115

## Informational: TDD commit-shape deviations

The brief calls for strict test-first commit shape — one failing-test
commit then one make-it-pass commit per acceptance criterion. The run
honoured this for the four core ACs that needed implementation
(parse-through-clap for `--repo`, repeatable `--issue`, positional +
`--issue` mutex, and the `--repo` validation/disambiguation/dedup
helper). The remaining ACs were checkpointed as **lock-in tests**
instead — single commits that add a passing test pinning behaviour
already implemented by an earlier cycle. Specifically:

- **Single-repo implicit-resolve** (`resolve_triage_filter_uses_only_repo_in_single_repo_config_without_repo_flag`)
  — already satisfied by the helper introduction. Commit `d94feea`.
- **`--repo` + `--issue` intersection**
  (`cli_parses_triage_with_repo_and_issue_flags_combined` +
  `resolve_triage_filter_intersects_repo_and_explicit_issues_in_multi_repo_config`)
  — the helper already returns the named repo + the explicit list
  unchanged. Commit `844d888`.
- **`--dry-run` combinable with new flags**
  (`cli_parses_triage_with_dry_run_combined_with_repo_and_issue`)
  — `dry_run: bool` predates issue #115 and clap composition is
  automatic. Commit `fa89cbc`.
- **Operator-override semantics**
  (`resolve_triage_filter_passes_through_operator_supplied_issues_without_label_gating`)
  — `call_triage_one` already does not gate on `needs-triage` (slice
  T1 contract), and the helper is pure-config so cannot observe
  labels. The testable surface is that `explicit_issues` round-trips
  the operator input verbatim. Commit `ad7e72f`.

Each lock-in commit message names this file and explains why a RED
commit is impossible. The tests still serve their forward-looking
purpose: a future refactor that silently drops one of those
behaviours will fail the suite.

## Informational: helper introduction was bundled, then re-split

The first attempt at the helper landed the full
`resolve_triage_filter` implementation (`--repo` validation +
multi-repo disambiguation + silent dedup) in one GREEN commit after
the unknown-`--repo` RED test. That violated strict test-first
because the disambig and dedup ACs would have nothing to RED against.
The commit was rolled back with `git reset --soft HEAD~1` and the
helper was trimmed to handle only the unknown-`--repo` branch (commit
`f302e42`). Disambig and dedup then got proper RED→GREEN cycles
(`fa35e7c` → `05286ac`, `66be5b5` → `298b238`).

## No findings of concern

Nothing about `/workspace` raised the kind of red flags described in
the operator brief. Everything touched is first-party Bellows source.

## Unaddressed finding: several issue 115 behaviours were implemented before their tests

Addressing this would require rewriting the already-published issue #115 branch history so each affected acceptance criterion has a failing RED test commit before the implementation commit that makes it pass, specifically splitting or reordering the commits called out in the review finding. I cannot safely do that in this single-finding review-fix run because the branch is already tracking `origin/agent/115-bellows-triage-repo-issue-flags-for-filtered-backl`, and Bellows's post-run flow expects an additive commit/push rather than a force-pushed history rewrite. A human operator should decide whether to rewrite the branch history or accept this PR as a failed/draft handoff.
