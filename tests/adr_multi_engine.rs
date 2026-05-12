//! Integration tests pinning the load-bearing content of
//! `docs/adr/0005-multi-engine-support.md`.
//!
//! ADR-0005 is the contract that the multi-engine feature slices
//! (#81 chain-walking + persisted state, #82 implementation) read
//! before they touch code. The tests below pin each acceptance
//! criterion from issue #80's brief to a checkable assertion against
//! the document's body, so the ADR can evolve in prose without
//! silently losing a load-bearing decision.
//!
//! Shape mirrors `tests/readme.rs`: assert the load-bearing nouns and
//! phrases (chain names, label strings, env-var names, file paths)
//! are present rather than verbatim sentences, so the prose around
//! them can flex without breaking the tests.

use std::fs;
use std::path::PathBuf;

fn adr_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("adr")
        .join("0005-multi-engine-support.md")
}

fn read_adr() -> String {
    fs::read_to_string(adr_path()).unwrap_or_else(|e| {
        panic!(
            "ADR-0005 must exist at {}: {}",
            adr_path().display(),
            e,
        )
    })
}

fn assert_contains_all(body: &str, needles: &[&str], context: &str) {
    let mut missing = Vec::new();
    for needle in needles {
        if !body.contains(needle) {
            missing.push(*needle);
        }
    }
    assert!(
        missing.is_empty(),
        "ADR-0005 is missing {} in section {:?}: {:?}",
        if missing.len() == 1 { "value" } else { "values" },
        context,
        missing,
    );
}

/// AC#1 — `docs/adr/0005-multi-engine-support.md` exists and follows
/// the same shape as ADR-0001/0002/0003/0004.
///
/// "Same shape" in this repo means: a single H1 title sentence
/// stating the decision, a body paragraph, a `## Considered
/// alternatives` section, and a `## Consequences` section. Any
/// regression on those structural sections is caught here.
#[test]
fn adr_0005_exists_and_follows_shape_of_other_adrs() {
    let path = adr_path();
    assert!(
        path.is_file(),
        "ADR-0005 must exist at {} (the multi-engine design is the \
         contract for slices #81/#82)",
        path.display(),
    );
    let body = read_adr();
    assert!(
        !body.trim().is_empty(),
        "ADR-0005 must not be empty",
    );
    // Title line: ADRs in this repo open with an H1 line stating the
    // decision in a single sentence (no `# ADR-0005:` numbering — see
    // ADR-0001..0004 for the convention).
    let first_line = body.lines().next().unwrap_or("");
    assert!(
        first_line.starts_with("# "),
        "ADR-0005 must open with an H1 title line (got: {:?})",
        first_line,
    );
    assert_contains_all(
        &body,
        &[
            "## Considered alternatives",
            "## Consequences",
        ],
        "ADR shape (sections that ADR-0001..0004 also carry)",
    );
}

/// AC#2 — Documents the two stated wins (diversity + throughput) and
/// which design choices are load-bearing for each.
///
/// The brief names diversity and throughput as the wins the design
/// chases; both must be named explicitly. The reviewer-not-grading-
/// its-own-homework framing is the diversity rationale; the
/// per-phase fallback (across implement/review/review-fix/security-
/// review/security-fix) is the throughput rationale.
#[test]
fn adr_0005_documents_diversity_and_throughput_wins() {
    let body = read_adr();
    assert_contains_all(
        &body,
        &[
            // The two named wins, verbatim.
            "Diversity",
            "Throughput",
            // The diversity rationale — reviewer not grading its
            // own homework.
            "reviewer",
            "homework",
            // The throughput rationale — per-phase fallback across
            // every agent-invoking phase, not just implement. The
            // comma-separated co-occurrence pins the list shape as
            // a single needle so the prose has to name all five
            // phases together; the bare phase names independently
            // would pass on unrelated uses elsewhere in the ADR
            // (the config snippet, the rate-limit-split section,
            // the operator-UX section all mention them).
            "implement, review, review-fix, security-review, security-fix",
            // Soft preference + visible collapse phrasing pinned so
            // the prose names the load-bearing trade-off.
            "soft",
            "collapse",
        ],
        "wins / diversity + throughput",
    );
}

