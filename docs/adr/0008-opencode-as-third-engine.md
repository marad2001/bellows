# OpenCode as a third engine: API-key auth and build-time operating-context transform

Anthropic's signalled pricing changes (and the parallel evaluation of
GPT-5.5) push bellows to integrate a third engine whose cost profile
differs materially from claude and codex. The operator has settled on
**OpenCode** as the agent CLI and **DeepSeek V4 Pro** as the model
behind it — a combination DeepSeek's own integration docs publish, so
the integration path is exercised upstream rather than improvised.

ADR-0005 promised that "adding a third engine is data, not code
shape." OpenCode mostly honours that promise — the chain config, the
soft-diversity picker, the cooling-state file, the per-issue
`engine:<name>` label, and the per-phase `BELLOWS_ENGINE` env var all
extend by data, not code. Two surfaces break the promise and are the
subject of this ADR: **auth shape** (OpenCode uses an API key in an
env var, not an OAuth credentials volume), and **operating context**
(OpenCode auto-discovers content from disk like claude, but at
different paths and with a different skill abstraction). The rest of
the integration is mechanical extension of the patterns ADR-0005
already established.

## Engine identity: CLI, not model

`Engine::Opencode` names the **CLI** (`opencode`), not the model
(`deepseek-v4-pro`). The model is pinned per chain entry using the
existing `engine:model` chain syntax in `orchestrator.toml` — e.g.
`"opencode:deepseek/deepseek-v4-pro"` — and passed into the container
as `BELLOWS_MODEL=deepseek/deepseek-v4-pro`. OpenCode's `--model`
flag expects `provider/model` format; the `/` in the model name
survives `EngineChainParseError`'s `split_once(':')` parse because
that split takes the *first* colon only.

Naming the variant after the CLI rather than the model preserves
ADR-0005's "data, not code shape" property along the model axis. A
future swap from `"opencode:deepseek/deepseek-v4-pro"` to
`"opencode:qwen/qwen-3-coder"` is one chain-entry edit; renaming a
variant would be a schema change. CONTEXT.md's glossary records the
**Engine** (CLI) vs **Model** (pin) distinction; this ADR is the
authoritative reason for the distinction.

## Auth model: API key in env var, not credentials volume

OpenCode's interactive `/connect` flow persists credentials inside the
operator's OpenCode session, but the same configuration can be
declared up-front in `opencode.json` using OpenCode's `{env:VAR_NAME}`
substitution syntax. With a pre-baked `opencode.json` that references
`{env:DEEPSEEK_API_KEY}`, the headless `opencode run` invocation
reads the key from the environment and the interactive flow is
skipped entirely. There is no OAuth refresh token, no persisted
session blob, and therefore no state that benefits from a Docker
named volume.

Bellows therefore introduces a **second auth shape** alongside the
existing volume-mounted OAuth pattern:

```toml
# Existing OAuth-engine shape (unchanged):
[auth.claude]
credentials_volume = "bellows-claude-credentials"
[auth.codex]
credentials_volume = "bellows-codex-credentials"

# New API-key-engine shape:
[engine.opencode]
api_key_env_file = "~/.config/bellows/opencode.env"
```

`[engine.opencode]` lives under a top-level `[engine.<name>]` table
deliberately *not* under `[auth.<name>]`, because the
`[auth.<name>]` table's documented purpose is to declare a
credentials *volume*. An `[auth.opencode]` block with no volume
field, or with `credentials_volume = ""`, would lie about what the
table means. Distinct schema for distinct shape.

**API-key file format.** A standard env-file: one `KEY=VALUE` per
line, mode 0600, bellows-owned. An operator running multiple
providers under OpenCode in different phases (e.g.
`"opencode:deepseek/deepseek-v4-pro"` in implement,
`"opencode:qwen/qwen-3-coder"` in review-fix) puts both keys in the
same file:

```
DEEPSEEK_API_KEY=sk-...
QWEN_API_KEY=qwen-...
```

