# Agent notes — issue #146 (refresh MERGER_PROMPT to advisory framing, ADR-0011)

This is a **pure-prompt-text** change: the acceptance criteria only alter
human-readable prompt prose (`MERGER_PROMPT`) and a doc comment
(`render_merger_prompt`), with no observable behavioural change to verdict
parsing or routing. The verdict parser (`parse_merger_verdict`) and the
`VERDICT: <token>` contract are untouched.

## Test-first shape / deviation

The natural test surface for these ACs is string assertions over
`render_merger_prompt()`.

- AC1 ("prompt no longer asserts the verdict gates/routes the run") was
  driven strictly test-first:
  `render_merger_prompt_does_not_claim_verdict_gates_or_routes_the_run`
  was written and confirmed RED against the unchanged prompt (it asserted
  the absence of the routing phrases that the old prompt contained), then
  the `MERGER_PROMPT` prose change turned it GREEN.
- AC3 (tokens framed as advisory opinions) and AC4 (`[phases.merge].posting`
  toggle still consumes the token) are pinned by
  `render_merger_prompt_frames_tokens_as_advisory_opinions` and
  `render_merger_prompt_mentions_posting_toggle_consumes_the_token`. These
  are positive string assertions for prose that did not exist in the old
  prompt, so they would have failed against the unchanged source; because
  the prose lives in the same const already edited for AC1, they passed on
  first run rather than being individually driven RED→GREEN. This is the
  pure-prompt-text TDD exception (informational channel), not an
  unsatisfied AC.

## AC coverage

- AC "no longer asserts the verdict gates or routes the run": satisfied;
  removed the `classify_exit will reject a MERGE vote`, `the run should land
  as a normal (non-draft) PR`, and `a draft PR is the right shape` claims.
- AC "VERDICT contract preserved": unchanged parser + existing
  `parse_merger_verdict_*` tests still green.
- AC "tokens framed as advisory opinions": MERGE = ship-ready,
  HOLD-NOTED = ship-but-worth-a-look, HOLD-DRAFT = would-hold-if-it-could.
- AC "posting toggle mentioned as still consuming the token":
  `[phases.merge].posting` / `post-on-hold-only` / `## Merge verdict`
  named in the prompt.
- `cargo test --all-targets --all-features` and
  `cargo clippy --all-targets --all-features -- -D warnings` both green.
