# Bellows v1 — PRD

## Problem Statement

I'm a solo, self-taught developer with a triage workflow that produces well-specified `ready-for-agent` issues — issues with concrete, testable acceptance criteria captured in an `AGENT-BRIEF.md` comment. What I'm missing is the dispatcher: something that watches my GitHub repo while I'm AFK, picks up those labelled issues, runs an agent against them safely, and leaves me a PR to review when I'm back.

Constraints make the off-the-shelf options unworkable:

- I can't afford additional API costs. Whatever I build has to use my existing Claude Max subscription via headless `claude -p`.
- This is a learning vehicle. The orchestrator must be written in Rust — that's the brief ("Sandcastle for Rust") and the personal goal.
- I'm running on a single laptop, not a fleet. Multi-tenant sandbox-as-a-service products (E2B) solve a security problem I don't have, at a cost I can't justify.

So the work the agent does on my behalf needs a dedicated piece of plumbing that's mine, in Rust, that respects subscription auth, runs locally, and produces PRs I can review the same way I'd review a junior teammate's.

## Solution

Bellows is a Rust orchestrator that runs on my laptop while it's open, polling one or more configured GitHub repos every 30–60 seconds for `ready-for-agent` issues. On each tick it walks every configured repo and claims the oldest such issue across the combined set (issue #35 moved multi-repo into v1 scope; the legacy single-`[repo]` table form still parses). Concurrency stays hard-coded to 1 regardless of how many repos are configured. When it finds an issue:

1. **Claim.** Atomic label-swap: remove `ready-for-agent`, add `agent-in-progress`. The poll query is `label = ready-for-agent AND label != agent-in-progress`, so the swap is re-entrant-safe.
2. **Workspace.** Fresh `git clone` into a temp dir. New branch `agent/{issue-number}-{slug}` off the default branch.
3. **Sandbox.** Spawn a Docker container from a baked **policy image** (Rust toolchain + Claude Code + `tdd` skill + baseline CLAUDE.md). Mount:
   - The workspace (RW)
   - A persistent **credentials volume** for subscription OAuth tokens (RW)
   - A per-repo `target/` cache volume (RW)
   - A shared cargo registry cache volume (RW)
4. **Run.** Render a kickoff prompt (issue body + agent brief + "use the `tdd` skill" + stop conditions) and invoke `claude -p` inside the container.
5. **Watch.** Stream stdout/stderr to a log file. Enforce a 60-minute wall-clock cap. Classify exit reason from output signature.
6. **Finalise.** Always push the branch and open a PR. Regular PR on success (`cargo test` exit 0); draft PR on any failure mode. Attach a collapsible `<details>` comment with the logs tail and the final test output. Apply the appropriate outcome label (`agent-done`, `agent-failed`, `agent-rate-limited`, `agent-cancelled`).

Concurrency is hard-coded to 1: issues queue serially. This sidesteps races on the shared cache volumes and credentials volume, makes failure modes easier to debug, and reflects the reality that subscription auth doesn't support meaningful parallelism anyway.

I wake up to a list of PRs — passing ones ready to merge, failing ones with the evidence already attached.

## User Stories