The dispatcher passes the file wholesale to `docker run` via
`--env-file`; OpenCode reads whichever key its current `--model`
selection needs.

**`bellows setup-auth --engine opencode`.** Skips the OAuth-in-a-
container path entirely. On the host, prompts the operator for
provider name and API key (echo-off), then writes or updates the
relevant `<PROVIDER>_API_KEY=<value>` line in
`~/.config/bellows/opencode.env`. Creates the file at mode 0600 if
missing. No verification call against the upstream provider —
lazy-validation per ADR-0005 means the key is exercised on the next
dispatch.

**`bellows refresh-auth --engine opencode`.** Re-prompts and
overwrites the existing line, then prints a one-line note clarifying
that OpenCode uses static API keys rather than OAuth refresh tokens.
The two subcommands collapse into the same code path for this
engine; the rename exists for symmetry with the OAuth engines'
operator vocabulary.

**Lazy validation at dispatch.** When the chain walk picks opencode,
the dispatcher checks `api_key_env_file` exists and is readable. If
not, the run terminates with `MissingApiKey { engine: Opencode }`,
parallel to the existing `MissingCredentialsVolume` shape, and the
run-log callout names opencode + points the operator at
`bellows setup-auth --engine opencode`.

**Run-log auth-error callout** for opencode reads "OpenCode's
upstream provider returned an auth error — re-issue your API key on
the provider's dashboard and re-run
`bellows setup-auth --engine opencode`." The "provider" is
deliberately generic; bellows does not parse provider identity out of
the stderr, because the operator already knows which provider their
chain entry referenced.

## Operating context: build-time transform, not inline at kickoff

OpenCode discovers operating-context content from disk at session
start, mirroring claude's pattern rather than codex's inline-at-
kickoff pattern. Per OpenCode's documented discovery order:

1. Local `AGENTS.md` walking up from cwd
2. Local `CLAUDE.md` walking up from cwd
3. Global `~/.config/opencode/AGENTS.md`
4. Global `~/.claude/CLAUDE.md` (claude-code fallback, unless disabled)

