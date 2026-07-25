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

**Advance**:
Abandoning the current **Engine**'s attempt at a phase, discarding its workspace changes, and re-running that phase from the base commit under the next chain entry. The judgement is always "this engine cannot make progress on this issue," never "this engine produced bad work" — an advance throws away an attempt, not a verdict.
_Avoid_: Retry, fallback, failover, escalation (an advance changes engine; a retry would not).

### Run progress

**Stall**:
A run in which the **Engine** is no longer making progress against the workspace. An umbrella for two distinct shapes — **Oscillation** and **Idleness** — which differ in whether the lack of progress is unambiguous.
_Avoid_: Thrash, loop, spinning, hang, stuck.

**Oscillation**:
The shape of **Stall** in which the workspace cycles through repeated states — an edit made, reverted, and made again. Unambiguous: no healthy run does this. Oscillation is the only **Stall** shape that justifies an **Advance**.
_Avoid_: Thrash, flapping, churn.

**Idleness**:
The shape of **Stall** in which the workspace is unchanged for a prolonged stretch. Ambiguous by nature — indistinguishable from an **Engine** reasoning about a hard problem, or one that has finished and is about to exit. Recorded for the operator, never acted on.
_Avoid_: Hang, freeze, timeout, inactivity.

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
- A **Stall** is either an **Oscillation** or an **Idleness**, never both at once.
- An **Oscillation** triggers an **Advance**; an **Idleness** never does.
- An **Advance** has two independent triggers: a rate-limited **Engine**, and an **Oscillation**. Both mean the same thing — no progress is available from this engine — and both produce the same response.

## Example dialogue

> **Operator:** "Issue #96 is blocked by #95. If I close #95 as wontfix, what happens to #96?"
> **Bellows maintainer:** "Closing #95 — for any reason — counts as **Cleared**. On the next blocked-issue sweep, bellows will strip #96's `blocked-by` label and #96 becomes claimable. If wontfix-ing #95 means #96 no longer makes sense, that's an operator-attention moment — you'd either close #96 too or rewrite its brief. Bellows doesn't second-guess closure intent."

> **Operator:** "I want to swap from DeepSeek V4 Pro to Qwen 3 Coder for the review phase — do I need a new Engine?"
> **Bellows maintainer:** "No — the **Engine** is the CLI (`opencode`); the **Model** is the pin. Flip `phases.review.cli_chain` from `\"opencode:deepseek-v4-pro\"` to `\"opencode:qwen-3-coder\"`. No code change, no new credentials volume, no new engine label."

> **Operator:** "The log says the workspace hasn't changed in twenty minutes. Why hasn't bellows done something about it?"
> **Bellows maintainer:** "Because that's **Idleness**, not **Oscillation**. A still workspace looks identical whether the engine is deep in thought, genuinely wedged, or thirty seconds from a clean exit — and an **Advance** discards the workspace, so acting on it risks throwing away a run that was about to succeed. We record **Idleness** and let the wall-clock budget be the backstop. Only **Oscillation** — the same edit made, reverted, and made again — is unambiguous enough to advance on."

> **Operator:** "So an **Advance** means the engine did bad work and we're punishing it?"
> **Bellows maintainer:** "No — an **Advance** is never a verdict on quality. It says only that no further progress is available from this engine on this issue. That's why the same response covers a rate limit and an **Oscillation**: in both cases the engine has nothing more to give, and the cheapest next move is to hand the issue to the next chain entry from a clean base. The work is discarded because it's unfinished, not because it's bad."

## Flagged ambiguities

None yet.
