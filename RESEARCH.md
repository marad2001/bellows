# Bellows — Research Notes

> *Source document for `/to-prd`. Captures the research conversation that preceded PRD writing. The next agent should treat this as the briefing — read end-to-end before invoking `/to-prd`.*

## What Bellows is

Bellows is a Rust-based orchestrator that lets AFK ("away from keyboard") AI coding agents pick up labeled GitHub issues, work on them inside isolated Docker sandboxes, and open pull requests for human review. It is a "Rust equivalent of Sandcastle" (Matt Pocock's TypeScript tool) — same architectural family, but written in Rust with first-class support for Rust target codebases.

Bellows itself **does not** contain an LLM agent loop. It spawns headless Claude Code (`claude -p`) inside the sandbox; Claude Code is the brain. Bellows is the plumbing: container management, git, GitHub API, lifecycle policy.

## Workflow context

Bellows is the final stage of an AI-Hero-style pipeline:

1. `grill-with-docs` — research / shared-understanding phase (this conversation)
2. `to-prd` — synthesise a PRD from the research
3. `triage` — move issues through a state machine; the terminal state is `ready-for-agent`, which requires a structured `AGENT-BRIEF.md` comment with concrete testable acceptance criteria
4. **Bellows polls GitHub for `ready-for-agent` issues and dispatches AFK agents to work on them**
5. Each agent does TDD inside its sandbox (`tdd` skill), then opens a PR — draft on failure, regular on success

The gating mechanism that prevents weak-test runs is the triage step: `ready-for-agent` requires concrete, testable acceptance criteria. Bellows trusts that gate.

## Hard constraints

- **Subscription auth only.** The user cannot afford additional API costs. Claude Code inside the sandbox authenticates via OAuth tokens stored in a persistent Docker volume, seeded once with `claude login`. API-key auth is planned as a future option, not for v1.
- **Solo developer, self-taught.** Optimise for the smallest end-to-end thing that works.
- **v1 runs on the developer's laptop only** ("AFK" in v1 means "while my laptop is open and running"). True 24/7 AFK on a VPS is v2.
- **Rust orchestrator confirmed.** Reasons: (a) the developer wants to deepen Rust skills and is using this project as a vehicle, (b) "Sandcastle for Rust" is the brief. Acknowledged trade-off: longer build vs Python/TS, but justified by the learning goal.

## Architectural decisions (v1)

Each of the following was settled in the grilling conversation. Reasoning is included so the PRD can defend each choice.

### Brain vs plumbing

- **Decision:** Claude Code (headless `claude -p`) is the agent loop; Bellows is sandbox + git + GitHub plumbing.
- **Why:** Avoid re-implementing an agent framework. Inherit Claude Code's skills, MCP, hooks, and CLAUDE.md mechanics for free. Smaller scope.
- **Alternative rejected:** Build the agent loop in Rust using `rig` or `llm-chain`. Larger project, weaker ecosystem, no benefit over Claude Code for this use case.

### Sandbox technology and deployment

- **Decision:** Local Docker containers, orchestrator on the developer's laptop. v2 path is a small VPS (Hetzner, ~£4/mo).
- **Why:** Docker is sufficient isolation for trusted-but-fallible agents working on the developer's own repos. No need for Firecracker microVMs (E2B). No need to pay E2B per sandbox-second.
- **Alternative rejected:** E2B managed sandboxes — solves a multi-tenant security problem the developer doesn't have, adds cost.

### Pickup mechanism

- **Decision:** Polling GitHub every 30–60s for issues with the `ready-for-agent` label (or its mapped string in the tracker).
- **Why:** No public endpoint required, no webhook signing, no ngrok. Polling tolerates orchestrator crashes (issues stay labelled). For AFK work, polling latency is irrelevant.
- **Alternative rejected:** GitHub webhooks. Same processing logic, more setup overhead.

### Concurrency model

- **Decision:** Concurrency = 1, hard-coded for v1. Issues queue serially.
- **Why:** Easier to debug. Removes container races on shared volumes (registry / target). Subscription auth doesn't support meaningful parallelism anyway.
- **Locking:** Label-swap pattern. On pickup, remove `ready-for-agent` and add `agent-in-progress` atomically. Polling query is "label = ready-for-agent AND label != agent-in-progress" — re-entrant-safe.

### Git lifecycle

- **Branch naming:** `agent/{issue-number}-{slugified-title}`
- **Branching:** Fresh `git clone` per run, branch from default branch. No auto-rebase if the default branch moves; let the GitHub UI's "branch out of date" warning handle it on review.
- **Merging:** **Human only**, enforced by GitHub branch protection on the target branch. The agent never merges.
- **PR strategy:** Squash-on-merge. The agent's intermediate commits are noise; the final PR description is the durable record.
- **Always open a PR — even on failure.** Failure PRs are opened as **drafts** with logs as a `<details>` comment so the user wakes up to evidence, not to a missing branch.

### TDD integration

- **Decision:** The `tdd` skill is invoked inside the sandbox. The kickoff prompt instructs the agent to write failing tests first based on the agent brief's acceptance criteria, then implement to green, then refactor.
- **The done-signal:** `cargo test` exit code 0. Binary, machine-checkable.
- **Weak-test mitigation:**
  1. The triage gate requires concrete acceptance criteria in `AGENT-BRIEF.md` before issues become `ready-for-agent`.
  2. Bellows runs a post-hoc `git diff` check to confirm at least one new `#[test]` was added (skipped for refactor-tagged issues).

### Programmatic policy baking

User constraint: prompts and skills must be baked in automatically, not something the user remembers to set up.

The policy lives in three layers:

1. **Docker policy image** (rebuilt only when policy changes):
   - Rust toolchain (`rustc`, `cargo`, `clippy`, `rustfmt`)
   - Claude Code binary
   - Curated skills in `~/.claude/skills/` (minimum: `tdd`, likely `diagnose`)
   - Baseline `~/.claude/CLAUDE.md` ("you are headless, do not ask the user, write findings to `/workspace/agent-notes.md` if blocked")

2. **Orchestrator config (`orchestrator.toml`):** model name, label-string mappings, kickoff-prompt template path, stopping rules, auth method, per-repo overrides if needed.

3. **Per-issue runtime:** rendered kickoff prompt (template + issue body + agent brief), repo URL, branch name.

The container invocation is fully constructed by orchestrator code. The user never types a prompt at runtime.

### Auth

- **Decision (v1):** Subscription via persistent Docker volume.
  - One-time setup: developer runs an interactive container with the volume mounted, executes `claude login` inside, completes OAuth.
  - From then on: every agent container mounts the same volume read-write at `/root/.claude`.
  - Concurrency = 1 prevents container races on the volume.
  - The host's interactive Claude Code session uses separate credentials (different OS, different filesystem) and does not collide.
- **Future variant:** API key. Modelled as an enum with two variants (`Subscription`, `ApiKey`). Only `Subscription` is implemented in v1; `ApiKey` is `todo!()`. Switching = edit `orchestrator.toml`, restart.
- **Risk acknowledgement:** Anthropic's stance on programmatic Max-plan use. Headless `claude -p` is officially supported; the AFK pattern with concurrency=1 is consistent with personal use as understood, but if Anthropic tightens subscription-usage rules, the fallback is API mode (which costs money).

### Stopping conditions

| Stop reason | Detection | Action |
|---|---|---|
| Success | `cargo test` exit 0 after agent reports done | Push branch, regular PR, label `agent-done` |
| Wall-clock cap (60 min default) | Orchestrator timer | Kill container, push branch, draft PR, label `agent-failed` |
| Agent self-reports failure | Claude Code clean exit with "couldn't solve" message | Push branch, draft PR, label `agent-failed` |
| Final `cargo test` red | Sanity gate fails after agent reports done | Push branch, draft PR, label `agent-failed` |
| Rate limit hit | stderr/output matches rate-limit signature | Kill container, push branch, draft PR, label `agent-rate-limited` |
| Process crash | Container exits unexpected non-zero | Capture logs, no PR if no branch, label `agent-failed` |
| Human cancel (`bellows kill <issue>`) | External signal | Same as wall-clock, label `agent-cancelled` |

Defaults:
- Wall-clock cap: **60 minutes per issue**, configurable.
- No external iteration cap in v1 (Claude Code's internal session limits + wall-clock are the levers).
- **No auto-retry.** A failed issue stays failed until the user manually re-labels.
- Logs (stderr tail + `cargo test` output) attach as a collapsible `<details>` comment on the PR. They are *not* committed to the branch.
- Failure labels are mutually exclusive with `agent-failed` — use the most specific that applies (`agent-rate-limited`, `agent-cancelled`).

### Build cache strategy

The single biggest Rust-specific operational risk. A cold-cache `cargo build` can burn 20+ minutes of the 60-minute budget.

- **Per-repo named volume** for `target/` (`bellows-target-{repo-slug}`). Mounted on every run for that repo. First run is cold; subsequent runs are incremental.
- **Shared named volume** for the cargo registry (`bellows-cargo-registry`). One copy of crate downloads across all repos.
- Concurrency = 1 makes shared volumes safe (no concurrent `cargo` invocations corrupting locks).
- **`sccache` is a v1.5 enhancement**, not v1. Pointed at a third volume when added.
- Volumes will grow to many GB. Bellows exposes a `prune` subcommand for manual cleanup. No auto-prune in v1.

## v1 scope

The smallest end-to-end thing that works. Every excluded item is additive on this architecture.

**In v1:**

- Runs on the developer's laptop (lid open).
- Polls **one** configured repo every 30–60s.
- Concurrency = 1 (busy/idle, no queue).
- Auth = subscription only (enum variant present, only one implemented).
- Sandbox = local Docker, single policy image, Rust toolchain + Claude Code + `tdd` + baseline CLAUDE.md baked in.
- Caching = per-repo `target/` volume + shared cargo registry volume.
- Stop conditions = 60-min wall-clock, agent self-report, `cargo test` gate, rate-limit detection.
- Outputs = always push branch, open PR (draft on failure), label appropriately, logs as PR comment.
- GitHub auth = fine-grained PAT via env var.
- Configuration = single `orchestrator.toml`.
- Logging = single log file (`tail -f` to watch).

**Explicitly NOT in v1:**

- Multi-repo support (config field present, only first repo read).
- VPS deployment / true 24/7 AFK.
- Webhook listener.
- Concurrency > 1.
- API-key auth (variant exists as `todo!()`).
- `sccache`.
- Auto-prune of cache volumes.
- Web dashboard / TUI / status UI.
- Notifications (Discord, Pushover, etc.).
- Auto-retry of failed issues.
- GitHub App (PAT only).
- Per-repo policy overrides.
- Cost tracking (subscription mode doesn't need it).
- Observability beyond log files.

## Open questions for the PRD stage

These were either deferred during research or are PRD-stage details:

1. **Exact label-string mappings.** Bellows polls for the canonical `ready-for-agent`; the developer's tracker may use a different string (set by `/setup-matt-pocock-skills`). The `orchestrator.toml` schema needs a mapping table.
2. **Kickoff prompt content.** Skeleton: agent brief + acceptance criteria + "use the `tdd` skill" + stop conditions ("stop only when `cargo test` is green"). Exact wording to be drafted at PRD or implementation time.
3. **What goes into the policy image's CLAUDE.md.** Skeleton above, exact wording TBD.
4. **Crash-recovery semantics.** If Bellows itself restarts mid-run, what happens to the in-flight container? Probably "kill orphaned containers tagged `bellows-run-<id>` on startup", but to be designed.
5. **Repo configuration UX.** How does the developer add a new repo to Bellows' watch list? `bellows add-repo <url>` subcommand vs editing TOML by hand.
6. **`bellows setup-auth` subcommand.** First-time interactive container that runs `claude login` into the credentials volume — exact UX TBD.
7. **`bellows kill <issue>` and `bellows status` subcommands.** Required by the design but not yet specified.

## Risks the PRD should address

- **Subscription token refresh edge cases.** If the volume's refresh token expires for real, agent runs fail with auth errors. Mitigation: clear failure mode in logs + a `bellows refresh-auth` subcommand. Documented limitation.
- **Anthropic TOS posture on AFK Claude Code use.** Concurrency=1 + personal use seems consistent with intended use, but if rules tighten, the fallback is API mode. The auth enum makes the switch a config flip.
- **Cold-cache cost on first run per repo.** First issue on a new repo will likely blow the 60-min cap. Documented limitation. Workaround: developer pre-warms the volume with one manual `cargo build` before labelling the first issue. v2 might automate this with a "first-time warm-up" subcommand.
- **Weak agent tests.** Mitigated by triage gate + post-hoc `git diff` check, but not eliminated. Human PR review is the final defence.
- **The orchestrator's own bugs.** Async Rust + Docker SDK (`bollard`) + Octocrab + child process supervision is non-trivial. Solid error handling, structured logging, and integration tests covering the lifecycle are essential.

## Why "Bellows"

Forge metaphor — a bellows pumps air into a fire to make it hotter, "powering the work." Fits the orchestrator role (the orchestrator powers the agents) and sits naturally alongside Sandcastle in evocative-tool-name space without copying it. Web search showed no major namespace conflicts in Rust crates, agent orchestration, or Docker tooling. Standard escape hatches if `bellows` is taken on crates.io: publish as `bellows-cli` or `bellows-rs`; the project name remains Bellows.

## Reference: prior art worth a look

- [Sandcastle](https://github.com/) — Matt Pocock's TypeScript tool, the namesake reference.
- [ComposioHQ/agent-orchestrator](https://github.com/ComposioHQ/agent-orchestrator) — recent prior art doing roughly this for parallel coding agents. Worth a 10-minute read at PRD stage to see what conventions to copy or avoid.

## Suggested module sketch (input for `/to-prd` step 2)

This is a starting point, not a finished design. The PRD step should refine.

- **`poller`** — periodic GitHub query for `ready-for-agent` issues; returns the next issue to work on or `None`.
- **`labeler`** — atomic label-swap operations on issues; the source of truth for issue state in the orchestrator.
- **`workspace`** — fresh `git clone` into a temp dir, branch creation, PR creation/draft, log-comment posting. Wraps Octocrab + `git2` (or shelling to `git`).
- **`sandbox`** — Docker container lifecycle: build/pull policy image, create container with the right mounts and env, stream stdout/stderr, kill on signal. Wraps `bollard`.
- **`policy`** — kickoff prompt rendering, stopping-rule enforcement, exit-reason classification (success / timeout / self-report / rate-limit / crash).
- **`auth`** — the `Auth` enum (`Subscription` / `ApiKey`) with `container_setup()` and `extra_stopping_rules()` methods. Only `Subscription` implemented in v1.
- **`config`** — `orchestrator.toml` parsing and validation. Loaded once at startup.
- **`runner`** — the top-level loop that wires the above together: poll → claim → spawn → wait → finalise → repeat.

These are candidate "deep modules" per the to-prd skill's guidance. The runner is shallow (orchestration glue); the others encapsulate cohesive behaviour behind small interfaces and should be testable in isolation.
