# Bellows

[![CI](https://github.com/marad2001/bellows/actions/workflows/ci.yml/badge.svg)](https://github.com/marad2001/bellows/actions/workflows/ci.yml)

> AFK Claude Code orchestrator for Rust repos. Bellows watches a GitHub
> issue tracker, picks up issues that you've triaged to `ready-for-agent`,
> and runs a sandboxed Claude Code agent inside a Docker container against
> a fresh clone of your repo. The agent works the issue test-first, the
> orchestrator gates on `cargo clippy` and `cargo test`, an automated
> code-review phase pushes follow-up findings, and the result lands on
> GitHub as a pull request (draft on failure, regular on success) for a
> human to review.

Bellows is the plumbing — container lifecycle, git, GitHub API, label
state machine, wall-clock budget, log shipping. Claude Code (running
headless inside the sandbox via `claude -p`) is the brain. V1 is a
single-repo, single-in-flight-issue orchestrator that runs on the
developer's laptop, authenticated to Anthropic via a personal
subscription. See [`RESEARCH.md`](RESEARCH.md) for the full design
briefing and [`PRD.md`](PRD.md) for the v1 contract.

## Prerequisites

Before installing bellows, you need:

- **Docker** running on the host (Docker Desktop on Windows/macOS,
  Docker Engine on Linux). Every agent run happens inside a container
  built from the policy image bellows ships; if Docker isn't reachable
  via the local socket, bellows can't dispatch.
- **A Rust toolchain** (stable, edition 2024). Needed to build bellows
  from source — bellows is not published to a package manager in v1.
- **A GitHub Personal Access Token (fine-grained)** for the target
  repo with the following permissions: **Issues: read & write**,
  **Pull requests: read & write**, **Contents: read & write**. Bellows
  reads this token via the env var named in `orchestrator.toml`'s
  `[github].pat_env_var` (default `BELLOWS_GITHUB_TOKEN`).
- **The target repo's GitHub issue tracker** with the canonical bellows
  label vocabulary already in place. The full list is below in
  [One-time setup](#one-time-setup) — you can create the labels through
  the GitHub UI or `gh label create`.

## Install

Clone the bellows repo and install the binary into your Cargo bin
directory:

```bash
git clone https://github.com/marad2001/bellows.git
cd bellows
cargo install --path .
```

`bellows` will land on your `PATH` if `~/.cargo/bin` is on it. There is
no `cargo install bellows` from crates.io in v1 — bellows is not
published to a package manager.

If you'd rather not install globally, `cargo build --release` then run
`./target/release/bellows` from the clone — every subcommand below works
the same way against the built binary.

## One-time setup

There are three things to set up once per operator + target-repo
pairing: the config file, the GitHub PAT in your environment, and the
Claude Code credentials volume. After that, daily use is a single
`bellows run` command.

### 1. Copy and edit the config

The canonical config schema lives in
[`orchestrator.example.toml`](orchestrator.example.toml). Copy it to
`orchestrator.toml` (the real file is gitignored, so your
deployment-specific values stay out of source control):

```bash
cp orchestrator.example.toml orchestrator.toml
$EDITOR orchestrator.toml
```

Sections of `orchestrator.toml`, all of which are read at startup:

- **`[repo].url`** or **`[[repo]]` array-of-tables** — the GitHub
  repositories bellows polls. A single `[repo]` table with one `url`
  configures one repo (the legacy single-repo shape, still accepted
  unchanged). A `[[repo]]` array-of-tables block with one entry per
  repo configures many — the polling loop walks every configured
  repo on each tick and claims the oldest `ready-for-agent` issue
  across the combined set. Concurrency stays at 1 across all repos
  (one agent run at a time, regardless of how many repos are
  configured).
- **`[github].pat_env_var`** — the name of the environment variable
  bellows will read your PAT from. Defaults to `BELLOWS_GITHUB_TOKEN`.
  Renaming this is purely cosmetic; bellows never reads the token from
  the file directly.
- **`[polling]`** — how often to poll GitHub
  (`interval_seconds`, default 45) and which label flags an issue as
  ready (`pickup_label`, default `ready-for-agent`).
- **`[runtime_labels]`** — the label strings bellows applies as the
  agent moves through its state machine (`agent_in_progress`,
  `agent_done`, `agent_failed`, `agent_rate_limited`,
  `agent_cancelled`). Defaults match the names verbatim; override only
  if your tracker uses different strings.
- **`[logging].path`** — where bellows writes its single log file.
  Default `bellows.log` in the current working directory; `tail -f` is
  the intended UX.
- **`[auth]`** — `method = "subscription"` (the only v1 variant) and
  `credentials_volume`, the name of the Docker volume holding the
  Claude Code OAuth session (default `bellows-claude-credentials`).
- **`[agent].wall_clock_minutes`** — the per-issue wall-clock budget.
  Default 60. Bellows tracks elapsed time across all containers in the
  pipeline (implement + post-implement gate + review + review-fix +
  end-pipeline gate); when the budget is spent the run halts and
  routes to a draft PR with `agent-failed`.

### 2. Export your GitHub PAT

Put the token wherever your shell picks it up at startup
(`~/.zshrc`, `~/.bashrc`, a `direnv` `.envrc`, etc.):

```bash
export BELLOWS_GITHUB_TOKEN=ghp_yourtokenhere
```

The variable name must match `[github].pat_env_var`. Bellows refuses
to start if the variable is unset, so you'll know immediately.

### 3. Seed the Claude Code credentials volume

The agent inside the sandbox authenticates to Anthropic via a
persistent Docker volume holding an OAuth session — your personal
Max-plan subscription. Seed it once with `bellows setup-auth`:

```bash
bellows setup-auth
```

This opens an interactive Claude Code session inside a one-shot
container, with the credentials volume mounted at the right path.
Inside the container, type `/login` to start the OAuth flow, complete
it in your browser, then `/exit`. The volume retains the tokens; every
future agent run mounts the same volume read-write. Concurrency=1
prevents container races on the volume. If the refresh token expires
later, [`bellows refresh-auth`](#daily-use) re-seeds it the same way.

### 4. (Optional) Seed SSH deploy keys for private cross-repo deps

Skip this step if every repo bellows polls has its dependency tree
fully on crates.io or in the same monorepo. Reach for it when a
target repo's `Cargo.toml` references private git deps via SSH URLs
(`ssh://git@github.com/owner/repo.git`) — without credentials, cargo
inside the sandbox cannot fetch those deps and the agent + the
cargo-checks gate both crash on `cargo fetch`. The cure (per
[ADR-0002](docs/adr/0002-ssh-deploy-keys-for-private-cross-repo-deps.md))
is per-repo opt-in SSH deploy keys mounted read-only at
`/home/bellows/.ssh/` into the containers spawned for the opted-in
`[[repo]]`. Keys live in a bellows-managed Docker volume (parallel to
the credentials volume), default name `bellows-deploy-keys`,
overridable via `[auth].ssh_keys_volume`.

The four steps per shared private crate:

1. **Generate or pick a deploy key.** A deploy key is a single
   SSH keypair scoped to one private GitHub repo (the one carrying
   the shared crate). Generate one with `ssh-keygen -t ed25519 -f
   ./deploy_key_workboard_core -C "bellows deploy key for workboard-core"`
   if you don't have one to hand. The private half stays on the
   operator's machine; the public half (`*.pub`) goes onto GitHub
   in step 2.

2. **Register the public half on GitHub.** On the shared crate's
   repo (e.g. `marad2001/workboard-core`), go to **Settings → Deploy keys → Add deploy key**, paste the contents of the `*.pub` file,
   give it a clear title (the bellows volume key name is a good
   match — e.g. `workboard-core`), and leave **Allow write access**
   unchecked. Cargo only needs read access; an escaping agent with
   write would be much worse.

3. **Import the private half into the bellows volume.** Pipe the
   private half into `bellows setup-deploy-keys add <name>` — the
   `<name>` is what the consuming `[[repo]]` block will reference,
   and what shows up in `bellows setup-deploy-keys list` later.

   ```bash
   bellows setup-deploy-keys add workboard-core < ./deploy_key_workboard_core
   ```

   The subcommand runs inside a one-shot container with the
   deploy-keys volume mounted: it writes the key with mode 600,
   ensures `/home/bellows/.ssh/config` has a `Host github.com` stanza
   pointing at the new key with `IdentitiesOnly yes`, and seeds
   `/home/bellows/.ssh/known_hosts` via `ssh-keyscan`. Re-running
   `add` for the same name is idempotent.

   For a self-hosted git server, override the host:
   `bellows setup-deploy-keys add my-key --ssh-host git.example.com`.

   `bellows setup-deploy-keys list` shows every key the volume holds
   plus the Host stanzas; `bellows setup-deploy-keys remove <name>`
   reverses an `add` (idempotent on a missing key).

4. **Reference the key from the consuming repo's `[[repo]]` block.**
   In `orchestrator.toml`, add `deploy_keys = [...]` to every
   `[[repo]]` that depends on the private crate:

   ```toml
   [[repo]]
   url = "https://github.com/marad2001/workboard-financial-advice"
   deploy_keys = ["workboard-core"]
   ```

   This is the explicit opt-in. `[[repo]]` blocks without a
   `deploy_keys` list (or with an empty list) get no SSH mount —
   preserving the "no creds in sandbox by default" posture for
   every repo that doesn't need private-dep access. Bellows
   validates at startup that every name in every `deploy_keys =
   [...]` references a key actually present in the volume, and
   refuses to start when one is missing — naming the missing key
   and the offending repo so you can re-run `setup-deploy-keys add`
   immediately instead of hitting a confusing cargo-fetch crash
   later inside the sandbox.

### 5. Create the GitHub label vocabulary

Bellows operates a label state machine on the target repo, so the
labels need to exist before bellows starts polling. There are two
groups.

**Triage labels** (used by the triage skill to move issues into the
`ready-for-agent` state; see
[`docs/agents/triage-labels.md`](docs/agents/triage-labels.md)):

- `needs-triage` — maintainer needs to evaluate this issue.
- `needs-info` — waiting on reporter for more information.
- `ready-for-agent` — fully specified, ready for an AFK agent.
- `ready-for-human` — requires human implementation.
- `wontfix` — will not be actioned.

Plus the two category labels that the triage skill uses on every
issue:

- `bug` — the issue describes a defect in existing behavior.
- `enhancement` — the issue describes new or changed behavior.

**Runtime labels** (applied by bellows to the **issue** as a run
progresses — the PR opened by the run is itself unlabeled; override
the strings via `[runtime_labels]` in `orchestrator.toml` if you need
to):

- `agent-in-progress` — bellows has claimed the issue; a container is
  running or about to run.
- `agent-done` — the run finished successfully; PR opened, `cargo
  test` was green.
- `agent-failed` — the run finished unsuccessfully; a draft PR was
  opened with logs as a `<details>` comment.
- `agent-rate-limited` — a draft PR was opened because Anthropic
  rate-limited the agent mid-run.
- `agent-cancelled` — an operator ran `bellows kill <issue>`; the
  container was stopped and a draft PR was opened.

Quick way to create all of these with the `gh` CLI:

```bash
for label in needs-triage needs-info ready-for-agent ready-for-human wontfix bug enhancement \
             agent-in-progress agent-done agent-failed agent-rate-limited agent-cancelled; do
  gh label create "$label" --force
done
```

### 6. Cargo-checks gate mirrors target CI

Per [ADR-0004](docs/adr/0004-bellows-gate-mirrors-target-ci.md),
bellows's cargo-checks gate is target-CI-aware: at workspace-prepare
time it reads the target repo's `.github/workflows/*.yml`, finds the
workflow named `CI`, and snapshots the literal `cargo clippy ...` and
`cargo test ...` invocations declared in that workflow's Linux-runner
job. The gate then runs those snapshotted commands verbatim inside
the sandbox — the same posture CI runs, by construction. This is
**how bellows guarantees "gate passes ⇒ CI passes"** without making
the operator maintain two specs in sync.

When the workflow can't be parsed — no `.github/workflows/` directory
at all, malformed YAML, no workflow named `CI`, or the clippy/test
step wrapped in a shell script bellows can't follow — bellows falls
back to operator-declared defaults in `orchestrator.toml`:

```toml
[gates]
clippy_flags = "--all-targets --all-features -- -D warnings"
test_flags   = "--all-targets --all-features"
```

The defaults preserve today's strict bar. Override either flag set
when the workflow is unparseable but you still want to mirror a
specific posture (e.g. `clippy_flags = "-- -D clippy::correctness -D
clippy::suspicious"` for repos that deliberately narrow clippy so
pre-existing latent debt doesn't block new work). The per-command
fallback is independent — bellows can extract `cargo test` from the
workflow while still falling back on `clippy_flags` if clippy alone
is unparseable. At each gate-phase start, the run-log states the
actual command and its provenance (`parsed from .github/workflows/...`
vs `fallback from [gates].clippy_flags`) so an operator reading the
log can always tell which path took effect.

### 7. Branch protection setup

Bellows runs its own `cargo clippy` + `cargo test` gate inside the
sandbox on every agent commit, but that gate doesn't run on commits a
human pushes directly to a feature branch (review fix-ups, drive-by
edits, etc.). The GitHub Actions workflow at
[`.github/workflows/ci.yml`](.github/workflows/ci.yml) re-runs the
exact same checks on every pull request and on every push to `master`,
so the merge button is gated by the same verdict bellows uses
internally. Wire it into branch protection on `master` so the gate is
mechanically required, not relied on by memory.

Open **Settings → Branches → Add branch protection rule** for the
`master` branch and apply these settings:

| Setting | Value |
| --- | --- |
| Branch name pattern | `master` |
| Require a pull request before merging | enabled |
| Require status checks to pass before merging | enabled |
| Required status check | `ci` |
| Require linear history | enabled (squash-on-merge friendly) |
| Block force pushes | enabled |
| Block deletions | enabled |
| Allow administrators to bypass | enabled (operator emergency lever) |

**GitHub-UI ordering quirk.** The required-status-check picker only
lists checks GitHub has already *seen* fire on the branch. On a
freshly added workflow, the `ci` check is **not** selectable in the
protection rule until the workflow has run at least once on `master`.
The post-merge ordering is therefore:

1. Merge this PR (or a later one) to `master`.
2. The push-to-master event fires `ci.yml`; the workflow run shows
   up in the Actions tab.
3. Reopen **Settings → Branches** and edit the `master` protection
   rule. The `ci` check is now visible in the required-checks picker;
   tick it and save.

After that, every subsequent PR (human- or agent-authored) has to
show `ci` green before the merge button enables, and no path to
`master` bypasses the gate except the explicit admin override above.

## Daily use

The v1 operator-facing toolkit is four subcommands: `run`, `status`,
`kill`, `refresh-auth`. Three of these (`run`, `status`,
`refresh-auth`) ship today; `kill` is planned for a later slice and
carries an explicit "planned" note pointing at its tracking issue so
this README captures the full v1 surface in one place.

### `bellows run` — start the polling loop

```bash
bellows run
```

`run` is the foreground polling loop. It reads `orchestrator.toml` from
the current directory (override with `--config path/to/file`), connects
to Docker, cleans up any orphan containers left by a prior bellows
process (slice 7), then polls GitHub every `interval_seconds`. When it
sees a `ready-for-agent` issue, it swaps that label for
`agent-in-progress` and dispatches a sandboxed Claude Code container.

In the terminal you'll see phase-boundary announcements as the
pipeline progresses (implement → post-implement gate → review →
review-fix → end-pipeline gate → finalise). The same lines, plus the
full log stream, also land in the log file at the path configured in
`[logging].path` (default `bellows.log` in the working directory).
`tail -f bellows.log` from a second terminal is the recommended way to
watch a long-running run unattended.

Bellows is foreground by design in v1 — close the terminal and the
loop stops. The standard pattern is to run it in `tmux` / `screen` /
`nohup` if you want it to persist while the lid is closed.

### `bellows status` — is bellows busy or idle?

```bash
bellows status
```

`status` prints one line describing what the polling loop is doing
right now: not running, idle, or busy on issue #N (with the agent
phase and elapsed wall-clock time). It reads a small status file the
running orchestrator keeps fresh — no IPC needed. Useful from a
second terminal when you don't want to grep `bellows.log` to figure
out whether bellows is alive.

### `bellows kill <repo>/<issue>` — abort an in-flight run

```bash
bellows kill marad2001/my-repo/42
# or, with a single-repo orchestrator.toml:
bellows kill 42
```

`kill` stops an in-flight run on a specific issue: SIGKILLs the
container, pushes whatever the agent had committed up to that point,
opens a draft PR with the logs as a `<details>` comment, and
transitions the issue's label to `agent-cancelled`. Use it when you
realise an issue brief was wrong, or when an agent has clearly stuck
itself and burning the rest of the wall-clock budget won't help.

The `<owner>/<name>/<issue>` form is the explicit shape (issue #35
multi-repo polling). The bare `<issue>` form continues to work when
exactly one `[[repo]]` is configured; with multiple repos it refuses
with a clear error rather than guessing which repo you meant. The
container lookup filters on `bellows-repo=<owner>/<name>` AND
`bellows-issue-number=<N>`, so repo A's `#42` and repo B's `#42` are
never confused.

### `bellows refresh-auth` — re-seed expired OAuth tokens

```bash
bellows refresh-auth
```

When the Claude Code OAuth refresh token in the credentials volume
expires (or the volume gets clobbered), agent runs start failing with
401-shaped auth errors in stderr. `refresh-auth` opens the same
interactive Claude Code container as `setup-auth` so you can `/login`
again and re-seed the volume. The volume name is the one in
`[auth].credentials_volume`. See
[Troubleshooting](#troubleshooting) for the signature to watch for.

## Issue lifecycle

Bellows is the final stage of a four-step pipeline. End-to-end, a
single issue moves like this:

1. **File the issue** on the target GitHub repo. Apply `needs-triage`
   (or just leave it unlabelled — your triage workflow's choice).
2. **Triage** the issue manually or via the triage skill. The
   terminal state is `ready-for-agent`, which requires:
   - one category label (`bug` or `enhancement`);
   - an **agent brief** as the latest comment on the issue, with
     concrete testable acceptance criteria. The brief is what the
     in-container agent will treat as its contract; weak briefs
     produce weak runs. The triage skill's `AGENT-BRIEF.md` template
     is the canonical shape — see
     [`docs/agents/triage-labels.md`](docs/agents/triage-labels.md)
     and [`docs/agents/issue-tracker.md`](docs/agents/issue-tracker.md)
     for the conventions this repo's triage uses.
3. **Bellows polls** every `interval_seconds`. On the next tick after
   the label change, it claims the issue (swaps `ready-for-agent` for
   `agent-in-progress`), spawns a sandboxed Claude Code container with
   a kickoff prompt built from the brief, and starts the pipeline.
4. **The agent runs** test-first under the `tdd` skill (see
   [`policy-image/skills/`](policy-image/skills/)). The orchestrator
   gates on `cargo clippy` and `cargo test`, runs an automated
   code-review phase, applies fixes from that review, then a final
   gate.
5. **The PR opens.** Regular PR on success; draft PR on failure,
   with the run's logs attached as a collapsible `<details>` comment.
   The outcome label (`agent-done` / `agent-failed` /
   `agent-rate-limited` / `agent-cancelled`) is applied to the
   **issue**, not the PR — bellows-opened PRs are unlabeled.
6. **The PR merges.** A bellows-authored Success PR auto-squash-merges
   to the default branch once `ci.yml` passes — see the auto-merge
   note below. Draft PRs (failure / rate-limited / cancelled runs)
   sit open for a human to review and either merge manually or close.
   Squash-on-merge keeps the agent's intermediate commits out of
   `master`'s history.

Bellows-authored Success PRs (non-draft, on `agent/*` branches, targeting
the default branch) auto-merge via the GitHub Actions workflow at
[`.github/workflows/auto-merge.yml`](.github/workflows/auto-merge.yml).
The workflow watches `ci.yml` via a `workflow_run` trigger and, on a
green CI run, calls the squash-merge API directly — closing the
bellows-on-bellows loop so the operator does not have to click "merge"
by hand. This is a **workflow-file feature, not a bellows-binary
feature**: the bellows runtime is unchanged, and an operator can opt
out — delete `auto-merge.yml` (or comment out its `on:` trigger) —
without rebuilding bellows. Draft PRs, PRs whose branch does not start
with `agent/`, PRs targeting a non-default branch, and PRs whose CI
failed are all skipped — so human-authored PRs and bellows failure
PRs still need a human merge.

The `agent/*` branch namespace is **bellows-owned by convention**.
Bellows creates `agent/<N>-<slug>` on every claim, force-overwrites
on retries, and — per [ADR-0003](docs/adr/0003-pre-claim-stale-agent-branch-deletion.md)
— sweeps any stale `agent/<N>-*` ref on origin before claiming an
issue, so a failed prior run's leftover branch can't crash the next
reclaim. Practical consequence: **closing a bellows PR without
`--delete-branch` is treated as "abandon — bellows will reclaim and
overwrite the branch on the next claim."** Closing the PR through
the GitHub UI or `gh pr close` leaves the `agent/*` branch on
origin by default; on the next `ready-for-agent` flip, bellows will
delete that branch and rebuild from scratch. If you want to preserve
the work an agent produced on a closed PR, `git checkout` and rebase
those commits onto a non-`agent/*` branch first — bellows does not
look at non-`agent/*` branches.

Bellows does **not** auto-retry. If a run lands at `agent-failed` and
you want to try again, fix whatever needs fixing (the agent brief, the
test environment, an upstream dep) and re-label the issue back to
`ready-for-agent`. Bellows will pick it up on the next poll.

## Troubleshooting

### Cold-cache build time on first run for a repo

A fresh agent-side workspace has no warm cargo registry and no
`target/` directory the very first time bellows runs against a given
repo, so that first `cargo build` recompiles every dependency from
scratch — easily 15–25 minutes for a non-trivial project, which can
eat most of the default 60-minute wall-clock budget before the agent
has done anything useful.

Bellows mitigates this by mounting two persistent Docker named volumes
on every agent container:

- a per-repo `target/` cache at the workspace's `target/` directory,
  named `bellows-target-<owner-repo>` (the suffix is the slugified
  `owner/repo` segment of `[repo].url`);
- a host-wide shared cargo registry cache at `/usr/local/cargo/registry`,
  named `bellows-cargo-registry`.

Both volumes are created lazily by Docker on first mount — no manual
setup needed — and tagged with `bellows-managed=true` plus a
`bellows-volume-kind=target|cargo-registry` label so future tooling
(`bellows prune`, planned in issue #13) can find them. The second
and every subsequent run against the same repo reuses these caches:
the registry index is not re-fetched and dependency crates are not
recompiled.

**First-run workaround.** The very first run on a new repo still pays
the cold-cache cost — the volumes get *populated* during that run.
If you'd rather pay this cost outside the agent's wall-clock budget,
run `cargo build` once on a fresh host clone of the target repo
before kicking off the first bellows run. The agent's clone is still
fresh inside the sandbox, so this is a partial mitigation; subsequent
runs are warm regardless of pre-warm.

If you're seeing repeated wall-clock-cap failures on a new repo and
you haven't pre-warmed, that's almost certainly why — see the
wall-clock entry below for bumping the cap as a stopgap.

### Expired Claude Code refresh token

**Symptom:** agent runs start failing very fast (well under the
wall-clock cap), and the run's `<details>` log comment contains
`401`-shaped errors, `authentication` strings, or other auth-failure
signatures in the agent's stderr. The Claude Code OAuth refresh token
in the credentials volume has expired or been invalidated.

**Remedy:** run `bellows refresh-auth` to re-seed the volume. This
opens the same interactive container as `setup-auth`; `/login` again
in the browser, `/exit` when done, and the next bellows run will pick
up the new tokens.

Auto-detection of this failure signature is tracked in
[issue #10](https://github.com/marad2001/bellows/issues/10) (slice 12)
— once it lands, bellows will surface the remedy directly in the log
instead of leaving you to grep stderr.

### Orphan containers from a crashed bellows process

If bellows crashes or is killed mid-run, the agent container it spawned
is left behind by Docker (it has nothing else to clean it up). Bellows
handles this automatically: at startup, the `run` command lists
bellows-tagged containers and removes any that aren't associated with
the current process. No manual intervention needed; you'll see a
`cleaned up N orphan containers` line in `bellows.log` if the cleanup
fired.

The orphan-container cleanup does **not** auto-reclaim GitHub issues
that were stuck at `agent-in-progress` from the killed run. See the
next item.

### Issue stuck at `agent-in-progress`

If you see an issue stuck at `agent-in-progress` and no container is
actually running, one of two things happened:

- bellows crashed mid-run (the next startup cleaned up the orphan
  container but cannot infer the operator's intent for the labels), or
- you ran `bellows kill <issue>` (in which case the label should have
  already moved to `agent-cancelled`).

Either way, the remedy is the same: manually swap the label back to
`ready-for-agent`. Bellows will pick the issue up again on the next
poll. Bellows does not auto-retry because a flapping orchestrator
would otherwise blow through your wall-clock budget on the same
broken issue.

### Wall-clock cap firing too aggressively

If runs are consistently failing at `agent-failed` with `(killed by
deadline)` in stderr and you've ruled out a stuck agent, bump
`[agent].wall_clock_minutes` in `orchestrator.toml`. The default of 60
is calibrated for warm-cache runs against a not-too-large Rust repo;
bigger codebases or cold-cache scenarios may legitimately need more.

Be wary of pushing this much past 90 — long-running agents tend to
drift, and the wall-clock cap exists partly to detect that drift.

## TOS posture

Bellows authenticates the in-container agent to Anthropic using a
**personal Claude Code subscription** (the OAuth session you seeded
into the credentials volume during `setup-auth`), invoked via Claude
Code's officially supported **headless mode** (`claude -p ...`).
Concurrency is **hard-coded to 1**: bellows runs at most one agent
container at any moment, queueing other `ready-for-agent` issues
serially. This matches what a single human operator working AFK could
do interactively, and avoids parallelism patterns that would be
inconsistent with personal-subscription terms as currently understood.

This is the right call for v1, but it does sit on a single
TOS-interpretation assumption. If Anthropic tightens its stance on
headless / orchestrated subscription usage, the fallback is to switch
to API-key auth and pay per-token. The plumbing is already in place:
`Auth` in [`src/auth.rs`](src/auth.rs) is an enum with `Subscription`
and `Auth::ApiKey` variants, and only `Subscription` is implemented
today (the `Auth::ApiKey` arms `todo!()`). Switching, when needed,
would mean filling in those arms and flipping the config — no
architectural change.

## v1 scope

The smallest end-to-end thing that works. Every item in the "not in
v1" list is additive on the current architecture; future slices will
chip away at them.

### In v1

- multi-repo polling (one `[repo]` table OR a `[[repo]]`
  array-of-tables with one entry per repo) — every poll tick walks
  all configured repos and claims the oldest `ready-for-agent` issue
  across the combined set. Concurrency stays at 1 regardless of how
  many repos are configured;
- single in-flight issue at any moment (concurrency=1, hard-coded);
- subscription auth via Anthropic personal subscription, OAuth in a
  Docker volume;
- headless Claude Code agent (`claude -p ...`) inside a Docker
  sandbox built from a baked policy image;
- `cargo clippy` + `cargo test` gate after the agent reports done;
- a GitHub Actions CI gate (`.github/workflows/ci.yml`) re-running
  the same `cargo test --all-targets --all-features` and
  `cargo clippy --all-targets --all-features -- -D warnings` checks
  on every PR + every push to `master`, so human-authored commits get
  the same verdict bellows applies to agent commits;
- automated code review + fix loop after the gate passes
  (clippy / test failures land before review even runs);
- always-open-a-PR contract — regular PR on success, draft PR on
  failure with logs as a `<details>` comment;
- GitHub label state machine across triage labels +
  `agent-in-progress` / `agent-done` / `agent-failed` /
  `agent-rate-limited` / `agent-cancelled`.

### Not in v1

- multi-host orchestration / true 24/7 AFK from a VPS;
- parallelism / concurrent issues (concurrency stays at 1);
- `sccache` integration — bellows already mounts per-repo `target/`
  and shared cargo-registry named volumes on every agent container
  (see [Cold-cache build time](#cold-cache-build-time-on-first-run-for-a-repo)
  in Troubleshooting); sccache on top of that is deferred to v1.5;
- Windows or macOS CI runners (the workflow is Linux-only in v1);
- `cargo fmt --check`, code coverage reporting, or release-packaging
  steps in CI (all future additions to the workflow);
- web dashboard / TUI / status UI beyond the `bellows status`
  one-liner;
- push notifications (Discord, Pushover, etc.);
- auto-retry after failure (you re-label `ready-for-agent` to retry);
- GitHub App auth (fine-grained PAT only);
- per-repo policy customisation;
- cost / token tracking (subscription mode doesn't price per-token);
- automated triage (issues #21 and #22 are queued but not built);
- security review phase (issue #20 also queued).

## Further reading

For deeper context, the repo ships these documents alongside the
README. None are required to operate bellows; reach for them when you
want to understand the decisions, not just the keystrokes.

- [`RESEARCH.md`](RESEARCH.md) — the original briefing, with the full
  reasoning behind every architectural decision (brain-vs-plumbing,
  sandbox technology, polling vs webhooks, auth, build cache strategy,
  stop conditions, v1 scope justification). Start here if you want
  the "why".
- [`PRD.md`](PRD.md) — the v1 contract, slice by slice.
- [`CLAUDE.md`](CLAUDE.md) — the project's agent-skills config. Tells
  Claude Code agents working on the bellows codebase itself how this
  repo's domain docs and issue tracker are arranged.
- [`policy-image/CLAUDE.md`](policy-image/CLAUDE.md) — the baseline
  CLAUDE.md baked into the sandbox image. This is what the
  in-container agent sees on every run, before the per-issue kickoff
  prompt; read it if you want to know the agent's standing
  instructions.
- [`docs/agents/`](docs/agents/) — per-skill conventions: the
  [issue tracker](docs/agents/issue-tracker.md) (how skills should use
  `gh`), the [triage labels](docs/agents/triage-labels.md) (canonical
  vs project-specific label strings), and the
  [domain doc layout](docs/agents/domain.md) (where `CONTEXT.md` and
  ADRs live).
- [`orchestrator.example.toml`](orchestrator.example.toml) — the
  canonical config schema with inline commentary for every field.