1. As the developer, I want to install Bellows with a single `cargo install` (or equivalent) so that I can get started without a complex setup ritual.
2. As the developer, I want a `bellows setup-auth` subcommand that runs an interactive container with `claude login` so that I authenticate the subscription once and never again.
3. As the developer, I want a single `orchestrator.toml` to configure the repo URL, model name, label mappings, kickoff template path, wall-clock cap, and auth method so that all configuration is in one place I can version-control.
4. As the developer, I want `bellows run` (or just `bellows`) to start the polling loop and stay running in the foreground so that I can see what it's doing with `tail -f` or by watching the terminal.
5. As the developer, I want logs written to a single file in a predictable location so that I can `tail -f` it from another terminal without arguing with structured logging configuration.
6. As the developer, I want Bellows to poll my configured repo every 30–60 seconds for `ready-for-agent` issues so that turnaround is fast enough to feel responsive without hammering the GitHub API.
7. As the developer, I want Bellows to atomically claim an issue by swapping `ready-for-agent` for `agent-in-progress` so that a crashed-and-restarted Bellows never picks the same issue twice.
8. As the developer, I want the polling query to filter out issues already labelled `agent-in-progress` so that a stuck or zombie issue doesn't get re-claimed on every poll.
9. As the developer, I want only one issue worked on at a time so that container races on shared volumes are impossible by construction.
10. As the developer, I want subsequent `ready-for-agent` issues to wait until the current one finishes so that I get serial, predictable behaviour and can reason about state.
11. As the developer, I want each agent run to start from a fresh `git clone` so that no state leaks between issues and recovery is just "delete the temp dir."
12. As the developer, I want the agent's branch named `agent/{issue-number}-{slugified-title}` so that I can identify which branch belongs to which issue at a glance.
13. As the developer, I want the agent's branch created from the default branch as it is at clone time so that the agent works against current `main` without me intervening.
14. As the developer, I want Bellows to leave rebases and "branch out of date" handling to the GitHub UI so that the orchestrator doesn't second-guess my merge strategy.
15. As the developer, I want the agent to run inside a Docker container with no network access beyond what Claude Code needs so that a misbehaving agent can't reach my host filesystem or other services.
16. As the developer, I want the policy image to bake in the Rust toolchain (`rustc`, `cargo`, `clippy`, `rustfmt`) so that the agent has a working Rust environment instantly.
17. As the developer, I want the policy image to bake in the Claude Code binary at a pinned version so that container startup doesn't depend on a network install of the agent itself.
18. As the developer, I want the policy image to bake in the `tdd` skill (and likely `diagnose`) so that I don't have to remember to mount or copy them at runtime.
19. As the developer, I want the policy image to bake in a baseline `~/.claude/CLAUDE.md` that tells the agent it is headless, must not ask the user, and must write blockers to `/workspace/agent-notes.md` so that the agent's behaviour is well-defined when no human is watching.
20. As the developer, I want the policy image rebuilt only when the policy itself changes so that day-to-day runs don't pay a build cost.
21. As the developer, I want subscription auth to work via a persistent Docker volume mounted at `/root/.claude` so that the agent inherits my OAuth tokens without me passing API keys around.
22. As the developer, I want Bellows to model auth as an enum with `Subscription` and `ApiKey` variants — only `Subscription` implemented in v1 — so that a future switch to API mode is a config flip, not a refactor.
23. As the developer, I want a clear failure mode when the credentials volume's refresh token has expired so that I know to re-run `bellows setup-auth` rather than chasing a phantom bug.
24. As the developer, I want a per-repo `target/` cache volume named `bellows-target-{repo-slug}` mounted on every run so that incremental Rust compilation works across runs.
25. As the developer, I want a shared cargo registry cache volume across all repos so that I download crate sources once, not once per repo.
26. As the developer, I want `bellows prune` to let me manually clean cache volumes so that I can recover disk space when the volumes grow into the tens of GB.
27. As the developer, I want Bellows to render a kickoff prompt that includes the issue body, the `AGENT-BRIEF.md` content, and explicit instructions to use the `tdd` skill so that the agent has the full brief without me typing anything at runtime.
28. As the developer, I want the kickoff prompt to specify `cargo test` exit 0 as the stopping signal so that "done" is binary and machine-checkable.
29. As the developer, I want Bellows to enforce a 60-minute wall-clock cap (configurable) so that a stuck run can't burn an entire day.
30. As the developer, I want Bellows to recognise rate-limit signatures in stderr and stop the run cleanly so that I don't waste my budget hammering a closed door.
31. As the developer, I want Bellows to detect when the agent self-reports inability to solve the issue so that those runs end gracefully with evidence rather than dragging on to the wall-clock cap.
32. As the developer, I want Bellows to run a final `cargo test` after the agent reports done as a sanity gate so that an over-eager agent claiming success against a red suite doesn't slip through.
33. As the developer, I want a `bellows kill <issue>` subcommand to abort the current run from another terminal so that I can stop a clearly-doomed run without `Ctrl-C`-ing the orchestrator.
34. As the developer, I want every run — including failures — to push the agent's branch and open a PR so that I always wake up to evidence rather than to a missing branch.
35. As the developer, I want failure PRs marked as drafts so that GitHub's UI signals "review-only, don't merge" without me having to read a label.
36. As the developer, I want successful PRs opened as regular (non-draft) PRs so that they're ready to merge without me toggling them.
37. As the developer, I want the squash-on-merge workflow respected so that the agent's noisy intermediate commits collapse into one PR-shaped commit on merge.
38. As the developer, I want Bellows to attach a collapsible `<details>` log comment to every PR with the stderr tail and the final `cargo test` output so that the evidence sits next to the diff.
39. As the developer, I want logs attached as PR comments rather than committed to the branch so that the branch contains only the agent's actual work.
40. As the developer, I want each terminal state to apply a specific outcome label (`agent-done`, `agent-failed`, `agent-rate-limited`, `agent-cancelled`) so that I can filter PRs by outcome at a glance.
41. As the developer, I want failure labels mutually exclusive with the generic `agent-failed` (use the most specific that applies) so that the issue list isn't cluttered with redundant labels.
42. As the developer, I want a post-hoc `git diff` check that verifies the agent added at least one new `#[test]` (skippable for refactor-tagged issues) so that "weak tests" runs are caught before I open the PR.
43. As the developer, I want the merge gate to be human-only, enforced by GitHub branch protection on the target branch, so that the agent can't merge its own work no matter what the prompt says.
44. As the developer, I want Bellows to never auto-retry a failed issue so that a flaky failure doesn't quietly burn through my subscription quota.
45. As the developer, I want a failed issue to stay failed — labelled and PR-attached — until I manually re-label it `ready-for-agent` so that retries are an explicit human decision.
46. As the developer, I want Bellows to clean up orphaned containers tagged with its run-id on startup so that an orchestrator crash doesn't leave Docker state behind.
47. As the developer, I want a `bellows status` subcommand that reports whether the orchestrator is idle, busy, and which issue it's working on so that I can check progress without parsing logs.
48. As the developer, I want GitHub authentication via a fine-grained PAT in an env var so that v1 setup doesn't require a GitHub App.
49. As the developer reviewing the agent's PR, I want to see which acceptance criterion each new test maps to so that I can verify the brief was actually satisfied (delivered via the agent's PR description, written by Claude Code, not Bellows).
50. As the developer reviewing the agent's PR, I want the PR description to summarise what the agent attempted, what worked, and what didn't so that I can decide quickly whether to merge, request changes, or close.
51. As the developer reviewing the agent's PR, I want failure PRs to include the last 100 lines of stderr and the full final `cargo test` output in the log comment so that I have the same diagnostic surface I'd have if I'd run it myself.
52. As the developer, I want Bellows to support one or more configured repos in v1 (single `[repo]` table or `[[repo]]` array-of-tables; the polling loop picks the oldest `ready-for-agent` issue across the combined set) so that one bellows process can drive multiple repos from the same laptop. Concurrency stays at 1 across all repos.
53. As the developer, I want VPS deployment, webhooks, concurrency > 1, API-key auth, `sccache`, auto-prune, dashboards, notifications, auto-retry, GitHub App auth, per-repo policy overrides, cost tracking, and observability beyond log files all explicitly out of scope for v1 so that scope creep doesn't kill the project.
54. As the developer, I want the cold-cache cost of the first run on a new repo documented as a known limitation so that I don't mistake a 90-minute first run for a bug.
55. As the developer, I want Bellows's TOS posture (concurrency=1, personal subscription use) documented so that I understand the fallback (API mode via the auth enum) if Anthropic tightens the rules.

