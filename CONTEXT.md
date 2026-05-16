# Bellows

Bellows is an AFK orchestrator that dispatches sandbox-isolated AI coding agents (claude, codex) to work on labelled GitHub issues. Operators configure which repos bellows watches; bellows polls, claims issues, runs a multi-phase pipeline per claim, and opens PRs.

## Language

### Engine selection

**Engine**:
A headless agent CLI bellows can dispatch to (today: `claude`, `codex`; planned: `opencode`). Names the *CLI binary*, not the model behind it. Wired through `Engine::*` in `src/config.rs`, `BELLOWS_ENGINE` env var, the `engine:<name>` per-issue label override, the `[auth.<name>]` credentials volume table, and per-engine stderr signatures (`is_rate_limit_signature`, `is_*_auth_error_signature`).
_Avoid_: Model, provider, backend, agent CLI (use "Engine" as the canonical short form).

**Model**:
The specific LLM the chosen **Engine** drives this run (e.g. `opus-4-7`, `gpt-5.5`, `deepseek-v4-pro`). Pinned per chain entry via the `engine:model` syntax in `cli_chain` (e.g. `"opencode:deepseek-v4-pro"`); passed into the container as `BELLOWS_MODEL` and consumed by `run-agent`. When the suffix is omitted, the CLI's default model is used.
_Avoid_: Engine (the CLI is the engine; the model is what it drives).

### Issue dependencies

**Blocker**:
An issue whose work must complete before a dependent issue can be sensibly worked on. Named in the dependent's agent brief under `**Blocked by:**`.
_Avoid_: Dependency, prerequisite, upstream issue.

**Dependent**:
An issue that names one or more blockers in its agent brief and therefore must wait. Bellows applies the `blocked-by` label to dependents so the polling loop can skip them cheaply.
_Avoid_: Downstream issue, child issue, blocked issue (the label is `blocked-by`, but the concept describing the issue itself is **Dependent**).

**Cleared**:
A blocker is **cleared** when its GitHub issue is in the `CLOSED` state, regardless of how it closed (merged PR, manual close, or wontfix). Once every blocker of a dependent is cleared, bellows strips the dependent's `blocked-by` label and the dependent becomes claimable on the next polling pass.
_Avoid_: Resolved, completed, done.

## Relationships

- A **Dependent** has one or more **Blockers**, all named in its agent brief.
- A **Blocker** can have many **Dependents**.
- A **Dependent** is unblocked only when every one of its **Blockers** is **Cleared**.

## Example dialogue

> **Operator:** "Issue #96 is blocked by #95. If I close #95 as wontfix, what happens to #96?"
> **Bellows maintainer:** "Closing #95 — for any reason — counts as **Cleared**. On the next blocked-issue sweep, bellows will strip #96's `blocked-by` label and #96 becomes claimable. If wontfix-ing #95 means #96 no longer makes sense, that's an operator-attention moment — you'd either close #96 too or rewrite its brief. Bellows doesn't second-guess closure intent."

> **Operator:** "I want to swap from DeepSeek V4 Pro to Qwen 3 Coder for the review phase — do I need a new Engine?"
> **Bellows maintainer:** "No — the **Engine** is the CLI (`opencode`); the **Model** is the pin. Flip `phases.review.cli_chain` from `\"opencode:deepseek-v4-pro\"` to `\"opencode:qwen-3-coder\"`. No code change, no new credentials volume, no new engine label."

## Flagged ambiguities

None yet.