/// AC#3 — Documents the engine selection model. Five load-bearing
/// shapes from the brief:
///   1. Per-phase `cli_chain` in config (each agent-invoking phase
///      declares an ordered list of engines).
///   2. Engine choice happens at each phase's start, not once at
///      claim time.
///   3. Soft-diversity two-pass picker: first pass = (hot AND ≠
///      implementer-CLI); second pass = (hot) with a visible
///      collapse warning; empty → terminate run as `RateLimited`.
///   4. Per-issue `engine:claude` / `engine:codex` label = forced
///      single-engine override (no fallback, no diversity logic).
///   5. Both labels present → refuse-to-claim (parallel to
///      `MissingAgentBrief`).
#[test]
fn adr_0005_documents_engine_selection_model() {
    let body = read_adr();
    assert_contains_all(
        &body,
        &[
            // Config shape.
            "cli_chain",
            "phases.implement",
            "phases.review",
            // Phase-start picker, not claim-time picker — both
            // halves of the contrast pinned, so a regression that
            // drops one side fails the test. Bare "phase" / "claim"
            // appear independently many times in the ADR.
            "at the start of each phase",
            "claim time",
            // Soft-diversity two-pass shape. "Hot" is defined in
            // the picker as `cooling_until` being in the past or
            // unset; pinning that definition is stronger than the
            // bare adjective, which appears in unrelated prose.
            "first pass",
            "second pass",
            "in the past or unset",
            "implementer-CLI",
            "RateLimited",
            // Forced-engine label override.
            "engine:claude",
            "engine:codex",
            "forced-single-engine",
            // Refuse-to-claim parallel to MissingAgentBrief.
            "MissingAgentBrief",
            "refuse-to-claim",
        ],
        "engine-selection-model",
    );
}

/// AC#4 — Documents the auth model.
///   - ChatGPT subscription login (parallel to claude
///     `credentials_volume`).
///   - Per-engine credentials volume config:
///     `auth.claude.credentials_volume` AND
///     `auth.codex.credentials_volume`.
///   - Flat `auth.credentials_volume` rewrites to claude for
///     backwards compatibility.
///   - Lazy validation: only the engine about to be dispatched to
///     is required to exist.
#[test]
fn adr_0005_documents_auth_model() {
    let body = read_adr();
    assert_contains_all(
        &body,
        &[
            // ChatGPT subscription login, parallel to claude.
            "ChatGPT",
            "subscription",
            // Per-engine credentials volume nesting.
            "auth.claude.credentials_volume",
            "auth.codex.credentials_volume",
            // Flat backwards-compatibility rewrite.
            "auth.credentials_volume",
            "backwards compatib",
            // Lazy validation, named verbatim.
            "Lazy validation",
            "dispatched",
        ],
        "auth-model",
    );
}

/// AC#5 — Documents the policy-image strategy.
///   - Single image with both CLIs baked and pinned.
///   - Engine choice passed in via `BELLOWS_ENGINE` env var set
///     per-phase at each container start.
///   - `run-agent` and the review/security-review scripts branch on
///     the env var.
#[test]
fn adr_0005_documents_policy_image_strategy() {
    let body = read_adr();
    assert_contains_all(
        &body,
        &[
            // Single image, both CLIs baked + pinned. Pin the
            // load-bearing emphasis phrasing rather than the bare
            // adjectives, which appear in unrelated ADR prose.
            "**single** policy image",
            "both CLIs **baked** and **pinned**",
            // Env-var dispatch — named verbatim.
            "BELLOWS_ENGINE",
            "per-phase",
            // The image's entrypoint scripts branch on the env
            // var. "analogous wrappers" pins the claim that the
            // review/security-review phases each get a wrapper;
            // "branch on" + "to invoke the right CLI" pin the
            // dispatch shape so the prose has to keep the
            // wrappers-branch-on-`BELLOWS_ENGINE` rationale.
            "run-agent",
            "analogous wrappers",
            "branch on",
            "to invoke the right CLI",
        ],
        "policy-image-strategy",
    );
}

/// AC#6 — Documents the operating-context strategy. Engine-aware
/// `policy::render_kickoff`; Codex path inlines the operating-
/// context body + all baked skill bodies into the kickoff prompt
/// itself (no parallel `AGENTS.md` maintained in lockstep).
#[test]
fn adr_0005_documents_operating_context_strategy() {
    let body = read_adr();
    assert_contains_all(
        &body,
        &[
            // The engine-aware renderer is named in code today.
            "policy::render_kickoff",
            "engine-aware",
            // Codex path inlines bodies.
            "Codex",
            "inline",
            "kickoff",
            "skill",
            // The thing we're NOT doing — the rationale for inlining.
            "AGENTS.md",
            "lockstep",
        ],
        "operating-context-strategy",
    );
}