## Implementation Decisions

**Module layout (seven modules, four deep):**

- **`tracker`** (deep) — combines issue polling and label state machine into one cohesive module. Owns every GitHub-issue-state interaction: poll for `ready-for-agent`, atomic label-swap claim, transition labels on stop reasons, post log-comments. Hides Octocrab. Merging poller and labeler avoids a leaky boundary — re-entrant-safe claim depends on the polling query, so they need to live together.
- **`workspace`** (deep) — fresh `git clone` into a temp dir, branch creation, push, PR creation (regular or draft), log-comment posting. Wraps Octocrab and either `git2` or shells to `git`.
- **`sandbox`** (deep) — Docker container lifecycle: build/pull policy image, create container with the right mounts and env, stream stdout/stderr, kill on signal, classify exit. Wraps `bollard`.
- **`policy`** (deep, pure) — kickoff-prompt rendering and exit-reason classification (success / timeout / self-report / rate-limit / crash). No I/O, all logic. Designed for unit tests with no fakes.
- **`auth`** (moderate) — `Auth` enum with `Subscription` and `ApiKey` variants. Methods: `container_setup()` (returns the mounts and env to add) and `extra_stopping_rules()` (returns extra exit signatures to watch for). v1 implements `Subscription`; `ApiKey` is `todo!()`.
- **`config`** (shallow) — `orchestrator.toml` parsing and validation. Loaded once at startup. Includes label-string mapping table, repo URL, model name, kickoff-template path, wall-clock cap, auth selection.
- **`runner`** (shallow, glue) — top-level loop that wires the others together: poll → claim → spawn → wait → finalise → repeat.

