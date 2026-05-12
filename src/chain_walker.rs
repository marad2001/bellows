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

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::Engine;

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