/// AC#7 — Documents the persisted rate-limit state.
///   - State file `bellows-state.json` (or similar) alongside
///     `bellows.log`, recording per-engine `cooling_until`
///     timestamps.
///   - Updated when a phase exits with a rate-limit signature;
///     cooldown parsed from the CLI's stderr signature.
///   - Read at every phase-start before chain walking.
///   - Self-correcting: if cooldown is wrong (CLI lied about reset
///     time), the next call hits a fresh rate-limit, state is
///     updated, chain advances. Single pass per phase prevents
///     thrashing.
#[test]
fn adr_0005_documents_persisted_rate_limit_state() {
    let body = read_adr();
    assert_contains_all(
        &body,
        &[
            // The state file name, the per-engine field name, and
            // the sibling-file relationship to bellows.log.
            "bellows-state.json",
            "cooling_until",
            "bellows.log",
            // When it's updated, when it's read, and how the cooldown
            // is derived from stderr.
            "rate-limit signature",
            "stderr",
            "phase-start",
            "chain walk",
            // Self-correcting + thrash-prevention.
            "self-correcting",
            "thrash",
            "single pass",
        ],
        "persisted-rate-limit-state",
    );
}

/// AC#8 — Documents the rate-limit behaviour split.
///   - Implement phase, workspace at base SHA, mid-execution rate
///     limit → in-place chain advancement (drop workspace, swap to
///     next hot chain entry, re-run from base; max 1 in-place
///     advancement per phase invocation).
///   - All other agent-invoking phases mid-execution rate-limit →
///     terminate run as `RateLimited`. State file updated. Next
///     claim consults state and walks chain afresh.
#[test]
fn adr_0005_documents_rate_limit_behaviour_split() {
    let body = read_adr();
    assert_contains_all(
        &body,
        &[
            // The implement-phase carve-out.
            "in-place chain advancement",
            "base SHA",
            "drop workspace",
            "max 1",
            // All other phases terminate.
            "terminate",
            "Next claim",
            "afresh",
        ],
        "rate-limit-behaviour-split",
    );
}

/// AC#9 — Documents operator UX: `--engine` flag on `setup-auth`
/// and `refresh-auth` (default = first entry of
/// `phases.implement.cli_chain`); auth-error callout in run-log
/// comment names the engine to refresh.
#[test]
fn adr_0005_documents_operator_ux() {
    let body = read_adr();
    assert_contains_all(
        &body,
        &[
            // The flag itself + both subcommands it applies to.
            "--engine",
            "setup-auth",
            "refresh-auth",
            // The default behaviour when --engine is omitted.
            "phases.implement.cli_chain",
            "default",
            // The run-log callout names the engine.
            "run-log",
            "auth-error",
        ],
        "operator-ux",
    );
}

/// AC#10 — Incorporates the empirical findings from #79: Codex
/// stderr signatures, headless invocation flags, volume-mount
/// behaviour. The ADR has to NAME the load-bearing findings
/// explicitly so a later implementer (slice #81/#82) does not need
/// to re-read the spike comment.
#[test]
fn adr_0005_incorporates_spike_79_empirical_findings() {
    let body = read_adr();
    assert_contains_all(
        &body,
        &[
            // Sourcing — the ADR must point readers at the spike.
            "#79",
            // The codex headless-invocation flag set surfaced by
            // the spike.
            "codex exec",
            "--dangerously-bypass-approvals-and-sandbox",
            // The load-bearing stdin-closure finding (without it
            // codex hangs forever).
            "</dev/null",
            // The pinned codex version covered by the spike.
            "rust-v0.130.0",
            // Rate-limit stderr substrings sourced from the spike's
            // source-code reading.
            "quota exceeded",
            "rate limit:",
            // Auth-error substring composite.
            "401 Unauthorized",
            "Missing bearer or basic authentication",
            // Reset-timestamp absence → conservative 5-minute
            // default (load-bearing for #82's state file).
            "5-minute",
            // Volume-mount behaviour confirmed by the spike.
            "$CODEX_HOME",
        ],
        "spike-79-empirical-findings",
    );
}