**Container invocation is fully constructed by code.** The user never types a prompt at runtime. The kickoff prompt is rendered from a template + the issue body + the agent brief + stop conditions.

**Three-layer policy:**

1. **Docker policy image** (rebuilt only when policy changes): Rust toolchain, Claude Code binary, curated skills (`tdd`, likely `diagnose`), baseline `~/.claude/CLAUDE.md`.
2. **`orchestrator.toml`**: model name, label mappings, kickoff-prompt template path, wall-clock cap, auth method.
3. **Per-issue runtime**: rendered kickoff prompt, repo URL, branch name.

**Stopping condition matrix** (codified in `policy`):

| Stop reason | Detection | Outcome |
|---|---|---|
| Success | `cargo test` exit 0 after agent reports done | regular PR, label `agent-done` |
| Wall-clock cap (60 min default) | orchestrator timer | draft PR, label `agent-failed` |
| Agent self-reports failure | clean exit + "couldn't solve" message | draft PR, label `agent-failed` |
| Final `cargo test` red | sanity gate after agent reports done | draft PR, label `agent-failed` |
| Rate limit | stderr signature match | draft PR, label `agent-rate-limited` |
| Process crash | unexpected non-zero exit | logs only, label `agent-failed` |
| Human cancel (`bellows kill`) | external signal | draft PR, label `agent-cancelled` |

**Cache strategy:**

- Per-repo `target/` named volume: `bellows-target-{repo-slug}`. Mounted on every run for that repo.
- Shared cargo registry named volume: `bellows-cargo-registry`. One copy of crate downloads across all repos.
- Concurrency = 1 makes shared volumes safe (no concurrent `cargo` invocations corrupting locks).
- `sccache` is a v1.5 enhancement, not v1.
- `bellows prune` for manual cleanup. No auto-prune in v1.

**Auth model:**

- `Auth::Subscription` (v1): persistent Docker volume mounted at `/root/.claude`. One-time setup via `bellows setup-auth` runs an interactive container with `claude login`.
- `Auth::ApiKey` (v2): `todo!()` for now. Switch is a config edit + restart.

**Crash recovery:**

- Bellows tags every container with a run-id (e.g., `bellows-run-<id>`).
- On startup, Bellows scans for orphaned containers with that tag prefix and kills them.
- Issues stay claimed (`agent-in-progress`) across crashes; the human re-labels back to `ready-for-agent` to retry.

**CLI surface (v1):**

- `bellows run` (or `bellows`) — start the polling loop in the foreground.
- `bellows setup-auth` — interactive `claude login` into the credentials volume.
- `bellows kill <issue>` — abort the current run.
- `bellows status` — report idle/busy + current issue.
- `bellows prune` — clean cache volumes.
- `bellows refresh-auth` — same as `setup-auth`, semantic alias for the "my token expired" case.

**Outputs (every run, regardless of outcome):**

- Branch pushed (when a clone existed).
- PR opened: regular if success, draft otherwise.
- Log `<details>` comment attached: stderr tail + final `cargo test` output.
- Outcome label applied (most-specific wins).

**`orchestrator.toml` schema (sketch, finalised at implementation time):**

- `[repo]`: `url`, `default_branch_override` (optional)
- `[labels]`: mapping for each canonical role to the actual GitHub label string (tracks `docs/agents/triage-labels.md`)
- `[agent]`: `model`, `wall_clock_minutes`, `kickoff_template_path`
- `[auth]`: `method = "subscription"` (only valid value in v1)
- `[cache]`: volume name overrides (optional)

## Testing Decisions

**What makes a good test in this codebase:** test external behaviour, not implementation details. A good test fixes the contract a module promises to its callers; it does not pin internal helper signatures or mock private collaborators. Tests should survive refactors that don't change behaviour.

**Modules to test (chosen with the user):**

1. **`policy`** — unit tests, no fakes needed. Inputs: stdout/stderr samples, exit codes, agent-message text. Outputs: classified `ExitReason` enum. Also kickoff-prompt rendering: template + brief → string match. Pure logic; cheapest, highest-ROI tests in the project.
2. **`tracker`** — integration tests against `gh` CLI fakes or recorded HTTP fixtures. Critical correctness target: the label state machine. Verify atomic claim, idempotent transitions, and that the polling query excludes claimed issues. A bug here means double-runs or stuck issues.
3. **`workspace`** — integration tests against a temp git repo + Octocrab fakes. Verify branch naming, push, PR creation (regular vs draft), log-comment posting. A bug here means a missing PR or a misnamed branch — visible to the human reviewer, but worth catching pre-merge.

