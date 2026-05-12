//! Chain-walking + persisted rate-limit state for issue #82.
//!
//! Each test pins one acceptance criterion from the issue's brief to a
//! checkable assertion against the `chain_walker` module. Failing-test
//! commits land first; the make-it-pass commits follow.
//!
//! Unit-test isolation per the fixture note: the implementer-CLI is
//! injected directly into the picker as a known input rather than wired
//! through the full runner pipeline (which would need docker).

use bellows::chain_walker::{
    pick_engine, EngineState, PickError, PickReason, StateFile,
};
use bellows::config::{ChainEntry, Engine};
use chrono::{DateTime, Duration, Utc};

fn entry(engine: Engine) -> ChainEntry {
    ChainEntry { engine, model: None }
}

// -----------------------------------------------------------------
// AC: `bellows-state.json` created on first rate-limit; updated on
// every subsequent rate-limit (per engine); read at every phase-start.
// -----------------------------------------------------------------

#[test]
fn state_file_load_returns_empty_for_missing_path() {
    // First claim of a fresh bellows install does not have a state
    // file yet. A phase-start read must return an empty state (every
    // engine hot) rather than erroring.
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("bellows-state.json");
    let state = StateFile::load(&path).expect("missing file → empty state");
    assert!(state.engines.is_empty(), "missing file must yield empty state");
}

#[test]
fn state_file_save_then_load_round_trips_per_engine_cooling_until() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("bellows-state.json");
    let cooling_until = DateTime::parse_from_rfc3339("2026-05-12T17:42:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut state = StateFile::default();
    state.record_rate_limit(Engine::Claude, cooling_until);
    state.save(&path).expect("save must succeed");
    let loaded = StateFile::load(&path).expect("load must succeed");
    let claude_state = loaded
        .engines
        .get("claude")
        .expect("claude entry must round-trip");
    assert_eq!(
        claude_state.cooling_until,
        Some(cooling_until),
        "cooling_until must round-trip exactly",
    );
}