The policy image already bakes `/home/bellows/.claude/CLAUDE.md`
(claude's operating context). If we did nothing, OpenCode would pick
that up via path 4. But the existing file is *claude-flavored* —
opens with "You are Claude Code running headless..." — which is wrong
for an opencode agent. ADR-0005 handles this for codex via
`neutralise_claude_phrasing_for_codex` at kickoff-render time.

OpenCode also has a sub-agent abstraction at
`~/.config/opencode/agents/*.md` (markdown + YAML frontmatter,
`description` / `mode` / `model` fields), invoked by `@mention` or
auto-selected from description. This is structurally similar to
claude's skills (`~/.claude/skills/<name>/SKILL.md`) but neither the
directory layout nor the frontmatter schema is a fallback for the
other.

**Decision: build-time transform.** A Rust binary in the policy
image (`policy-image/bin/bellows-policy-image-gen` or similar) runs
as a `RUN` step in the Dockerfile and produces:

- `~/.config/opencode/AGENTS.md` from `/home/bellows/.claude/CLAUDE.md`
  with `neutralise_claude_phrasing_for_*` applied.
- `~/.config/opencode/agents/{tdd,diagnose,triage}.md` from
  `policy-image/skills/*/SKILL.md` with frontmatter translated from
  claude's skill schema to opencode's agent schema, and the same
  body neutralisation applied.

The neutralisation logic already exists in `src/policy.rs`
(`neutralise_claude_phrasing_for_codex`); the build-time bin reuses
it directly, generalised over engine name. There is one source of
truth — the existing `CLAUDE.md` + baked skills directory — and the
opencode views are *derived* at image build time, not separately
maintained. ADR-0005's rejected alternative — "parallel `AGENTS.md`
*maintained* next to `CLAUDE.md`" — does not apply, because the
opencode files are build-time outputs, not separate inputs.

**Claude-fallback disabled.** The baked `opencode.json` explicitly
disables OpenCode's auto-discovery of `~/.claude/CLAUDE.md`.
Otherwise both the neutralised `AGENTS.md` (path 3) and the
claude-flavored `CLAUDE.md` (path 4) would be valid discovery
targets, and we would have to reason about which one OpenCode chose
on any given session. With the fallback off, opencode reads
*only* the neutralised view; claude continues reading the original
`CLAUDE.md` as it always has.

**Kickoff stays short for opencode.** Unlike codex, opencode does
*not* receive an inlined operating-context body in the kickoff text.
`render_kickoff_for_engine(Engine::Opencode, ...)` returns the
unwrapped body (claude-shape), because OpenCode will auto-discover
context from disk. `wrap_phase_prompt_for_engine` gains an
`Engine::Opencode` arm that matches `Engine::Claude`'s behaviour
(identity). The cost win this preserves is non-trivial — the
inlined-context path adds materially to every kickoff's token count,
and cost is the entire reason DeepSeek is being integrated.

## Headless invocation shape

`run-agent` gains a third `case` arm. Final flag set per spike #117:

```sh
case "$BELLOWS_ENGINE" in
    opencode)
        if [ -n "${BELLOWS_MODEL:-}" ]; then
            exec opencode run --model "$BELLOWS_MODEL" --dangerously-skip-permissions --pure --print-logs "$PROMPT" </dev/null
        else
            exec opencode run --dangerously-skip-permissions --pure --print-logs "$PROMPT" </dev/null
        fi
        ;;
```

Each flag's role:

- **`--dangerously-skip-permissions`** — auto-approves opencode's
  permission prompts. The spike confirmed the effective deny-list under
  this flag covers only `question`, `plan_enter`, `plan_exit` (the
  user-blocking categories that don't make sense headlessly); every
  tool category bellows cares about (bash, write, edit, read, glob,
  grep, webfetch, todowrite, websearch) is allowed without prompting.
- **`--pure`** — disables loading external plugins from npm. OpenCode
  bundles its own node + npm runtime and fetches plugins lazily into
  `~/.npm/_cacache/` at runtime; `--pure` keeps bellows' execution
  deterministic by short-circuiting that fetch entirely. Parallel to
  codex's closed-system posture.
- **`--print-logs`** — emits opencode's structured log lines to stderr.
  Without this flag the rate-limit and auth-error signatures bellows
  matches against do not appear in stderr at all — only the terminal
  output does, which lacks the JSON-encoded error structures. This
  flag is **load-bearing** for bellows' detection of opencode's
  failure modes.
- **`</dev/null`** — defensive stdin closure for symmetry with codex's
  arm. The spike confirmed opencode does **not** require it (unlike
  codex, which hangs forever without it). Kept defensively because
  including it costs nothing and prevents a category of future
  regression.

**Auto-commit suppression is unnecessary.** The spike confirmed
opencode does not auto-commit — `git status` after a prompt that
edits files shows the worktree dirty, no new git log entries. The
`run-agent` arm therefore does **not** need a post-run
`git reset --soft HEAD~N` step.

**Tarball pin is the glibc variant**, not musl. The
`opencode-linux-x64-musl.tar.gz` asset published by OpenCode is
dynamically linked against musl libc (`interpreter
/lib/ld-musl-x86_64.so.1`), not statically linked — running it on
the debian-based policy image requires `apt-get install -y musl`.
The glibc variant (`opencode-linux-x64.tar.gz`) is empirically
confirmed to run cleanly on `debian:bookworm-slim` without any
additional packages. See spike #117 AC10 for the exact URL +
SHA256.

## Default chain composition

The default `cli_chain` for operators who never edit
`orchestrator.toml` stays `["claude"]` — today's v1-compatible
behaviour. Multi-engine remains opt-in. New operators do not
suddenly need a DeepSeek API key to claim anything.

The **documented example** in the README and
`orchestrator.toml.sample` shows the **cost-as-throughput-fallback**
posture:

```toml
[phases.implement]
cli_chain = ["claude", "codex", "opencode:deepseek/deepseek-v4-pro"]

[phases.review]
cli_chain = ["codex", "claude", "opencode:deepseek/deepseek-v4-pro"]
```

OpenCode sits *behind* the existing two engines on every phase, so
the day-one impact of shipping this integration is purely additive:
the existing claude↔codex diversity behaviour is unchanged; opencode
fires only when both primaries are cooling. Cost-sensitive operators
who want OpenCode-primary can flip their chains to lead with
`"opencode:..."`, and the soft-diversity picker degrades visibly
(run-log warning) when forced to pair opencode with itself across
implement and review phases.

## Considered alternatives

- **Build the agent loop ourselves against DeepSeek's API directly**
  (option b from the grilling tree — no third-party CLI). Rejected:
  breaks ADR-0005's load-bearing "headless agent CLI" abstraction.
  The `run-agent` entrypoint, the per-phase `BELLOWS_ENGINE` env
  var, and the engine-aware kickoff renderer all assume a CLI that
  takes a prompt and edits a workspace; replacing that with a custom
  tool-use loop is a much bigger surface than "a third engine."

- **Pick a different agent CLI (Aider, Goose, etc.).** Aider was
  the closest competitor on headless maturity but is not listed in
  DeepSeek's official integration docs; OpenCode is. Picking the
  vendor-published path reduces supply-chain risk meaningfully.

- **Name the engine variant `Engine::Deepseek`** (i.e. name it after
  the model). Rejected: conflates the CLI axis with the model axis
  and forfeits the `engine:model` chain syntax's optionality. A
  future swap of the model under OpenCode would become a schema
  change rather than a config edit, regressing ADR-0005's "data, not
  code shape" promise on the model dimension.

- **Force the API key into a credentials volume** (preserve auth
  schema uniformity). Rejected: the volume's purpose is to persist
  refreshable OAuth state across container starts. A static API key
  needs no persistence beyond the host file the operator writes
  once. Putting a key inside a volume buys uniformity at the cost
  of a misleading config key (`credentials_volume = ` with nothing
  to persist) and a worse operator UX (interactive container
  launch to write one line into a file).

- **Inline operating context at kickoff, codex-style.** Rejected:
  OpenCode auto-discovers content from disk by design; bypassing
  the auto-discovery to inline at kickoff bloats every prompt with
  redundant context, and cost is the integration's entire
  motivation. The build-time transform path keeps OpenCode's
  kickoff as short as claude's.

- **Maintain `AGENTS.md` and the opencode agents directory as
  parallel files in the policy-image tree.** Rejected for the
  reason ADR-0005 already documented: parallel *maintained* files
  create a permanent lockstep-maintenance tax on operating-context
  edits. The build-time transform path is different — the files
  are *generated*, not maintained.

- **Leave OpenCode's claude-fallback enabled** (`AGENTS.md` AND
  `~/.claude/CLAUDE.md` both discoverable). Rejected: two
  discovery paths can both fire and we have to debug whichever
  ordering OpenCode picked. Explicit fallback-disable + neutralised
  view is the only-one-path-fires shape.

- **Add opencode to the default `cli_chain`.** Rejected: forces
  every operator to seed a DeepSeek key before they can claim
  anything. Default stays `["claude"]`; opencode is opt-in.

- **Document cost-first ladder (opencode primary) as the default
  example.** Rejected: the day-one risk of "shipping this
  integration" should not be "and your primary engine is now
  opencode." (ii) cost-as-throughput-fallback is purely additive
  to existing behaviour; (i) cost-first is a follow-up the operator
  flips deliberately when DeepSeek's pricing and quality both
  validate against their use case.

## Consequences

- **Auth shape is no longer uniform.** Two patterns exist:
  volume-mounted OAuth (claude, codex) and host-side API-key env
  file (opencode). Operator UX docs grow a paragraph explaining the
  split; the schema is intentionally different per engine to keep
  each shape honest. Adding a fourth engine with OAuth fits the
  first pattern; with API-key fits the second.

- **Policy image gains a build-time generator.** A Rust bin runs
  during `docker build` to produce `~/.config/opencode/AGENTS.md`
  and the opencode agents directory from the existing CLAUDE.md +
  skills sources. Image builds get slower by the duration of that
  step (negligible for the input sizes involved). Operators who
  rebuild the image rarely (most of them) pay no recurring cost;
  operators iterating on `CLAUDE.md` or skill bodies see the
  regenerated opencode views automatically on the next build.

- **The diversity picker walks 3 chain entries naturally.** No
  code change in `chain_walker.rs` — the existing left-to-right
  two-pass logic produces the right answer for arbitrary N. A test
  confirms that with implementer=`claude` and chain=`[codex,
  opencode]` both hot, the picker picks `codex` (leftmost
  non-implementer hot).

- **Multi-label refusal generalises.** Today's "both `engine:claude`
  and `engine:codex` → refuse" extends to "count > 1 → refuse." The
  error message lists the labels actually found, not a hardcoded
  pair.

- **`bellows-state.json` gains a third map key.** Data-only;
  `engines.opencode.cooling_until` reads/writes through the same
  serde path. A v1 state file without the key deserialises fine
  (treated as hot).

- **`bellows refresh-auth --engine opencode` is identical to
  `setup-auth`.** Documented with a one-line note; the API-key
  shape has no refresh-vs-seed distinction. Future API-key engines
  follow this contract.

- **Cost optionality is now a runtime config flip, not a code
  change.** If Anthropic pricing changes force the operator to
  switch to opencode-primary, the change is one
  `orchestrator.toml` edit (chain order). Conversely, if DeepSeek
  pricing changes the other way, the same edit reverses it. The
  CLI/model split means a third-cost-engine swap (e.g.
  `"opencode:qwen/qwen-3-coder"`) is also config-only.

- **Lazy-validation rule extends naturally.** ADR-0005's "only
  validate the engine about to be dispatched to" applies
  unchanged: missing API-key file fails the same way missing
  credentials volume fails today — at dispatch time, with an
  engine-named error and a `bellows setup-auth --engine <name>`
  pointer.

- **Exit-code routing is opencode-asymmetric.** `policy::classify_exit`
  trusts exit code as the primary signal for claude and codex.
  Opencode breaks that contract — it returns exit 0 on auth errors
  (spike #117 AC9) and the surrounding harness's timeout on
  sustained rate-limits. The classifier gains an opencode-specific
  arm that runs substring matching against ANSI-stripped stderr
  *before* consulting the exit code. Other engines remain
  exit-code-first.

- **Per-run API-call multiplier.** OpenCode runs a `title` agent
  on every `opencode run` invocation that makes its own LLM call
  to generate a session title. Each bellows phase therefore makes
  at least two API calls against the opencode-configured provider.
  Token cost is small (title bounded to ≤50 chars output) but the
  operational consequence is real: title-gen-call failures (auth,
  rate-limit) surface in stderr the same way user-prompt failures
  do, and bellows treats them identically. Worth knowing for cost
  projection.

- **SIGTERM is advisory for opencode.** Bellows' wall-clock-budget
  kill path sends SIGTERM with a short grace period before SIGKILL
  escalation. Opencode's in-flight session step continues past
  SIGTERM until completion; the workspace is consistent at the
  end of that step but unbounded in duration mid-step. The
  integration slice decides whether to lengthen the SIGTERM grace
  for opencode runs specifically, or to escalate directly to
  SIGKILL with accepted risk of partial-write windows during
  long file edits.

## Empirical findings from spike #117

Spike #117 ran OpenCode v1.15.3 in a throwaway `debian:bookworm-slim`
container, exercising real prompts against opencode's built-in
free models for behaviour testing, the real DeepSeek API endpoint
with a bogus key for auth-error capture, and a local Python
http.server stub returning HTTP 429 for rate-limit capture. The
full transcript and AC-by-AC findings live in
`docs/spikes/0117-opencode-deepseek.md`. The load-bearing facts:

- **CLI version pinned: `v1.15.3` (published 2026-05-16).** Repo
  transferred from `sst/opencode` to `anomalyco/opencode`; release
  asset URLs resolve at the new canonical URL.
- **Tarball variant: glibc, not musl.** The `*-musl.tar.gz` asset
  is **dynamically linked** to musl libc and does not run on a
  glibc debian base without `apt-get install -y musl`. Use the
  glibc tarball:
  - URL: `https://github.com/anomalyco/opencode/releases/download/v1.15.3/opencode-linux-x64.tar.gz`
  - SHA256: `f8ae8678c9bccdbaf99777f36ff2d5efe689d473384f2e94b84d6cda256d2540`
- **DeepSeek V4 Pro model identifier: `deepseek/deepseek-v4-pro`.**
  Confirmed empirically via OpenCode's runtime Models.dev cache,
  which lists the canonical `deepseek` provider with models
  `[deepseek-chat, deepseek-v4-pro, deepseek-reasoner, deepseek-v4-flash]`,
  `env: ["DEEPSEEK_API_KEY"]`, `api: "https://api.deepseek.com"`,
  `npm: "@ai-sdk/openai-compatible"`. (An earlier WebFetch
  result suggesting V4 Pro lived only under a third-party `auriko`
  provider was wrong — the WebFetch sub-model missed the
  `deepseek`-provider entry. Trust the local cache over WebFetch
  summarisation of registries.)
- **Auth env var: `DEEPSEEK_API_KEY`.** Confirmed by the registry.
  ADR's `opencode.json` snippet (`apiKey: "{env:DEEPSEEK_API_KEY}"`)
  is correct.
- **API endpoint: `https://api.deepseek.com`.** Confirmed by the
  registry and by the AC9 error structure's `url` field.
- **`</dev/null` is NOT required.** OpenCode exits cleanly regardless
  of stdin state. Codex's hang-on-EOF behaviour does not transfer.
  Kept defensively in `run-agent` for symmetry.
- **`--dangerously-skip-permissions` covers all tool categories.**
  The effective permission policy denies only `question`,
  `plan_enter`, `plan_exit`; every tool (bash, read, write, edit,
  glob, grep, webfetch, todowrite, websearch) runs without prompts.
- **`--pure` is required for deterministic execution.** OpenCode
  bundles its own node + npm runtime and lazily fetches plugins
  into `~/.npm/_cacache/`. `--pure` short-circuits that.
- **`--print-logs` is load-bearing for signature detection.**
  Without it, opencode's structured log lines (which carry the
  rate-limit and auth-error JSON error structures) don't reach
  stderr — only terminal-shaped output does, which omits the
  signatures bellows substring-matches against.
- **OpenCode does not auto-commit.** `git status` after a prompt
  that edits files shows the worktree dirty. No
  `git reset --soft` post-step is needed in the opencode `run-agent`
  arm.
- **Stderr emits ANSI escape sequences by default.** `NO_COLOR=1`
  is ignored. No flag in `--help` disables ANSI. Bellows must
  strip ANSI before substring matching — a single sed/regex pass
  in a new `policy.rs::strip_ansi(&str) -> String` helper, applied
  before every `is_*_signature` call. Defensive against the
  same issue arising for claude or codex in the future.
- **Rate-limit stderr signature: composite match
  `"AI_APICallError"` + `"statusCode":429`.** The mocked HTTP 429
  produced a JSON error structure containing both substrings.
  Near-zero false-positive surface — web content would not
  contain both literal strings adjacent in opencode's stderr.
  Adjacent signatures (less specific) include
  `"rate_limit_exceeded"` (OpenAI-shape `error.code`), `"Rate limit"`
  (DeepSeek-shape `error.message`), and `"isRetryable":true` (AI SDK
  retry hint). The composite is the recommended primary match;
  the others are available for narrower or broader detection if
  bellows wants them.
- **Auth-error stderr signature: composite match
  `"AI_APICallError"` + `"statusCode":401`.** Symmetric with the
  rate-limit composite. Real DeepSeek returned the error structure
  with `"message":"Authentication Fails, Your api key: ****1234 is
  invalid"`, `"type":"authentication_error"`,
  `"code":"invalid_request_error"`. The DeepSeek-specific wording
  `"Authentication Fails"` (grammatical quirk — "Fails" not
  "Failed") is a stable narrower match if bellows wants to surface
  "specifically your DeepSeek key" in the operator UX.
- **OpenCode returns exit code 0 even on auth-error.** Unlike claude
  (exit 1) and codex (non-zero) on auth failures, opencode catches
  the error internally and exits cleanly. **Bellows cannot trust
  opencode's exit code for routing.** `policy::classify_exit` must
  match the auth-error / rate-limit signatures against ANSI-stripped
  stderr regardless of exit code for opencode runs. This is an
  asymmetry vs the existing engines worth a comment in the
  classifier.
- **OpenCode does not honour SIGTERM during active session work.**
  A 5-second SIGTERM grace expired with opencode still working;
  the process finished its in-flight session step naturally
  (~25s total) before exiting. The workspace remained consistent
  in this test (the file write completed cleanly) but the
  consistency is timing-dependent, not signal-handler-guaranteed.
  Bellows' wall-clock-budget kill path may need either (a) a
  longer SIGTERM grace for opencode runs (potentially exceeding
  the current cross-engine constant), or (b) direct SIGKILL with
  accepted risk of mid-write inconsistency. The integration slice
  picks one; the spike captured the constraint without
  prescribing the resolution.
- **OpenCode auto-retries `isRetryable:true` errors internally.**
  Rate-limits (429s) are retried with the `Retry-After` header
  honoured. By the time bellows observes a rate-limit signature,
  opencode has already burned wall-clock on internal retries. The
  cooling_until timestamp bellows derives should be conservative —
  ADR-0005's 5-minute codex default is a reasonable starting point
  for opencode too, since opencode's structured logs do not (at
  the time of the spike) emit a per-engine reset-at the way
  claude's stderr does.
