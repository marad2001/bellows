//! Chain-walking + persisted rate-limit state (issue #82 / ADR-0005).
//!
//! This module ships the chain-walking surface that the per-phase
//! `cli_chain` from #81 plugs into. It owns:
//!
//! - `StateFile` / `EngineState` — the persisted per-engine
//!   `cooling_until` snapshot that `bellows-state.json` carries.
//! - `pick_engine` / `pick_engine_for_phase` — the two-pass soft-
//!   diversity picker plus the forced-single-engine bypass.
//! - `parse_cooling_until` — derive a `cooling_until` timestamp from a
//!   rate-limit stderr signature, falling back to a conservative
//!   5-minute default when the signature carries no parseable
//!   timestamp (codex per ADR-0005 / spike #79).
//! - `decide_implement_rate_limit_action` /
//!   `decide_non_implement_rate_limit_action` — pure decision shapes
//!   the runner consults on rate-limit signature match.
//!
//! Every surface here is keyed off `Engine::as_name()` so a future
//! third engine fits as data, not code shape.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::config::Engine;

/// Conservative cooldown when the rate-limit stderr does not carry a
/// parseable reset-at timestamp. ADR-0005: "the codex match substrings
/// (`quota exceeded`, `rate limit:`) trigger a conservative 5-minute
/// default cooldown" because codex stderr does not include a
/// parseable reset-at (issue #79 spike findings). The same default
/// applies to a claude rate-limit stderr whose phrasing changes and
/// no longer matches the parseable shapes.
const COOLING_UNTIL_FALLBACK_MINUTES: i64 = 5;

/// Persisted per-engine rate-limit state. Written alongside
/// `bellows.log` in the operator's bellows working directory; read at
/// every phase-start; rewritten wholesale on each update (ADR-0005:
/// "the file is small enough to rewrite wholesale; first-write
/// creates it, subsequent writes overwrite").
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateFile {
    /// One entry per engine that has rate-limited in recent history.
    /// Absent engines are hot — there is no "implicit cold" state.
    #[serde(default)]
    pub engines: BTreeMap<String, EngineState>,
}

/// One engine's rate-limit state. Currently a single field; kept as
/// its own struct so a future per-engine field (e.g. a fallback flag
/// for the run-log) can land without re-flattening the on-disk shape.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineState {
    /// Absolute RFC3339 timestamp at which this engine becomes hot
    /// again. `None` means hot (the engine has no recent rate-limit).
    /// `Some(t)` with `t` in the past is also hot (cooldown elapsed).
    #[serde(default)]
    pub cooling_until: Option<DateTime<Utc>>,
}

impl StateFile {
    /// Read the state file from disk. A missing file produces an
    /// empty state (the cold-start path on a fresh bellows install).
    /// Other IO errors propagate so an operator who clobbered the
    /// file's permissions sees a real error rather than silently
    /// resetting.
    pub fn load(path: &Path) -> Result<Self, std::io::Error> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => return Err(e),
        };
        let parsed: StateFile = serde_json::from_str(&raw).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("bellows-state.json: {e}"),
            )
        })?;
        Ok(parsed)
    }

    /// Rewrite the state file in full. ADR-0005: "the structure is
    /// small enough that the entire file is rewritten on each update,
    /// no schema migration story needed yet." A pretty-printed JSON
    /// body keeps the file human-readable when an operator manually
    /// inspects or zeros a `cooling_until`.
    pub fn save(&self, path: &Path) -> Result<(), std::io::Error> {
        let serialized = serde_json::to_string_pretty(self).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("bellows-state.json: {e}"))
        })?;
        std::fs::write(path, serialized)
    }

    /// Whether the given engine is hot at `now`. ADR-0005: "an engine
    /// whose `cooling_until` is in the past or `null` is hot."
    pub fn is_hot(&self, engine: Engine, now: DateTime<Utc>) -> bool {
        match self.engines.get(engine.as_name()) {
            None => true,
            Some(state) => match state.cooling_until {
                None => true,
                Some(t) => t <= now,
            },
        }
    }

    /// Record a fresh rate-limit for the given engine, overwriting
    /// any prior `cooling_until`. The runner calls this at phase-exit
    /// when the captured stderr matches a known rate-limit signature.
    pub fn record_rate_limit(&mut self, engine: Engine, cooling_until: DateTime<Utc>) {
        self.engines.insert(
            engine.as_name().to_string(),
            EngineState {
                cooling_until: Some(cooling_until),
            },
        );
    }
}

