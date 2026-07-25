# Bellows's cargo-checks gate mirrors target CI, not its own quality bar

Bellows's cargo-checks gate runs `cargo clippy` and `cargo test` against
the cloned workspace in a sandbox container. Until this ADR, those
commands were hardcoded in bellows (`-D warnings` strict clippy,
`--all-features` test). If the target repo's CI ran a different
posture — for instance the workboard repos run
`-D clippy::correctness -D clippy::suspicious` (deliberately narrowed,
recorded in their own decisions) — bellows blocked work on lints CI
deliberately ignores, with no recourse for the agent doing the work.
We switch bellows's gate to **mirror the target repo's CI**: bellows
parses `.github/workflows/*.yml` at `workspace::prepare` time, extracts
the `cargo clippy` and `cargo test` commands from the workflow named
`CI` (the same name bellows's auto-merge workflow filters on via
`workflow_run`), and runs those commands verbatim. When parsing fails
or no workflow is present, bellows falls back to operator-declared
defaults in `[gates].clippy_flags` / `[gates].test_flags` (default-default
preserves today's strict behaviour). The invariant: "bellows gate
passes ⇒ CI gate passes" by construction — there is one spec, not two.

## Considered alternatives

- **Bellows owns the quality bar (Model A); operator opts into different posture via bellows config.** Rejected: requires the operator to maintain two specs (bellows config + CI workflow) and keep them in sync; drift is inevitable. The operational pain that drove this ADR was exactly Model A's failure mode — bellows blocking new work on pre-existing latent debt that CI deliberately ignores. Putting the spec in CI alone removes the drift class.
- **Run the target's CI workflow_dispatch and wait for the result.** Rejected: breaks bellows's pipeline shape (gate-fail feedback can't loop into review-fix because the failure happens on GitHub, not in bellows's container), adds minutes-per-phase to wall-clock, and pushes the entire feedback loop off the host. Bellows's local gate exists *because* it's fast — sacrificing speed defeats the point.
- **Target-repo-side config file** (e.g. `.bellows/gate.toml` at the repo root). Rejected: creates a new artifact alongside the CI workflow, and if the operator updates one but forgets the other, drift returns. The CI workflow is already the authoritative spec for "will this merge pass"; bellows should read it rather than ask the operator to mirror it manually.
- **Mirror clippy only, leave test on bellows-baked defaults.** Rejected: test feature-flag mismatches produce the same drift class. workboard CI runs `cargo test --features in-memory` while bellows defaults `--all-features` — same class of pain, different surface.
- **Mirror everything cargo-* in CI (clippy, test, fmt, doc, bench, etc.).** Rejected as over-scope for v1. The two load-bearing gates today are clippy and test; the rest are additive and can be added under a future ADR if specific operators demand it.

## Consequences

- **One source of truth** for "will CI pass?" — the workflow file. Bellows config carries only fallback defaults, used when parsing fails.
- **Bellows's gate becomes target-repo-aware.** Different repos legitimately get different gate posture. A repo that deliberately narrowed clippy (like the workboards) is respected. A repo on strict `-D warnings` continues to get strict gating.
- **Workflow-shape brittleness becomes a documented operator concern.** Matrix builds, conditional steps, script-shelling-out commands may not parse cleanly. The fallback path covers it; the run-log states explicitly whether the command was parsed or defaulted.
- **The agent's quality contract shifts** from "satisfy bellows's bar" to "satisfy CI's bar." Cleaner for AFK ownership — operators control CI; bellows respects it; the agent works against whatever the operator decided.
- **`[gates].clippy_flags` / `[gates].test_flags` in `orchestrator.toml`** are the new fallback knobs; default-default preserves today's behaviour so existing operators see no change unless they opt in.
- **Snapshot at workspace::prepare** — bellows reads the workflow once when cloning, caches the extracted commands for the run. Mid-pipeline workflow edits don't change the gate verdict for the in-flight run.

## Amendment (issue #180): the mirror includes CI's build environment

The original decision mirrored the *command* and stopped there. That
turned out to be half a mirror.

`workboard-financial-advice` sets `CARGO_PROFILE_TEST_DEBUG: "0"` on its
`cargo test` and `cargo clippy` steps as a documented linker-OOM guard —
its workspace test binaries link with full debuginfo by default and the
linker has been killed on standard runners. Bellows ran the same command
without that env, so the gate linked with `debuginfo=2`, was OOM-killed
(`ld terminated with signal 9 [Killed]`), and reported `FinalTestsRed` on
code the repo's own CI passes. Two consecutive overnight runs (#46 →
PR #648, #280 → PR #649) were lost to it: both implemented cleanly, both
were gated on a failure that existed only inside bellows.

This is the ADR's own invariant failing — "bellows gate passes ⇒ CI gate
passes" does not hold if the two run the same argv under different
environments. **The unit of mirroring is therefore the command *plus* the
environment it runs under, not the command alone.**

Specifics of the extension:

- **Allowlist, never denylist.** A workflow's `env:` routinely carries
  `${{ secrets.* }}`; forwarding it wholesale would hand the target
  repo's credentials to a sandbox with no need for them. Only names that
  tune the Rust build are eligible (`CARGO_PROFILE_*`, `CARGO_BUILD_*`,
  `RUSTFLAGS`, `RUSTDOCFLAGS`, `RUST_BACKTRACE`, `RUST_MIN_STACK`,
  `CARGO_INCREMENTAL`, `CARGO_TERM_COLOR`,
  `CARGO_NET_GIT_FETCH_WITH_CLI`). Anything bellows has never heard of is
  dropped, so a new secret name cannot leak by default.
- **Values carrying an unresolved `${{ ... }}` expression are dropped.**
  Bellows cannot evaluate GitHub's expression language and must never
  pass the literal text through as if it were a value.
- **Env is per-command, not per-container.** A repo may split clippy and
  test into sibling jobs with different `env:` blocks (FA does), so the
  env travels with its own command. Precedence follows GitHub's:
  workflow → job → step, nearest wins.
- **A fallback command carries no env.** When a command comes from
  `[gates].*_flags` rather than the workflow, it is bellows's posture and
  not CI's; pairing it with CI's environment would mirror half of each
  and could produce a posture neither side ever ran.
- **No policy-image change.** The env is rendered as a POSIX
  `VAR='value' cargo ...` assignment prefix inside the existing
  `BELLOWS_*_CMD` string, which `run-cargo-checks` already hands to
  `sh -c`. Values are single-quoted and any value containing a single
  quote is rejected at parse time, so the composed string cannot break
  out of its quoting.

Consequence: the "workflow-shape brittleness" caveat above now extends to
env blocks — an env value bellows cannot safely mirror is dropped
individually, which is strictly closer to CI than dropping all of them.

## Amendment (issue #186): a dead gate is not a verdict

#180 closed the env half of the mirror. What it could not close is the
*machine*: bellows's gate runs in a container whose memory ceiling is far
below a GitHub runner's, and no amount of mirroring makes those the same
computer.

On FA #314 (PR #650) the mirrored env applied correctly — the gate's rustc
invocation carried `-C strip=debuginfo` — and the gate still failed, because
cargo links several large test binaries concurrently and the kernel SIGKILLed
one. Cargo surfaced exit 101, `gate_failed` saw a non-zero check, and the run
was classified `FinalTestsRed`. The agent's own verification of the same diff
was green: 1314 lib tests passing. Third run in a row (after #46 and #280)
where a passing codebase was reported as red and a human had to intervene.

The distinction this ADR now draws: **a check that ran and reported a
verdict** versus **a check that was killed before reaching one**. Only the
first is evidence about the code. A failing assertion exits non-zero
*normally*; a SIGKILL is a death, and no signal-9 shape can be produced by a
test that merely failed. So:

- `policy::is_oom_kill_signature` / `gate_oom_killed` identify a gate that
  died rather than judged, consulting only checks that actually failed (a
  passing check quoting an OOM string in its output cannot trip it).
- The runner retries such a gate **once** with `CARGO_BUILD_JOBS=1`.
  Serialised linking fits in memory — the #314 agent demonstrated exactly
  this — so the retry usually reaches a real verdict and the run continues
  into review instead of halting. The retry's env is merged *over* the
  CI-mirrored env so a repo that sets its own `CARGO_BUILD_JOBS` cannot
  undo it.
- If the retry also dies, the run still drafts — unreviewed code should not
  auto-merge — but the PR body says plainly that the container ran out of
  memory, that this is not a code failure, and that the repo's own CI is
  authoritative. The ADR-0004 "this is the same failure CI would report"
  line is suppressed, because in this one case it is false.

This keeps the ADR's invariant honest. "Bellows gate passes ⇒ CI gate passes"
was always a claim about *verdicts*. A gate that never produced one has no
claim to make, and must not borrow the authority of one that did.