#[test]
fn state_file_is_hot_treats_past_cooling_until_as_hot() {
    // ADR-0005: "an engine whose `cooling_until` is in the past or
    // `null` is hot."
    let now = DateTime::parse_from_rfc3339("2026-05-12T18:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let past = now - Duration::minutes(5);
    let mut state = StateFile::default();
    state.record_rate_limit(Engine::Claude, past);
    assert!(state.is_hot(Engine::Claude, now), "past cooling_until = hot");
}

#[test]
fn state_file_is_hot_treats_future_cooling_until_as_cold() {
    let now = DateTime::parse_from_rfc3339("2026-05-12T18:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let future = now + Duration::minutes(5);
    let mut state = StateFile::default();
    state.record_rate_limit(Engine::Claude, future);
    assert!(
        !state.is_hot(Engine::Claude, now),
        "future cooling_until = cold (skipped by picker)",
    );
}

#[test]
fn state_file_is_hot_returns_true_for_engine_not_in_file() {
    // The state file only carries engines that have rate-limited in
    // recent history; an absent engine is hot by default.
    let now = Utc::now();
    let state = StateFile::default();
    assert!(state.is_hot(Engine::Claude, now));
    assert!(state.is_hot(Engine::Codex, now));
}

#[test]
fn state_file_save_overwrites_prior_cooling_until() {
    // ADR-0005: "first-write creates it, subsequent writes overwrite."
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("bellows-state.json");
    let early = DateTime::parse_from_rfc3339("2026-05-12T17:42:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let later = DateTime::parse_from_rfc3339("2026-05-12T18:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut state = StateFile::default();
    state.record_rate_limit(Engine::Claude, early);
    state.save(&path).unwrap();

    let mut state = StateFile::load(&path).unwrap();
    state.record_rate_limit(Engine::Claude, later);
    state.save(&path).unwrap();

    let loaded = StateFile::load(&path).unwrap();
    assert_eq!(
        loaded.engines.get("claude").and_then(|s| s.cooling_until),
        Some(later),
        "the later cooldown must overwrite the earlier one",
    );
}

#[test]
fn state_file_default_engine_state_is_hot_unset() {
    let s = EngineState::default();
    assert_eq!(s.cooling_until, None);
}

// -----------------------------------------------------------------
// AC: `cooling_until` parsed from each engine's rate-limit stderr
// when the signature carries a parseable timestamp; otherwise falls
// back to a conservative 5-minute default and the fallback is noted
// in the run-log.
// -----------------------------------------------------------------

#[test]
fn parse_cooling_until_extracts_claude_unix_epoch_marker() {
    // Claude Code stderr commonly carries a unix-epoch reset marker
    // (`Claude AI usage limit reached|<epoch>`). The parser picks it
    // up and returns the absolute timestamp.
    let now = DateTime::parse_from_rfc3339("2026-05-12T18:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    // Epoch 1778702400 = 2026-05-13T01:00:00Z (well past `now`).
    let stderr = "Claude AI usage limit reached|1778702400\n";
    let parsed = bellows::chain_walker::parse_cooling_until(Engine::Claude, stderr, now);
    let expected = DateTime::<Utc>::from_timestamp(1778702400, 0).unwrap();
    assert_eq!(
        parsed.cooling_until, expected,
        "claude unix-epoch marker must parse: {stderr:?}",
    );
    assert!(
        !parsed.used_fallback,
        "parsed timestamp must NOT be marked as fallback",
    );
}

#[test]
fn parse_cooling_until_extracts_claude_rfc3339_reset_at() {
    // Alternative claude phrasing: a literal RFC3339 timestamp in the
    // rate-limit message. The parser handles either shape so future
    // claude-cli rephrasings don't silently drop into the fallback.
    let now = DateTime::parse_from_rfc3339("2026-05-12T18:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let stderr = "Error: rate_limit_error — resets at 2026-05-12T20:30:00Z\n";
    let parsed = bellows::chain_walker::parse_cooling_until(Engine::Claude, stderr, now);
    let expected = DateTime::parse_from_rfc3339("2026-05-12T20:30:00Z")
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(parsed.cooling_until, expected);
    assert!(!parsed.used_fallback);
}

#[test]
fn parse_cooling_until_falls_back_to_5_minutes_for_codex_quota_exceeded() {
    // Issue #79 spike findings: codex stderr does NOT include a
    // parseable reset-at timestamp (reset times come from HTTP
    // headers, not from default-text stderr). The parser falls back
    // to a 5-minute default and marks the result as a fallback so
    // the runner can note it in the run-log.
    let now = DateTime::parse_from_rfc3339("2026-05-12T18:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let stderr = "codex: error: quota exceeded for this billing window\n";
    let parsed = bellows::chain_walker::parse_cooling_until(Engine::Codex, stderr, now);
    assert_eq!(
        parsed.cooling_until,
        now + Duration::minutes(5),
        "codex without a timestamp must fall back to now + 5 minutes",
    );
    assert!(
        parsed.used_fallback,
        "fallback must be flagged so the runner can note it in the log",
    );
}

#[test]
fn parse_cooling_until_falls_back_when_claude_signature_has_no_timestamp() {
    // Defensive: a claude rate-limit stderr that does NOT include a
    // parseable timestamp (older or rephrased message) falls back to
    // the same 5-minute default. The fallback flag is set so the
    // operator can see the conservative cooldown in the run-log.
    let now = DateTime::parse_from_rfc3339("2026-05-12T18:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let stderr = r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#;
    let parsed = bellows::chain_walker::parse_cooling_until(Engine::Claude, stderr, now);
    assert_eq!(parsed.cooling_until, now + Duration::minutes(5));
    assert!(parsed.used_fallback);
}

// -----------------------------------------------------------------
// AC: Two-pass soft-diversity picker. Pass-1 picks hot AND ≠
// implementer-CLI; pass-2 picks hot with an operator-visible
// collapse warning; no hot entry → `RateLimited`.
//
// Unit tests (a/b/c/d) from the brief; the implementer-CLI is
// injected directly into the picker as a known input per the
// "fixture note for tests (a)/(b)/(c)".
// -----------------------------------------------------------------

#[test]
fn pick_engine_a_diversity_preference_picks_codex_when_implementer_was_claude() {
    // AC (a): Implementer = claude; review chain = [codex, claude];
    // both hot → diversity preference picks codex (the non-claude
    // chain entry, which happens to be chain[0]).
    let now = Utc::now();
    let state = StateFile::default(); // both hot
    let chain = vec![entry(Engine::Codex), entry(Engine::Claude)];
    let picked = pick_engine(&chain, &state, Some(Engine::Claude), now)
        .expect("pick must succeed");
    assert_eq!(picked.entry.engine, Engine::Codex);
}

#[test]
fn pick_engine_b_collapse_second_pass_when_diversity_alt_is_cooling() {
    // AC (b): Implementer = claude; review chain = [codex, claude];
    // codex cooling, claude hot → diversity collapses, second pass
    // picks claude with operator-visible warning.
    let now = DateTime::parse_from_rfc3339("2026-05-12T18:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut state = StateFile::default();
    state.record_rate_limit(Engine::Codex, now + Duration::minutes(10));
    let chain = vec![entry(Engine::Codex), entry(Engine::Claude)];
    let picked = pick_engine(&chain, &state, Some(Engine::Claude), now)
        .expect("second-pass pick must succeed");
    assert_eq!(picked.entry.engine, Engine::Claude);
    assert_eq!(
        picked.reason,
        PickReason::SecondPassAfterCollapse,
        "second pass must be reported so the operator sees the collapse warning",
    );
}

#[test]
fn pick_engine_c_diversity_preference_picks_claude_when_implementer_was_codex() {
    // AC (c): Implementer = codex; review chain = [codex, claude];
    // both hot → diversity preference picks claude (skips codex
    // because it matches the implementer-CLI).
    let now = Utc::now();
    let state = StateFile::default();
    let chain = vec![entry(Engine::Codex), entry(Engine::Claude)];
    let picked = pick_engine(&chain, &state, Some(Engine::Codex), now)
        .expect("pick must succeed");
    assert_eq!(picked.entry.engine, Engine::Claude);
    assert_eq!(
        picked.reason,
        PickReason::DiversityPreferred,
        "the pick must be reported as diversity-preferred so the operator can audit",
    );
}

#[test]
fn pick_engine_d_terminates_when_every_chain_entry_is_cooling() {
    // AC (d): Both engines cooling per state file at phase-start →
    // terminate run as RateLimited.
    let now = DateTime::parse_from_rfc3339("2026-05-12T18:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut state = StateFile::default();
    state.record_rate_limit(Engine::Claude, now + Duration::minutes(10));
    state.record_rate_limit(Engine::Codex, now + Duration::minutes(10));
    let chain = vec![entry(Engine::Codex), entry(Engine::Claude)];
    let result = pick_engine(&chain, &state, Some(Engine::Claude), now);
    assert!(
        matches!(result, Err(PickError::AllCooling)),
        "all-cooling must terminate as RateLimited; got {result:?}",
    );
}

#[test]
fn pick_engine_no_implementer_uses_first_hot_entry() {
    // Implement phase has no implementer-CLI yet (the implementer-CLI
    // is set at implement-phase end). The picker falls through to
    // "first hot entry" — chain-order driven, no diversity constraint.
    let now = Utc::now();
    let state = StateFile::default();
    let chain = vec![entry(Engine::Claude), entry(Engine::Codex)];
    let picked = pick_engine(&chain, &state, None, now).expect("pick must succeed");
    assert_eq!(picked.entry.engine, Engine::Claude);
    assert_eq!(picked.reason, PickReason::ChainFirstHotEntry);
}

#[test]
fn pick_engine_no_implementer_skips_cooling_first_entry() {
    // Implement phase with no implementer set yet, chain = [claude,
    // codex], claude cooling: picker skips to codex (the first HOT
    // entry — pure chain walking).
    let now = DateTime::parse_from_rfc3339("2026-05-12T18:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut state = StateFile::default();
    state.record_rate_limit(Engine::Claude, now + Duration::minutes(10));
    let chain = vec![entry(Engine::Claude), entry(Engine::Codex)];
    let picked = pick_engine(&chain, &state, None, now).expect("pick must succeed");
    assert_eq!(picked.entry.engine, Engine::Codex);
    assert_eq!(picked.reason, PickReason::ChainFirstHotEntry);
}

// -----------------------------------------------------------------
// AC: Implement-phase rate-limit splits in two branches —
//   - At base SHA (no commits beyond base) with no prior in-place
//     advance → in-place chain advancement (drop workspace, swap
//     engine, re-run).
//   - Ahead of base SHA, OR a prior in-place advance already used
//     in this phase invocation → terminate as RateLimited.
//
// Tests (e), (e2), (f) from the brief pin all three sub-cases against
// one pure function so the runner-level decision is testable without
// docker.
// -----------------------------------------------------------------

use bellows::chain_walker::{decide_implement_rate_limit_action, ImplementRateLimitAction};

#[test]
fn implement_e_at_base_sha_with_no_prior_advance_advances_in_place() {
    // AC (e): implement rate-limits with workspace at base SHA →
    // in-place chain advancement.
    let action = decide_implement_rate_limit_action(
        /* at_base_sha = */ true,
        /* advances_used = */ 0,
    );
    assert_eq!(action, ImplementRateLimitAction::InPlaceAdvance);
}

#[test]
fn implement_e2_max_one_inplace_advance_terminates_on_second_rate_limit() {
    // AC (e2): max-1 in-place advance per phase invocation. The
    // advanced engine ALSO rate-limits before committing →
    // terminate as RateLimited rather than walking deeper into the
    // chain in-place. ADR-0005: preserves the single-pass-per-phase
    // invariant.
    let action = decide_implement_rate_limit_action(
        /* at_base_sha = */ true,
        /* advances_used = */ 1,
    );
    assert_eq!(action, ImplementRateLimitAction::Terminate);
}

#[test]
fn implement_f_workspace_ahead_of_base_terminates_without_advance() {
    // AC (f): workspace ahead of base SHA → terminate as RateLimited
    // (invariant guard fires; no in-place advance regardless of
    // advances_used). The agent committed work the runner does not
    // want to drop.
    let action = decide_implement_rate_limit_action(
        /* at_base_sha = */ false,
        /* advances_used = */ 0,
    );
    assert_eq!(action, ImplementRateLimitAction::Terminate);
    let action_with_advance_used = decide_implement_rate_limit_action(
        /* at_base_sha = */ false,
        /* advances_used = */ 1,
    );
    assert_eq!(action_with_advance_used, ImplementRateLimitAction::Terminate);
}

// -----------------------------------------------------------------
// AC (g): All other agent-invoking phases (review, review-fix,
// security-review, security-fix) mid-execution rate-limit →
// terminate as RateLimited.
// -----------------------------------------------------------------

use bellows::chain_walker::{
    decide_non_implement_rate_limit_action, NonImplementRateLimitAction,
};

#[test]
fn non_implement_rate_limit_terminates_for_review_phase() {
    assert_eq!(
        decide_non_implement_rate_limit_action("review"),
        NonImplementRateLimitAction::Terminate,
    );
}

#[test]
fn non_implement_rate_limit_terminates_for_review_fix() {
    assert_eq!(
        decide_non_implement_rate_limit_action("review-fix"),
        NonImplementRateLimitAction::Terminate,
    );
}

#[test]
fn non_implement_rate_limit_terminates_for_security_review() {
    assert_eq!(
        decide_non_implement_rate_limit_action("security-review"),
        NonImplementRateLimitAction::Terminate,
    );
}

#[test]
fn non_implement_rate_limit_terminates_for_security_fix() {
    assert_eq!(
        decide_non_implement_rate_limit_action("security-fix"),
        NonImplementRateLimitAction::Terminate,
    );
}
