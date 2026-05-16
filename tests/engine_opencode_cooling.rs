//! AC8 of issue #120: the `bellows-state.json` cooling-state file
//! must round-trip the opencode engine as a peer of claude and codex.
//!
//! Note on commit shape: AC8 is data-shaped, not behaviour-shaped.
//! The `StateFile.engines` map is keyed by `Engine::as_name()` so a
//! new engine name "just works" once the `Engine::Opencode` variant
//! exists (AC1). These tests lock in the contract — opencode round-
//! trips under the literal JSON key `"opencode"`, `is_hot` works for
//! opencode, and `parse_cooling_until` falls back to the conservative
//! 5-minute default for opencode stderr (no parseable reset-at in the
//! opencode JSON-shaped 429 / 401 stderr, per ADR-0008).
//!
//! Because AC1's `Engine::Opencode` introduction already routes these
//! through the engine-agnostic infra, the make-it-pass surface for
//! AC8 *is* AC1's commit; the failing-test commit shape is honoured
//! by the standalone failing commit on this AC's `parse_cooling_until`
//! behaviour (see git log).

use std::path::PathBuf;

use chrono::{DateTime, Duration, Utc};

use bellows::chain_walker::{parse_cooling_until, EngineState, StateFile};
use bellows::config::Engine;

#[test]
fn state_file_round_trips_opencode_cooling_state_under_opencode_key() {
    let now: DateTime<Utc> = DateTime::parse_from_rfc3339("2026-05-12T20:30:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let cooling_until = now + Duration::minutes(5);

    let mut state = StateFile::default();
    state.record_rate_limit(Engine::Opencode, cooling_until);

    let dir = tempfile::tempdir().expect("tempdir");
    let path: PathBuf = dir.path().join("bellows-state.json");
    state.save(&path).expect("save");
    let raw = std::fs::read_to_string(&path).expect("read");
    assert!(
        raw.contains("\"opencode\""),
        "bellows-state.json must serialise the opencode engine under the \
         literal JSON key \"opencode\": {raw}",
    );

    let reloaded = StateFile::load(&path).expect("load");
    let entry = reloaded
        .engines
        .get("opencode")
        .expect("opencode entry must be present after round-trip");
    assert_eq!(
        entry,
        &EngineState {
            cooling_until: Some(cooling_until),
        },
    );
}

#[test]
fn state_file_is_hot_distinguishes_opencode_from_claude_and_codex() {
    let now: DateTime<Utc> = DateTime::parse_from_rfc3339("2026-05-12T20:30:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut state = StateFile::default();
    // Only opencode is cooling — claude and codex stay hot.
    state.record_rate_limit(Engine::Opencode, now + Duration::minutes(5));

    assert!(!state.is_hot(Engine::Opencode, now), "opencode is cooling");
    assert!(state.is_hot(Engine::Claude, now), "claude must stay hot");
    assert!(state.is_hot(Engine::Codex, now), "codex must stay hot");

    // After the cooldown elapses, opencode is hot again.
    let later = now + Duration::minutes(6);
    assert!(state.is_hot(Engine::Opencode, later));
}

#[test]
fn parse_cooling_until_for_opencode_uses_five_minute_fallback() {
    // ADR-0008: opencode's stderr is JSON-shaped (`AI_APICallError` +
    // `statusCode:429`) with no parseable reset-at. The cooldown must
    // therefore always come from the conservative 5-minute fallback.
    let now: DateTime<Utc> = DateTime::parse_from_rfc3339("2026-05-12T20:30:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let stderr = r#"{"name":"AI_APICallError","statusCode":429,"message":"rate limited"}"#;
    let parsed = parse_cooling_until(Engine::Opencode, stderr, now);
    assert!(
        parsed.used_fallback,
        "opencode rate-limit stderr must always trigger the 5-minute fallback",
    );
    assert_eq!(parsed.cooling_until, now + Duration::minutes(5));
}