/// Result of parsing a `cooling_until` timestamp from a rate-limit
/// stderr signature. `used_fallback` is `true` when the parser could
/// not find a parseable timestamp and produced the conservative
/// 5-minute default; the runner notes that in the run-log so an
/// operator inspecting `bellows-state.json` can see why a cooldown is
/// suspiciously short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedCooldown {
    pub cooling_until: DateTime<Utc>,
    pub used_fallback: bool,
}

/// Derive a `cooling_until` timestamp from a rate-limit stderr
/// signature. The function tries the parseable shapes claude is known
/// to emit, falling back to `now + 5 minutes` per ADR-0005 when no
/// shape matches (codex's default-text stderr, or a future claude
/// rephrasing).
///
/// Claude shapes tried, in order:
///   1. `<some preamble>|<unix epoch>` — Claude Code's
///      `Claude AI usage limit reached|<epoch>` marker.
///   2. An RFC3339 timestamp embedded anywhere in the stderr —
///      catches `resets at 2026-05-12T20:30:00Z` and any future
///      rephrasing that retains a literal timestamp.
///
/// Codex always falls back (spike #79: reset times come from HTTP
/// headers, not from default-text stderr). The fallback flag lets
/// the runner surface "5-minute default applied" in the log.
pub fn parse_cooling_until(
    engine: Engine,
    stderr: &str,
    now: DateTime<Utc>,
) -> ParsedCooldown {
    if matches!(engine, Engine::Claude) {
        if let Some(t) = extract_unix_epoch_after_pipe(stderr) {
            return ParsedCooldown {
                cooling_until: t,
                used_fallback: false,
            };
        }
        if let Some(t) = extract_rfc3339_in_text(stderr) {
            return ParsedCooldown {
                cooling_until: t,
                used_fallback: false,
            };
        }
    }
    ParsedCooldown {
        cooling_until: now + Duration::minutes(COOLING_UNTIL_FALLBACK_MINUTES),
        used_fallback: true,
    }
}

/// Look for `<preamble>|<unix-epoch-seconds>` in `text` and decode the
/// trailing digit run into a UTC timestamp. Picks the FIRST such pipe-
/// delimited number — claude's marker is the only known shape the
/// stderr carries today, and matching the first one keeps the parser
/// deterministic if a future error body interpolates additional
/// numbers.
fn extract_unix_epoch_after_pipe(text: &str) -> Option<DateTime<Utc>> {
    for line in text.lines() {
        if let Some(pipe_idx) = line.find('|') {
            let tail = &line[pipe_idx + 1..];
            // Take the leading run of ASCII digits.
            let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.len() >= 9
                && let Ok(epoch) = digits.parse::<i64>()
                && let Some(t) = DateTime::<Utc>::from_timestamp(epoch, 0)
            {
                return Some(t);
            }
        }
    }
    None
}

/// Scan `text` for an embedded RFC3339 timestamp (`YYYY-MM-DDTHH:MM:SSZ`
/// or with a timezone offset). Returns the FIRST one found. Loose
/// detection — chrono's parser does the precise validation; we just
/// hunt for a plausible-shaped substring to feed it.
fn extract_rfc3339_in_text(text: &str) -> Option<DateTime<Utc>> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 20 <= bytes.len() {
        // Loose shape probe: 4 digits, '-', 2 digits, '-', 2 digits, 'T'.
        if bytes[i].is_ascii_digit()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
            && bytes[i + 4] == b'-'
            && bytes[i + 7] == b'-'
            && bytes[i + 10] == b'T'
        {
            // Find the end of the timestamp (Z or +/-HH:MM).
            let mut end = i + 19; // YYYY-MM-DDTHH:MM:SS
            if end < bytes.len() {
                match bytes[end] {
                    b'Z' => end += 1,
                    b'+' | b'-' if end + 6 <= bytes.len() => end += 6,
                    b'.' => {
                        // Fractional seconds. Consume digits, then
                        // the timezone designator.
                        end += 1;
                        while end < bytes.len() && bytes[end].is_ascii_digit() {
                            end += 1;
                        }
                        if end < bytes.len() {
                            match bytes[end] {
                                b'Z' => end += 1,
                                b'+' | b'-' if end + 6 <= bytes.len() => end += 6,
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            if let Ok(t) = DateTime::parse_from_rfc3339(&text[i..end]) {
                return Some(t.with_timezone(&Utc));
            }
        }
        i += 1;
    }
    None
}