**Modules NOT covered by automated tests in v1:**

- `sandbox` — Docker integration tests are valuable but expensive (real daemon required). Deferred to v1.5. Manual verification during early runs is the v1 strategy.
- `auth` — small surface; covered indirectly by `sandbox` and `tracker` tests at the seam.
- `config` — small surface; covered by `serde` deserialisation tests if anything turns up wrong.
- `runner` — pure glue; tested implicitly by running Bellows against a real issue. End-to-end verification, not unit tests.

**Prior art:** none in this repo (greenfield). The TDD pattern follows `cargo test` conventions: integration tests in `tests/`, unit tests in `#[cfg(test)] mod tests` blocks beside the code. Use `tempfile` for temp dirs, `wiremock` or recorded fixtures for HTTP, `assert_cmd` for CLI subcommand tests.

## Out of Scope

The following are explicitly **not** in v1. Each is additive on the v1 architecture and will land in a later milestone:

- **VPS deployment / true 24/7 AFK.** v1 runs on the laptop with the lid open.
- **Webhooks.** Polling only.
- **Concurrency > 1.** Hard-coded to 1.
- **API-key auth.** Enum variant exists as `todo!()`.
- **`sccache`.** Deferred to v1.5.
- **Auto-prune of cache volumes.** Manual via `bellows prune`.
- **Web dashboard / TUI / status UI** beyond `bellows status`.
- **Notifications** (Discord, Pushover, etc.).
- **Auto-retry** of failed issues. Human-triggered only.
- **GitHub App.** PAT-only.
- **Per-repo policy overrides.** Single policy image for all runs.
- **Cost tracking.** Subscription mode doesn't need it.
- **Observability beyond log files.** No Prometheus, no OTLP, no tracing dashboards.
- **Sandbox tests** in the automated suite. Manual verification only in v1.

## Further Notes

**TOS posture.** Headless `claude -p` is officially supported. Concurrency = 1 with personal subscription use is consistent with how the Max plan is intended to be used as currently understood. If Anthropic tightens subscription-usage rules, the fallback is API mode — the auth enum is structured exactly so that switch is a config flip, not a refactor. Document this limitation visibly in the README.

**Cold-cache risk.** The first `cargo build` on a new repo can burn 20+ minutes of the 60-minute budget. Documented as a known limitation. Workaround for v1: pre-warm the volume with one manual `cargo build` before labelling the first issue on a new repo. v1.5 may automate this with a "first-time warm-up" subcommand.

**Weak tests.** The triage gate (`ready-for-agent` requires concrete acceptance criteria) is the primary defence. The post-hoc `git diff` check (verify at least one new `#[test]`) is a backstop. Human PR review is the final defence. None of these is bulletproof; the layered approach is the v1 plan.

**The orchestrator's own bugs.** Async Rust + Docker SDK (`bollard`) + Octocrab + child-process supervision is non-trivial. Solid error handling, structured logging, and integration tests covering the lifecycle are essential. The deep modules (`tracker`, `workspace`, `policy`) are designed specifically to be testable in isolation; the shallow `runner` is glue and will be exercised end-to-end.

**Why Rust.** The brief is "Sandcastle for Rust." Acknowledged trade-off: longer build vs Python/TS, but justified by the personal goal of deepening Rust skills. The architecture decisions (deep modules, enum-based polymorphism for `Auth`) are chosen partly to make the project a good Rust learning vehicle.

**Naming.** Forge metaphor: a bellows pumps air into a fire to make it hotter. The orchestrator powers the agents. Standard fallback if `bellows` is taken on crates.io: publish as `bellows-cli` or `bellows-rs`; the project name remains Bellows.

**Reference prior art.**
- [Sandcastle](https://github.com/) — Matt Pocock's TypeScript namesake.
- [ComposioHQ/agent-orchestrator](https://github.com/ComposioHQ/agent-orchestrator) — recent prior art for parallel coding agents. Worth a 10-minute read at implementation time to copy or avoid conventions.

**Open detail items deferred to implementation time:**

1. Exact wording of the kickoff-prompt template.
2. Exact wording of the policy image's baseline `~/.claude/CLAUDE.md`.
3. Exact `orchestrator.toml` field names (sketch above).
4. Exact `bellows status` output format.
5. Exact `bellows kill <issue>` semantics (graceful stop vs SIGKILL).
6. Whether `workspace` uses `git2` or shells to `git` — pick at implementation time based on which is less painful.