- **OpenCode fires a `title` agent on every `opencode run`
  invocation.** The title agent makes its own LLM call to generate
  a session title — meaning **every bellows phase makes at least
  two API calls** (title-gen + the user prompt). Token cost is
  bounded (title is ≤50 chars) but the operational consequence is
  real: title-gen failures (auth, rate-limit) surface in stderr
  before the user prompt is even attempted. Bellows' substring
  matching catches these the same way as user-prompt failures;
  worth being aware of in cost projections and error-routing
  analysis.
- **Generated `AGENTS.md` + `agents/*.md` auto-discovery validated.**
  Sentinel `AGENTS.md` at `~/.config/opencode/AGENTS.md` was
  consumed by opencode at session start (sentinel phrase echoed in
  response). Sentinel agent at `~/.config/opencode/agents/canary.md`
  appeared in `opencode agent list` as `canary (subagent)` with
  the YAML frontmatter `mode` honoured. The build-time-transform
  path in this ADR is empirically validated for both files.
- **OpenCode ships built-in free models** under the `opencode/*`
  provider: `big-pickle`, `deepseek-v4-flash-free`, `minimax-m2.5-free`,
  `nemotron-3-super-free`, `qwen3.6-plus-free`. Useful for
  smoke-testing bellows' dispatch shape with zero API spend.
  Not part of the integration slice's contract but worth
  mentioning in the README as a developer-onboarding option.

These findings are load-bearing for the integration slice's
`is_rate_limit_signature` / `is_*_auth_error_signature` substring
additions, the `strip_ansi` helper, the policy-image Dockerfile's
opencode CLI pin and tarball-variant choice, the `run-agent`
opencode branch's flag set, the `policy::classify_exit` opencode
arm, and the wall-clock-budget kill-path treatment of opencode runs.
