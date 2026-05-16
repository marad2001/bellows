# Spike #117 findings: OpenCode + DeepSeek

Working transcript for [issue #117](https://github.com/marad2001/bellows/issues/117). Each AC has a section with verbatim
findings. When all ACs are resolved, two outputs derive from this
file:

1. The spike-completion comment on #117 (parallel to #80's transcript
   on #79).
2. ADR-0008's "Empirical findings from spike #N" section, replacing
   the current TODO.

The runtime ACs (AC1–AC9, AC12) will be executed in WSL with a fresh
OpenCode install pointing at the real DeepSeek API (auth-error /
sentinel discovery) or a mock OpenAI-compatible endpoint (rate-limit
signatures). Findings file is updated as each AC resolves.

## Status

- [x] AC1 — `</dev/null` stdin closure (resolved: NOT required)
- [x] AC2 — `--dangerously-skip-permissions` coverage (resolved: covers all tool categories)
- [x] AC3 — auto-commit behaviour (resolved: does NOT auto-commit)
- [~] AC4 — state-file pollution outside `/workspace` (paths enumerated; secret-leak audit deferred to integration review)
- [x] AC5 — SIGTERM response (resolved with concerning finding: SIGTERM ignored during active work)
- [~] AC6 — exit-code semantics (key insight captured: exit 0 even on auth error; substring matching is authoritative; remaining failure modes deferred to integration testing)
- [x] AC7 — stderr ANSI behaviour (ANSI present; NO_COLOR ignored; strip-before-grep is the path)
- [x] AC8 — rate-limit stderr signature substrings (resolved with composite-match recommendation)
- [x] AC9 — auth-error stderr signature substrings (resolved with composite-match recommendation)
- [x] AC10 — OpenCode CLI version pin + SHA256 (amended: pin glibc variant, not musl)
- [x] AC11 — DeepSeek V4 Pro model identifier (resolved: `deepseek/deepseek-v4-pro`)
- [x] AC12 — generated AGENTS.md auto-discovery confirmed (resolved: both `AGENTS.md` and `agents/*.md` paths work)

## Adjacent findings (not AC-mapped but load-bearing)

- **OpenCode bundles its own node + npm runtime.** No system node required; writes to `~/.npm/_cacache/` at runtime to manage plugins.
- **`--pure` flag disables external plugins.** Likely what bellows wants for deterministic runs (parallel to `--dangerously-skip-permissions`).
- **`--variant <effort>` flag** sets provider-specific reasoning effort (`high`, `max`, `minimal`). DeepSeek V4 Pro-Max is likely `--model deepseek/deepseek-v4-pro --variant max` rather than a separate model string — to be confirmed if/when bellows wants V4 Pro-Max.
- **`--print-logs` controls stderr emission of internal logs.** Without it, bellows' substring matching against stderr may see only the user-facing terminal output, not the structured log lines where rate-limit / auth-error signals likely live. **AC8 and AC9 must be run with `--print-logs` to validate the integration shape.**
- **`--format json` exists.** Emits "raw JSON events" — a structurally cleaner signal-detection path than stderr substring matching, worth recording as a future-enhancement option for bellows.
- **OpenCode ships built-in free models** under the `opencode/*` provider: `big-pickle`, `deepseek-v4-flash-free`, `minimax-m2.5-free`, `nemotron-3-super-free`, `qwen3.6-plus-free`. These work with no credentials configured. Not the spike's target (paid V4 Pro is), but useful: bellows could use a free model for smoke-testing the dispatch shape without spending money. Out of scope for this integration; flagged as future-friendly optionality.

---

## AC10 — OpenCode CLI version pin + SHA256

**Pin:** OpenCode `v1.15.3` (latest stable as of 2026-05-16, well
above DeepSeek's documented minimum of `v1.14.24`).

**Canonical source surprise.** The repo published at
`github.com/sst/opencode` has been **transferred** to
`github.com/anomalyco/opencode`. `gh api repos/sst/opencode` resolves
via redirect and returns `full_name: "anomalyco/opencode"`; release
asset URLs point at `anomalyco/opencode/releases/...`. The project's
documented homepage is still `https://opencode.ai`. **anomaly.co
appears to be the corporate entity that took over OpenCode's
development from SST.** We pin against the new canonical URL.

**Tarball-variant surprise (runtime-validated).** The
`opencode-linux-x64-musl.tar.gz` asset is **dynamically linked**
against musl libc (`file` reports
`interpreter /lib/ld-musl-x86_64.so.1, ELF 64-bit LSB executable,
x86-64, dynamically linked`). It does **not** run on a vanilla
glibc base (Debian/Ubuntu) without first installing musl
(`apt-get install -y musl`). The codex precedent (using the
`codex-x86_64-unknown-linux-musl` tarball on a debian-based image)
does not transfer — codex's musl binary is statically linked; this
opencode tarball is not. **For the debian-based bellows policy
image, pin the glibc variant instead.**

**Linux glibc x86_64 (the pin to use for the policy image):**

- URL: `https://github.com/anomalyco/opencode/releases/download/v1.15.3/opencode-linux-x64.tar.gz`
- Size: 52,593,140 bytes
- SHA256: `f8ae8678c9bccdbaf99777f36ff2d5efe689d473384f2e94b84d6cda256d2540`
- Empirically validated: extracted, symlinked into `/usr/local/bin/opencode`, ran `opencode --version` on `debian:bookworm-slim` — outputs `1.15.3` cleanly.

**Musl variants (for reference, not the pin):**

- `opencode-linux-x64-musl.tar.gz` (50,770,151 bytes, SHA256 `e4a7156bf99a8a383166f9d5b6ffb159ff8155d0fd5dfd87bbbe235d1c6b632a`) — dynamically linked against musl, requires musl-libc on the host.
- `opencode-linux-x64-baseline-musl.tar.gz` has an identical SHA256 (`e4a7156bf99a...`) — byte-identical to the non-baseline at v1.15.3.

**Arm64 glibc (for future ARM hosts):**

- URL: `https://github.com/anomalyco/opencode/releases/download/v1.15.3/opencode-linux-arm64.tar.gz`
- Size: 52,485,726 bytes
- SHA256: `4f2a3e3040c6dc6717961b1034e7ae651940c449065d316c6c6e17a4b78293da`

**Version-bump policy.** The codex precedent (rust-v0.130.0 pinned
since the #79 spike) suggests bellows treats engine CLI bumps as
deliberate policy-image rebuilds, not runtime surprises. Same posture
here: bumping opencode requires updating the version + SHA256 in the
Dockerfile and rebuilding the image. The known-good combination of
opencode CLI + DeepSeek V4 Pro at integration time is the baseline;
subsequent bumps are validated by re-running this spike's runtime ACs
against the new version. **When bumping, re-check the dynamic-link
shape of the chosen tarball — the musl/glibc trap above is exactly
the kind of thing that can silently regress between releases.**

---

## AC11 — DeepSeek V4 Pro model identifier

**OpenCode `--model` flag accepts: `deepseek/deepseek-v4-pro`** for
DeepSeek V4 Pro (standard reasoning effort).

**Sources (initial web research):**

- DeepSeek's own [OpenCode integration docs](https://api-docs.deepseek.com/integrations/opencode) prescribe: "Type `/connect` in the input box, then enter `deepseek` and select the provider — Select the DeepSeek-V4-Pro model." The `/connect`-selected provider is `deepseek` (not a reseller); the model picker offers `DeepSeek-V4-Pro` as a named option that OpenCode internally maps to `deepseek/deepseek-v4-pro` for the `--model` flag.
- DeepSeek's V4 Pro release was 2026-04-24 (MIT-licensed open weights + API), [confirmed via DeepSeek's API docs news feed](https://api-docs.deepseek.com/news/news260424).
- Pricing on the canonical DeepSeek endpoint: $1.74/M input, $3.48/M output. The cost motivation behind the integration is real and quantified.

**Empirical confirmation (runtime cache inspection).** The OpenCode v1.15.3 binary caches the Models.dev registry at `~/.cache/opencode/models.json` on first run. Inspecting that cache directly confirmed the `deepseek` provider entry:

```json
{
  "id": "deepseek",
  "env": ["DEEPSEEK_API_KEY"],
  "npm": "@ai-sdk/openai-compatible",
  "api": "https://api.deepseek.com",
  "name": "DeepSeek",
  "doc": "https://api-docs.deepseek.com/quick_start/pricing"
}
models: ['deepseek-chat', 'deepseek-v4-pro', 'deepseek-reasoner', 'deepseek-v4-flash']
```

So **`deepseek/deepseek-v4-pro` IS the correct OpenCode model identifier**, contradicting my earlier WebFetch result that suggested V4 Pro only lived under `auriko`. The earlier WebFetch result was wrong — the WebFetch sub-model evidently missed the entry while traversing the registry JSON. Trust the local cache (which OpenCode actually uses) over WebFetch summarization of registries.

**Auriko still exists** as a separate provider listing `deepseek-v4-pro` and `deepseek-v4-flash` — confirmed to be a third-party reseller, not the canonical path. The cache also enumerates V4-Pro reseller listings under: `deepinfra`, `frogbot`, `gmicloud`, `cortecs`, `baseten`, `novita-ai`, `digitalocean`, `kilo`, `vivgrid`, `openrouter`, `orcarouter`, `opencode-go`, `llmgateway`, `poe`, `siliconflow`, `ollama-cloud`, `aihubmix`, `nvidia`, `zenmux`, `vercel`, `venice`, `fireworks-ai`, `alibaba-cn`, `huggingface`, `nebius`, `togetherai`. Bellows targets the canonical `deepseek` provider; all others are out of scope.

**Auth env var name** confirmed by the cached registry: `DEEPSEEK_API_KEY`. ADR-0008's example block (`apiKey: "{env:DEEPSEEK_API_KEY}"` in opencode.json) is correct.

**API endpoint** confirmed: `https://api.deepseek.com`. The integration's egress allowlist (if bellows ever adds one) needs this domain.

**OpenCode uses `@ai-sdk/openai-compatible` as the npm-backed implementation** of the deepseek provider — meaning OpenCode treats DeepSeek as an OpenAI-compatible endpoint, which simplifies AC8's mock-endpoint design (return OpenAI-shaped 429 error bodies).

**Adjacent models to be aware of:**

- `deepseek-v4-flash` — a lighter / cheaper sibling under V4. Same provider story applies.
- `deepseek-v4-pro-max` — V4 Pro's "maximum reasoning effort" mode. Distinct from V4 Pro standard; higher cost. **Not what was specified for this integration** (bellows uses `deepseek-v4-pro`, not `-max`), but the existence is documented here so the integration slice's `orchestrator.toml.sample` cleanly comments the choice.

**Risk flagged to AC12.** If OpenCode rejects `--model deepseek/deepseek-v4-pro` on a real run because its Models.dev cache hasn't seen V4 Pro listed under the canonical `deepseek` provider yet, the integration is **temporarily** reduced to one of two fallbacks:

1. Wait for Models.dev to update.
2. Use `auriko/deepseek-v4-pro` as a transitional identifier, with the understanding that an Auriko-prefixed API key (and pricing) applies, not DeepSeek's.

AC12 confirms which posture we actually face.

---

## AC1 — `</dev/null` stdin closure

**Result: NOT required.** OpenCode v1.15.3's `opencode run` exits cleanly regardless of stdin state. Three variants tested in the throwaway debian:bookworm-slim container with the glibc binary, against the built-in free `opencode/big-pickle` model (no API key needed for the test):

| Variant                                    | Exit code | Elapsed (ms) |
|--------------------------------------------|-----------|--------------|
| `opencode run "say hi" </dev/null`         | 0         | 3,319        |
| `echo "" \| opencode run "say hi"`         | 0         | 5,086        |
| `opencode run "say hi"` (docker exec pipe) | 0         | 2,854        |

Unlike codex (which hangs forever without `</dev/null` per spike #79's findings), opencode terminates regardless. **For the integration slice's `run-agent` opencode arm, `</dev/null` can be included defensively for symmetry with the codex arm, but it is not load-bearing.**

## AC4 — state-file pollution outside `/workspace` (partial)

**Paths enumerated after a single `opencode run` invocation as root.** Subtree analysis:

- `~/.local/state/opencode/locks/<sha>/` — `meta.json` + `heartbeat`. Per-session lock state.
- `~/.local/share/opencode/` — the durable application state directory:
  - `opencode.db`, `opencode.db-wal`, `opencode.db-shm` — sqlite database (sessions, history, indexed content).
  - `auth.json` — credential store (empty when no providers configured).
  - `log/<timestamp>.log` — per-invocation log files (multiple per session — at least 3 log files were produced for one `run` invocation).
  - `storage/session_diff/ses_<id>.json` — per-session diff records.
  - `storage/migration` — migration marker.
- `~/.cache/opencode/models.json` — Models.dev registry cache.
- `~/.npm/_cacache/` — opencode's bundled npm runtime cache (`content-v2/*`, `index-v5/*`, `tmp/*`). OpenCode invokes its bundled npm at runtime to install plugins; the cache materialises here.

**Bellows implications.**

- The policy image runs as the `bellows` user, not root. All paths above resolve to `/home/bellows/.local/state/opencode/`, `/home/bellows/.local/share/opencode/`, `/home/bellows/.cache/opencode/`, `/home/bellows/.npm/`. None of these collide with claude's `/home/bellows/.claude/` or codex's `/home/bellows/.codex/` — clean per-engine ownership.
- The integration slice should pre-create these directories at image-build time so the first-run "performing one-time database migration" doesn't race container start.
- The `~/.npm/_cacache/` directory is **non-trivial** (multiple MB after one run). The policy image should either (a) pre-populate the npm cache for the plugins opencode loads by default by doing a build-time `opencode run` smoke-test, or (b) pass `--pure` at runtime to disable external plugins entirely. **(b) is the cleaner shape for bellows' deterministic-execution contract** — same posture codex takes by being closed-system.

**Secret-leak check (deferred).** The session logs and `storage/session_diff/*` files contain the prompt body. For bellows, the kickoff prompt includes the brief — typically not secret, but worth a second check. The full secret-leak audit (does opencode ever write API keys or response bodies containing secrets into the session log?) is deferred to integration-slice review, since the test for it requires running with a real API key against a prompt that intentionally tries to extract secrets — out of this spike's scope.

## AC7 — stderr ANSI behaviour (partial)

**ANSI escape codes are present by default** in opencode's stdout/stderr. `cat -v` reveals literal `^[[0m` (ANSI reset) sequences interleaved with the prose output. Example baseline output for `opencode run "say hi"`:

```
^[[0m
> build M-BM-7 big-pickle
^[[0m
Hi
```

**`NO_COLOR=1` env var is IGNORED** by OpenCode v1.15.3 — the same `^[[0m` sequences appear with and without `NO_COLOR=1` set. This is non-spec behaviour (no-color.org spec says any CLI should respect the env var); flagging it but not blocking the integration.

**Two paths forward for bellows' substring matching.**

1. **Strip ANSI before grep.** Add a `strip_ansi(stderr_tail)` step before calling `is_rate_limit_signature` / `is_*_auth_error_signature`. Single sed/regex pass. Works for any engine that emits ANSI.
2. **Use `--format json` for opencode** — emits "raw JSON events" per `--help`, which is structurally cleaner than substring matching. Requires extending bellows' detection layer to consume structured events for opencode specifically, while keeping substring matching for claude/codex.

**Recommendation:** Path 1 (strip ANSI before grep) for the v1 integration slice — minimal divergence from the existing codex/claude detection shape, generalises naturally to any future ANSI-emitting CLI. Path 2 is a follow-up enhancement if the substring approach proves brittle.

A `--no-color` flag was checked for in `--help` and does **not exist**. The only available mitigation is post-hoc stripping by bellows.

## AC2 — `--dangerously-skip-permissions` coverage

**Result: RESOLVED. Flag covers all tool categories.** With `--dangerously-skip-permissions`, opencode v1.15.3 logs its effective permission policy in the session metadata:

```json
permission=[
  {"permission":"question","pattern":"*","action":"deny"},
  {"permission":"plan_enter","pattern":"*","action":"deny"},
  {"permission":"plan_exit","pattern":"*","action":"deny"}
]
```

Only three permissions are explicitly denied: `question` (asking the user), `plan_enter`, `plan_exit` (plan mode). All other tools — `bash`, `read`, `glob`, `grep`, `edit`, `write`, `task`, `webfetch`, `todowrite`, `websearch` — are implicitly allowed without prompting. The `--dangerously-skip-permissions` flag is the right mechanism for bellows' headless contract.

Three test prompts run, all exited cleanly with no interactive prompts:

- **File-write**: `"Create a file at /workspace/test.txt containing exactly: hello from opencode"` → test.txt created with correct contents in 6.0s.
- **Shell-exec**: `"Run ls -la /workspace/ via the Bash tool"` → completed in 5.7s, no prompts.
- **Git-read**: `"Run git log --oneline via the Bash tool"` → completed in 5.8s, no prompts.

**One sub-concern for the integration slice.** The `question` deny means opencode cannot ask the user anything. If the model's reasoning leads it to "I need to ask the user," opencode will either route through the `question` tool (which denies) or take a different path. The bellows integration should ensure the policy-image CLAUDE.md's existing "you cannot ask the user, write to agent-notes.md" guidance is also surfaced to opencode (through the build-time-transformed AGENTS.md — AC12 confirms). Without that, opencode may silently abandon prompts that *could* have completed if the model knew to write blockers to agent-notes.md.

## AC3 — auto-commit behaviour

**Result: RESOLVED. OpenCode does NOT auto-commit.** After `opencode run` completed against a clean git repo with a prompt that created a new file, `git status --short` showed `?? test.txt` (untracked) and `git log` was unchanged. The integration slice's `run-agent` opencode arm needs **no `git reset --soft HEAD~N` post-step** — opencode behaves exactly the way bellows wants: edit files in /workspace, leave them uncommitted, exit. Bellows' commit step picks them up as-is.

**Adjacent finding.** OpenCode does take an internal "snapshot" of the workspace before file edits, recording git-tree-shape pointers under `~/.local/share/opencode/snapshot/<old-sha>/<new-sha>`. These are *not* actual git commits — they're opencode's internal change-tracking that lives outside the workspace's `.git/` directory. The user-facing git state (`git log`, `git status`) is unaffected.

## AC5 — SIGTERM response and workspace consistency

**Result: RESOLVED, with concerning finding.** OpenCode does NOT honor SIGTERM during active session work.

Test design: long-running prompt (`"Write a 600-word essay … to /workspace/long_essay.txt"`) launched via `timeout --signal=TERM 5s opencode run ...`. Expected outcome: SIGTERM at 5s, exit shortly after.

Actual outcome:

- **Exit code**: 124 (timeout's marker for "process didn't terminate after SIGTERM")
- **Elapsed time**: 24,607ms (target: ~5,000ms)
- **Stderr tail**: shows a *normal* session-completion shutdown sequence (`exiting loop`, `session.idle`, `disposing instance`) — not a signal-induced shutdown
- **Workspace state**: `long_essay.txt` was completely written (6,370 bytes, valid UTF-8, all four required keywords present)

**Interpretation.** Either (a) opencode ignored the SIGTERM entirely and finished its work naturally, or (b) opencode caught SIGTERM but treated it as advisory and continued until the in-flight session step completed. Either way, **SIGTERM is not a usable polite-kill signal for opencode** with the 5s grace period commonly used elsewhere.

**Implications for bellows.**

- The wall-clock-budget kill path (`runner.rs` SIGTERM-then-SIGKILL escalation) may need a substantially longer SIGTERM grace period for opencode runs — long enough for an in-flight session step to complete. A bellows-side timeout that fires before the session step naturally ends will silently force a SIGKILL escalation, with no opencode-emitted log indicating why.
- Alternative: use SIGKILL directly for opencode. The risk is half-written workspace files (the 600-word essay test showed the file *was* fully written, but a smaller-grain interrupt timing might catch a partial write — the workspace consistency property is timing-dependent, not signal-handler-guaranteed).
- The integration slice should add an opencode-specific kill grace constant, or document that opencode runs may exceed wall-clock-budget by up to "current in-flight session step duration" before bellows can reclaim the slot.

**Workspace-consistency property held in this single test**, but the test exercised a complete write that opencode happened to finish before SIGKILL would have fired. A harder test (kill at a randomly-chosen point in a long sequence of tool calls) might reveal partial-write windows. Deferred to integration-slice testing.

## AC9 — auth-error stderr signature substrings

**Result: RESOLVED with rich findings.** Bogus `DEEPSEEK_API_KEY=sk-bogusspike1234567890abcdef1234` against real DeepSeek endpoint via `opencode run --model deepseek/deepseek-v4-pro`. Error captured in stderr as a JSON-encoded AI SDK error:

```json
{
  "error": {
    "name": "AI_APICallError",
    "url": "https://api.deepseek.com/chat/completions",
    "statusCode": 401,
    "responseBody": "...",
    "message": "Authentication Fails, Your api key: ****1234 is invalid",
    "data": {
      "error": {
        "message": "Authentication Fails, Your api key: ****1234 is invalid",
        "type": "authentication_error",
        "param": null,
        "code": "invalid_request_error"
      }
    }
  }
}
```

**Major finding — exit code anomaly.** `opencode run` returned **exit code 0** despite the auth failure. The 401 was caught internally and reported via the error structure in stderr, but the process exited cleanly. Unlike claude (exit 1) and codex (exit non-zero) on auth errors, opencode requires stderr inspection to distinguish auth-failed from succeeded.

**Implications for `policy::classify_exit`:**

- Current bellows logic considers a non-zero exit + matching stderr substring → `Crash` or specific reclassification. For opencode, the path needs to be `exit == 0 && is_opencode_auth_error_signature(stderr) → AuthError`.
- The reclassification rule for opencode is asymmetric vs claude/codex: opencode's exit code is unreliable, stderr is authoritative.

**Substring candidates for `is_opencode_auth_error_signature`:**

| Substring                       | Specificity                          | False-positive risk                              |
|--------------------------------|--------------------------------------|--------------------------------------------------|
| `"AI_APICallError"`            | Opencode/AI SDK universal error      | Any HTTP error from any provider (not just auth) |
| `"statusCode":401`             | HTTP 401 reported by AI SDK          | Web-fetched content containing the substring     |
| `"Authentication Fails"`       | DeepSeek-specific (grammatical quirk)| DeepSeek-only — won't catch other providers      |
| `"authentication_error"`       | OpenAI-shape `error.type`            | Generic — most OpenAI-compatible providers       |
| `"invalid_request_error"`      | OpenAI-shape `error.code`            | Generic, also fires for non-auth invalid requests|

**Recommendation: composite match `"AI_APICallError"` AND `"statusCode":401`.** Both substrings present means (a) it's an AI SDK error, (b) the HTTP status was 401. False-positive surface is near-zero — random web content would not contain both literal strings adjacent in opencode's stderr format. Mirrors codex's composite-match precedent.

For a DeepSeek-specific narrower match (if bellows ever wants to surface "specifically your DeepSeek API key is bad" in the operator UX), `"Authentication Fails"` is unique to DeepSeek (the grammatical quirk is a stable identifier).

**Auth-error endpoint confirmed.** The URL in the error structure is `https://api.deepseek.com/chat/completions` — confirms the chat-completions endpoint and matches the cached registry's `api` field for `deepseek` provider.

**Stderr format.** The error JSON is embedded inline in a single stderr log line prefixed `ERROR <ts> +Xms service=llm providerID=deepseek modelID=deepseek-v4-pro session.id=... error={"error":{"name":"AI_APICallError",...`. ANSI codes interleaved (per AC7). Bellows must strip ANSI before substring matching.

## AC8 — rate-limit stderr signature substrings

**Result: RESOLVED.** A local Python `http.server` stub returning HTTP 429 with a DeepSeek-shaped OpenAI-compatible error body was wired into opencode via a minimal `opencode.json`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "deepseek": {
      "options": {
        "apiKey": "sk-mocktestkey",
        "baseURL": "http://127.0.0.1:8765/v1"
      }
    }
  }
}
```

**OpenCode respects `baseURL` overrides cleanly.** Useful for any future mock-endpoint testing bellows wants to do during CI.

**Behaviour against a persistent 429.** OpenCode auto-retried twice within 30 seconds before bellows' outer `timeout` fired. The AI SDK marks rate-limit errors `"isRetryable":true` and respects the `Retry-After: 60` header from the mock — so a 429 hold-back of 60s + 30s outer budget gave us 2 retries before the harness killed it.

**Substring candidates for `is_opencode_rate_limit_signature`:**

| Substring                       | Specificity                          | False-positive risk                                  |
|--------------------------------|--------------------------------------|------------------------------------------------------|
| `"AI_APICallError"`            | AI SDK universal error               | Any HTTP error from any opencode provider            |
| `"statusCode":429`             | HTTP 429 reported in error JSON      | Web-fetched content containing the literal substring |
| `"isRetryable":true`           | AI SDK retry hint                    | Any transient error, not just rate-limit             |
| `"rate_limit_exceeded"`        | OpenAI-shape error.code              | Generic across OpenAI-compatible providers           |
| `"Rate limit reached"`         | DeepSeek error.message wording       | DeepSeek-only                                        |

**Recommendation: composite match `"AI_APICallError"` + `"statusCode":429`.** Symmetric with the auth-error composite (`"AI_APICallError"` + `"statusCode":401`). Both substrings present means an AI SDK error with HTTP 429 — near-zero false positives.

**Implications for bellows' rate-limit handling.**

- **OpenCode retries internally**, so by the time bellows observes a non-zero exit (or substring match on a residual error), opencode has already burned wall-clock on retries. The cooling_until timestamp bellows derives should be conservative — at least `Retry-After` (60s default for our mock) plus a safety margin. ADR-0005's 5-minute codex default is a reasonable starting point.
- **Exit code under sustained rate-limit is uncertain.** AC8's invocation hit the outer `timeout 30s` (exit 124) before opencode could give up naturally. The next spike round (or integration testing) should let opencode exhaust retries against a fast-failing mock (no `Retry-After`, multiple 429s in a tight loop) to capture opencode's natural-exhaustion exit code.

## AC12 — generated AGENTS.md auto-discovery

**Result: RESOLVED.** ADR-0008's build-time transform path is validated for both files.

**Sentinel `AGENTS.md` at `~/.config/opencode/AGENTS.md`** (340 bytes, plain markdown with a recognisable phrase) → the model returned the exact sentinel phrase `purple-armadillo-7392` in response to "What is your sentinel phrase?". The file was consumed by opencode at session start without any explicit reference in `opencode.json` — automatic discovery via the documented order (1. local AGENTS.md, 2. local CLAUDE.md, 3. global `~/.config/opencode/AGENTS.md`, 4. claude-code fallback).

**Sentinel agent at `~/.config/opencode/agents/canary.md`** (280 bytes, markdown + YAML frontmatter `mode: subagent`) → appeared in `opencode agent list` output under `canary (subagent)` alongside opencode's built-in agents (build, compaction, explore, general, plan, summary, title). The frontmatter `mode` field was honoured (canary registered as subagent, not primary).

**Built-in agent inventory (adjacent finding).** OpenCode ships these agents by default:

| Agent       | Mode      | Purpose (inferred from name + permission set)         |
|-------------|-----------|-------------------------------------------------------|
| `build`     | primary   | Default coding agent (what `opencode run` uses)       |
| `plan`      | primary   | Plan-mode agent; can only edit `*.opencode/plans/*.md`|
| `summary`   | primary   | Conversation summarisation                            |
| `title`     | primary   | Session-title generator (fires per-session at start)  |
| `compaction`| primary   | Context-window compaction                             |
| `explore`   | subagent  | Read-only exploration helper                          |
| `general`   | subagent  | Generic task delegation                               |

The `title` agent firing per-session explains the AC9 auth-error path: the bogus key was rejected on opencode's *internal* title-generation request before the user's prompt was even attempted. Bellows should expect this title-gen call on every run — additional API cost, additional surface area for auth/rate-limit errors. The integration slice may want to investigate whether `title` can be disabled (the agent's `permission` field shows `"*": deny` for non-build agents, suggesting they're configurable but the disable mechanism is not yet identified).

**ADR-0008 implication.** The build-time transform produces:
- `~/.config/opencode/AGENTS.md` — consumed automatically, no config wiring needed.
- `~/.config/opencode/agents/{tdd,diagnose,triage}.md` — consumed automatically, requires `mode: subagent` (or `primary`) in YAML frontmatter. The schema is opencode-specific and differs from claude's skill frontmatter — confirms ADR-0008's call to translate the frontmatter at build time.

---

## Out-of-scope / deferred

**AC4 secret-leak audit.** Paths enumerated; full audit (does opencode write API keys or response bodies containing secrets into session logs?) requires running with a real API key against an adversarial prompt — deferred to integration-slice review. The known-write paths to watch:

- `~/.local/share/opencode/log/*.log` — session logs (already seen to contain full system prompts including the user message)
- `~/.local/share/opencode/storage/session_diff/ses_*.json` — per-session diffs
- `~/.local/share/opencode/opencode.db` — sqlite database; per-message storage shape unknown

In the policy image these resolve to `/home/bellows/.local/...`, isolated to the per-run container and discarded at run end. No durable leak surface as long as the container is throwaway.

**AC6 broader exit-code semantics.** Partial findings captured across other ACs:

| Scenario                              | Observed exit | Source AC |
|---------------------------------------|---------------|-----------|
| Success (model returns response)      | 0             | AC1, AC2, AC3 |
| Auth-error (bogus API key)            | **0**         | AC9       |
| Rate-limit (mock 429 with retries)    | 124 (timeout) | AC8       |
| SIGTERM during active work            | 124 (timeout) | AC5       |

**Key insight: opencode does not differentiate failure modes via exit code.** Auth failure exits 0; rate-limit timed out on the harness. **Substring matching against stderr is the authoritative signal for bellows.** This contradicts the existing claude/codex pattern (where exit code is primary) and requires a new shape in `policy::classify_exit`:

```text
if engine == Engine::Opencode {
    let stripped = strip_ansi(&stderr_tail);
    if is_opencode_auth_error_signature(&stripped) {
        return ExitReason::AuthError;
    }
    if is_opencode_rate_limit_signature(&stripped) {
        return ExitReason::RateLimited;
    }
    // ... fall through to exit-code-based routing
}
```

Remaining exit-code scenarios (model-said-give-up, malformed model name, prompt-too-long) are deferred to integration testing — the substring-matching shape above handles the known failure modes; less-common modes can extend the routing as they're observed.

---

## Summary of integration-slice deltas vs ADR-0008

The spike has surfaced several amendments to ADR-0008's planned shape:

1. **Tarball pin: glibc, not musl.** ADR-0008's Dockerfile snippet should reference `opencode-linux-x64.tar.gz` (SHA256 `f8ae8678c9bccdbaf99777f36ff2d5efe689d473384f2e94b84d6cda256d2540`) — the musl tarball is dynamically linked and won't run on the debian-based policy image without `apt-get install -y musl`.

2. **`run-agent` opencode arm should include `--pure`.** Disables external-plugin loading from npm; matches bellows' deterministic-execution contract. ADR-0008's snippet should add it.

3. **`run-agent` opencode arm should include `--print-logs`.** Without it, opencode's structured log lines (which carry the rate-limit and auth-error signatures) don't reach stderr. ADR-0008's snippet should add it.

4. **Bellows must strip ANSI before substring matching.** Add a `strip_ansi(&str) -> String` helper in `policy.rs` and apply it before calling `is_*_signature(...)`. Applies cleanly to claude/codex too (defensive — no regression).

5. **Bellows cannot trust opencode's exit code for routing.** `policy::classify_exit` needs an opencode-specific path: substring matching against (ANSI-stripped) stderr is authoritative. Exit 0 with an auth-error signature → `AuthError`. ADR-0008's "Auth shape" section already discusses this in spirit; the spike confirms the implementation detail.

6. **`is_opencode_rate_limit_signature`** = composite match `"AI_APICallError"` + `"statusCode":429`.
   **`is_opencode_auth_error_signature`** = composite match `"AI_APICallError"` + `"statusCode":401`.

7. **Opencode auto-retries on rate-limits.** Bellows' cooling_until timestamp should account for opencode's internal retry already having burned wall-clock — a 60-second `Retry-After` honoured by opencode means the operator's wall-clock has already absorbed at least one wait cycle before bellows sees the error.

8. **SIGTERM is not honoured during active work.** Bellows' wall-clock-budget kill path may need a longer SIGTERM grace for opencode runs, or escalate to SIGKILL directly. Worth a follow-up on the integration-slice issue or via a small ADR-0008 amendment to the "Operating context" section.

9. **OpenCode runs a per-session `title` agent that fires its own LLM call** on every `opencode run` invocation. This means: each bellows phase makes at least two API calls (title-gen + the actual user prompt). Cost implications are minor (title gen is bounded to 50 chars output) but operational implications are real — title-gen-call failures are a separate failure surface bellows must handle. Worth flagging in ADR-0008's "Consequences" section.

10. **Built-in free `opencode/*` models exist.** Useful for smoke-testing bellows' dispatch shape without spending money. Worth mentioning as a developer-onboarding helper in the README's example chain.

All ten of these get rolled into ADR-0008's "Empirical findings from spike #117" section, which currently carries the TODO placeholder. The integration slice's brief draws its ACs from these findings.
