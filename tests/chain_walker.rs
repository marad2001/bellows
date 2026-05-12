//! Chain-walking + persisted rate-limit state for issue #82.
//!
//! Each test pins one acceptance criterion from the issue's brief to a
//! checkable assertion against the `chain_walker` module. Failing-test
//! commits land first; the make-it-pass commits follow.
//!
//! Unit-test isolation per the fixture note: the implementer-CLI is
//! injected directly into the picker as a known input rather than wired
//! through the full runner pipeline (which would need docker).

use bellows::chain_walker::{EngineState, StateFile};
use bellows::config::Engine;
use chrono::{DateTime, Duration, Utc};

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
