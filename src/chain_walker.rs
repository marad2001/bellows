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

use crate::config::{ChainEntry, Engine};

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
        let serialized = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::other(format!("bellows-state.json: {e}")))?;
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

/// Reason a chain entry was picked. Ships in the run-log line so an
/// operator can reconstruct the engine-selection trail from the log
/// alone. Each variant has a stable `as_run_log_phrase()` so log-
/// scraping tooling can match on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickReason {
    /// Pass-1 picked the first chain entry — it was hot, and either
    /// the run has no implementer-CLI yet (implement phase) or
    /// chain[0] already differs from the implementer-CLI.
    ChainFirstHotEntry,
    /// Pass-1 picked a non-first chain entry because earlier hot
    /// entries matched the implementer-CLI. The diversity preference
    /// kicked in.
    DiversityPreferred,
    /// Pass-2 ran because every hot chain entry matched the
    /// implementer-CLI. The picker degraded visibly; the runner
    /// surfaces a collapse warning to the operator.
    SecondPassAfterCollapse,
    /// The forced-single-engine `engine:<name>` label bypassed the
    /// chain walk entirely (ADR-0005). State file consulted only for
    /// the rate-limit termination decision, not for selection.
    ForcedViaLabel,
    /// The implement phase rate-limited at base SHA with no prior
    /// in-place advance; the picker walked the chain afresh and
    /// produced the next hot entry to re-run.
    InPlaceAdvancementAfterRateLimit,
    /// The implement phase was found **Oscillating** at base SHA with
    /// the shared advance allowance unspent and budget above the floor
    /// (issue #164). Same advance, different trigger — kept as its own
    /// reason so the run-log distinguishes "the engine ran out of
    /// quota" from "the engine wedged".
    InPlaceAdvancementAfterOscillation,
}

impl PickReason {
    /// Stable human-readable phrase for the run-log line. ADR-0005
    /// AC: each phase-start line carries phase, engine, model, and
    /// **reason** (one of these five phrases) so an operator can
    /// reconstruct the trail from the run-log alone.
    pub fn as_run_log_phrase(&self) -> &'static str {
        match self {
            PickReason::ChainFirstHotEntry => "chain first hot entry",
            PickReason::DiversityPreferred => "diversity-preferred entry",
            PickReason::SecondPassAfterCollapse => "second-pass after collapse",
            PickReason::ForcedViaLabel => "forced via label",
            PickReason::InPlaceAdvancementAfterRateLimit => {
                "in-place advancement after rate-limit"
            }
            PickReason::InPlaceAdvancementAfterOscillation => {
                "in-place advancement after oscillation"
            }
        }
    }
}

/// A chain entry picked by the two-pass soft-diversity picker, plus
/// the reason it was picked. The runner uses `reason` to render the
/// per-phase run-log line + the collapse-warning callout when
/// `SecondPassAfterCollapse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickedEntry {
    pub entry: ChainEntry,
    pub reason: PickReason,
}

/// Why `pick_engine` could not produce a chain entry. The runner
/// translates this into `ExitReason::RateLimited` and terminates the
/// run (deferred to the next claim, which will re-read the state
/// file and walk the chain afresh).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PickError {
    #[error("every chain entry is cooling per the state file; terminating as RateLimited")]
    AllCooling,
}

/// Two-pass soft-diversity picker. ADR-0005 §"Soft-diversity picker":
///
///  1. **First pass** — pick the first chain entry that is both (a)
///     hot AND (b) ≠ implementer-CLI. Diversity-preferring.
///  2. **Second pass** — if the first pass produces nothing, pick the
///     first chain entry that is just (a) hot. Operator-visible
///     collapse warning fires (see `PickReason::SecondPassAfterCollapse`).
///  3. **No hot entry** — terminate the run as `RateLimited` (the
///     persisted state file + the next claim's chain-walk together
///     make this self-correcting).
///
/// `implementer` is `None` for the implement phase itself (the
/// implementer-CLI is set at implement-phase end and consumed by
/// later phases). With `None`, the picker degrades to pure chain
/// walking — the first hot entry wins, no diversity preference.
pub fn pick_engine(
    chain: &[ChainEntry],
    state: &StateFile,
    implementer: Option<Engine>,
    now: DateTime<Utc>,
) -> Result<PickedEntry, PickError> {
    pick_from_chain(chain, state, implementer, now)
}

/// [`pick_engine`] with one engine struck out of the chain before the
/// walk begins (issue #164).
///
/// An **Oscillation**-triggered **Advance** has no state-file trace to
/// steer the next pick: unlike a rate-limit it records no
/// `cooling_until`, because `CONTEXT.md` is explicit that an advance is
/// "never a verdict on quality" and the engine is not out of quota — it
/// wedged on *this* issue. Without an exclusion the re-pick would see an
/// unchanged `StateFile` and hand the phase straight back to the engine
/// that just oscillated. So the runner names the abandoned engine here
/// instead, for exactly the one retry iteration.
///
/// The exclusion is **engine-level**, matching the rate-limit path's
/// engine-level `cooling_until`: `CONTEXT.md` defines an advance as
/// meaning "no further progress is available from this engine", so a
/// model-pinned sibling entry (`claude:opus` after `claude:sonnet`
/// wedged) is struck out too.
///
/// `excluded` of `None` is exactly [`pick_engine`].
pub fn pick_engine_excluding(
    chain: &[ChainEntry],
    state: &StateFile,
    implementer: Option<Engine>,
    excluded: Option<Engine>,
    now: DateTime<Utc>,
) -> Result<PickedEntry, PickError> {
    let Some(excluded) = excluded else {
        return pick_from_chain(chain, state, implementer, now);
    };
    let retained: Vec<ChainEntry> = chain
        .iter()
        .filter(|e| e.engine != excluded)
        .cloned()
        .collect();
    pick_from_chain(&retained, state, implementer, now)
}

/// Whether an **Advance** away from `abandoned` has anywhere to go: at
/// least one chain entry that is both hot and on a different engine
/// (issue #164). The runner consults this *before* discarding the
/// workspace — an advance with no target would throw the attempt away
/// and then re-run the same engine, or fail the re-pick outright.
pub fn has_advance_target(
    chain: &[ChainEntry],
    state: &StateFile,
    abandoned: Engine,
    now: DateTime<Utc>,
) -> bool {
    chain
        .iter()
        .any(|e| e.engine != abandoned && state.is_hot(e.engine, now))
}

/// The two-pass walk itself. Private so the exclusion is applied by
/// filtering the chain the walk sees, keeping one implementation of the
/// pass-1/pass-2 rules.
fn pick_from_chain(
    chain: &[ChainEntry],
    state: &StateFile,
    implementer: Option<Engine>,
    now: DateTime<Utc>,
) -> Result<PickedEntry, PickError> {
    // First hot entry overall — we'll need this in two places: the
    // "no implementer" path's pick, and the pass-2 fallback. Compute
    // once for clarity.
    let first_hot_overall = chain.iter().position(|e| state.is_hot(e.engine, now));

    let Some(implementer_engine) = implementer else {
        // Implement phase or any phase where the run-state has no
        // implementer-CLI yet. Pure chain walking: first hot entry
        // wins.
        return first_hot_overall
            .map(|idx| PickedEntry {
                entry: chain[idx].clone(),
                reason: PickReason::ChainFirstHotEntry,
            })
            .ok_or(PickError::AllCooling);
    };

    // Pass 1: first chain entry that is hot AND ≠ implementer.
    if let Some(idx) = chain
        .iter()
        .position(|e| state.is_hot(e.engine, now) && e.engine != implementer_engine)
    {
        // Reason: if no earlier hot entry was skipped because it
        // matched the implementer, this is just "chain first hot
        // entry"; otherwise the diversity preference kicked in and
        // skipped at least one same-as-implementer entry.
        let skipped_a_same_engine = chain[..idx]
            .iter()
            .any(|e| state.is_hot(e.engine, now) && e.engine == implementer_engine);
        let reason = if skipped_a_same_engine {
            PickReason::DiversityPreferred
        } else {
            PickReason::ChainFirstHotEntry
        };
        return Ok(PickedEntry {
            entry: chain[idx].clone(),
            reason,
        });
    }

    // Pass 2: first hot entry overall (every hot entry is the
    // implementer-CLI — diversity has collapsed). The runner emits
    // an operator-visible warning on this reason.
    if let Some(idx) = first_hot_overall {
        return Ok(PickedEntry {
            entry: chain[idx].clone(),
            reason: PickReason::SecondPassAfterCollapse,
        });
    }

    Err(PickError::AllCooling)
}

/// Render the single phase-start log line carrying phase, engine,
/// model, and **reason**. Issue #82 operator-visibility AC: operator
/// can reconstruct the engine-selection trail from the run-log alone.
///
/// Line shape — stable so log-scraping tooling can match:
///
/// ```text
/// bellows: phase `<phase>` engine=<name> model=<model> reason=<phrase>
/// ```
///
/// `model=<CLI default>` when the chain entry pinned no model
/// (preserves the slice-#81 phrasing); `reason=...` carries one of the
/// five `PickReason::as_run_log_phrase()` values.
pub fn format_phase_engine_log(
    phase_name: &str,
    entry: &ChainEntry,
    reason: PickReason,
) -> String {
    let model_field = entry.model.as_deref().unwrap_or("<CLI default>");
    format!(
        "bellows: phase `{phase}` engine={engine} model={model} reason={reason}",
        phase = phase_name,
        engine = entry.engine.as_name(),
        model = model_field,
        reason = reason.as_run_log_phrase(),
    )
}

/// Phase-start picker with the forced-single-engine label override
/// applied. ADR-0005 §"Per-issue forced-engine label": when
/// `forced_engine` is `Some(...)` the chain walk is bypassed entirely
/// — the labeled engine is used for every phase regardless of the
/// state file and chain order. Rate-limit on the forced engine
/// terminates the run (caller's responsibility; `pick_engine_for_phase`
/// itself never errors on a forced override).
///
/// When `forced_engine` is `None`, delegates to `pick_engine` so the
/// runner has a single call site covering both the forced and the
/// chain-walked cases.
///
/// Model pin: when the chain contains an entry for the forced engine
/// (e.g. `codex:gpt-5.5`), the picker surfaces that entry verbatim so
/// the operator's intended model survives the label override.
/// Otherwise it synthesises a model-less `ChainEntry` so the runner
/// can still dispatch — labels are engine-level, not model-pinning.
pub fn pick_engine_for_phase(
    chain: &[ChainEntry],
    state: &StateFile,
    implementer: Option<Engine>,
    forced_engine: Option<Engine>,
    now: DateTime<Utc>,
) -> Result<PickedEntry, PickError> {
    pick_engine_for_phase_excluding(chain, state, implementer, forced_engine, None, now)
}

/// [`pick_engine_for_phase`] with an engine struck out of the chain
/// walk (issue #164) — see [`pick_engine_excluding`] for why an
/// **Oscillation**-triggered advance needs one.
///
/// `forced_engine` still wins: a forced engine bypasses chain walking
/// entirely, and the runner never advances on an oscillation under a
/// forced engine, so the two are never both set in practice. Keeping
/// the forced branch first preserves ADR-0005's "the labeled engine is
/// used for every phase" promise regardless.
pub fn pick_engine_for_phase_excluding(
    chain: &[ChainEntry],
    state: &StateFile,
    implementer: Option<Engine>,
    forced_engine: Option<Engine>,
    excluded: Option<Engine>,
    now: DateTime<Utc>,
) -> Result<PickedEntry, PickError> {
    if let Some(engine) = forced_engine {
        let entry = chain
            .iter()
            .find(|e| e.engine == engine)
            .cloned()
            .unwrap_or(ChainEntry {
                engine,
                model: None,
            });
        return Ok(PickedEntry {
            entry,
            reason: PickReason::ForcedViaLabel,
        });
    }
    pick_engine_excluding(chain, state, implementer, excluded, now)
}

/// Implement-phase response to a rate-limit signature. Pure decision
/// shape consumed by the runner's bounded two-iteration implement
/// loop. ADR-0005 §"Rate-limit behaviour: implement phase vs the
/// rest":
///
/// - At base SHA with no prior in-place advance in this phase
///   invocation → `InPlaceAdvance` (drop workspace, swap to next hot
///   chain entry, re-run from base).
/// - Workspace ahead of base SHA, OR prior in-place advance already
///   used in this phase invocation → `Terminate` as RateLimited. The
///   ahead-of-base guard avoids dropping committed work; the max-1
///   cap preserves the single-pass-per-phase invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImplementRateLimitAction {
    /// Bounded retry: drop the workspace state (cheap — no commits to
    /// lose), pick the next hot chain entry, re-run from base.
    InPlaceAdvance,
    /// Terminate the run as RateLimited. State file is updated; the
    /// next claim consults it and walks the chain afresh.
    Terminate,
}

/// Decide how the runner should handle a rate-limit signature in the
/// implement phase. Pure function so the runner-level contract is
/// testable without docker.
pub fn decide_implement_rate_limit_action(
    at_base_sha: bool,
    advances_used: u8,
) -> ImplementRateLimitAction {
    if at_base_sha && advances_used == 0 {
        ImplementRateLimitAction::InPlaceAdvance
    } else {
        ImplementRateLimitAction::Terminate
    }
}

/// Non-implement-phase response to a rate-limit signature. Only one
/// shape today: terminate. Kept as an enum sibling of
/// `ImplementRateLimitAction` so a future relaxation (e.g. retry on
/// the non-fix phases) lands as a new variant rather than a
/// boolean-flip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonImplementRateLimitAction {
    /// Terminate the run as RateLimited. State file is updated; the
    /// next claim consults it and walks the chain afresh. ADR-0005:
    /// review-fix and security-fix operate on a workspace already
    /// carrying committed implement-phase work; dropping that
    /// workspace would destroy the agent's output, so terminate-and-
    /// defer is the only safe shape.
    Terminate,
}

/// Decide how the runner should handle a rate-limit signature in a
/// non-implement agent-invoking phase. The `_phase_name` parameter
/// is unused today (all four non-implement phases terminate) but is
/// kept on the signature so a future per-phase relaxation lands
/// without re-arranging the call site.
pub fn decide_non_implement_rate_limit_action(
    _phase_name: &str,
) -> NonImplementRateLimitAction {
    NonImplementRateLimitAction::Terminate
}

/// Default budget floor for an **Advance**: at least half the
/// configured `[agent].wall_clock_minutes` must remain. Overridable via
/// `[agent].advance_budget_floor_fraction`.
pub const DEFAULT_ADVANCE_BUDGET_FLOOR_FRACTION: f64 = 0.5;

/// Implement-phase response to an **Oscillation** (issue #164). The
/// sibling of [`ImplementRateLimitAction`] for the second advance
/// trigger; a separate enum because the fallback differs — a
/// rate-limit that cannot advance terminates the run as `RateLimited`,
/// whereas an oscillation that cannot advance is merely recorded and
/// the run continues to whatever terminal state it would have reached
/// anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscillationAdvanceAction {
    /// Drop the workspace, pick the next hot chain entry, re-run the
    /// implement phase from base — the same response the rate-limit
    /// path takes.
    InPlaceAdvance,
    /// Log the **Oscillation** and leave the run alone. Reached when
    /// the shared allowance is spent, the workspace is ahead of base
    /// SHA, or too little budget remains to be worth handing on.
    ContinueRun,
}

impl OscillationAdvanceAction {
    /// The disposition this action maps to, or `None` when the run
    /// carries on untouched. `InPlaceAdvance` deliberately produces the
    /// *same* [`RateLimitDisposition::InPlaceAdvance`] the rate-limit
    /// path produces: `CONTEXT.md` defines both triggers as meaning "no
    /// progress is available from this engine", so they share one
    /// response as well as one allowance.
    pub fn disposition(self) -> Option<RateLimitDisposition> {
        match self {
            OscillationAdvanceAction::InPlaceAdvance => {
                Some(RateLimitDisposition::InPlaceAdvance)
            }
            OscillationAdvanceAction::ContinueRun => None,
        }
    }
}

/// Decide how the runner should handle an **Oscillation** detected
/// during the implement phase. Pure function so the runner-level
/// contract is testable without docker, git, or a clock.
///
/// Four guards, all of which must pass:
///
/// 1. **At base SHA** — the pre-existing advance guard, unchanged. An
///    advance discards the workspace, so past base SHA there is
///    committed work to lose.
/// 2. **Shared allowance** — `advances_used == 0`. Oscillation and
///    rate-limit draw on the same max-1-per-phase-invocation budget; a
///    run that already advanced for either reason does not advance
///    again.
/// 3. **Budget floor** — at least `floor_fraction` of the configured
///    wall clock still remains. Handing a fresh engine the last ten
///    minutes of an hour wastes them.
/// 4. **Somewhere to advance to** — `next_entry_available`, from
///    [`has_advance_target`]. An advance that re-picked the engine that
///    just oscillated would discard the workspace to repeat the wedge;
///    with no other hot entry the honest answer is to log and continue.
pub fn decide_oscillation_advance_action(
    at_base_sha: bool,
    advances_used: u8,
    remaining: std::time::Duration,
    cap: std::time::Duration,
    floor_fraction: f64,
    next_entry_available: bool,
) -> OscillationAdvanceAction {
    if !at_base_sha || advances_used > 0 || !next_entry_available {
        return OscillationAdvanceAction::ContinueRun;
    }
    if !budget_above_floor(remaining, cap, floor_fraction) {
        return OscillationAdvanceAction::ContinueRun;
    }
    OscillationAdvanceAction::InPlaceAdvance
}

/// Whether `remaining` is at least `floor_fraction` of `cap`. A zero
/// `cap` is treated as "no budget left to hand on" rather than
/// dividing by zero; the config type forbids it, but the arithmetic
/// should not depend on that.
fn budget_above_floor(
    remaining: std::time::Duration,
    cap: std::time::Duration,
    floor_fraction: f64,
) -> bool {
    if cap.is_zero() {
        return false;
    }
    remaining.as_secs_f64() >= cap.as_secs_f64() * floor_fraction
}

/// How far into an implement-phase container an **Oscillation** may
/// still interrupt it (issue #164). `None` means "observe and log
/// only" — nothing detected during this container run could earn an
/// advance, so there is no reason to cut the run short.
///
/// This is the pre-launch half of the same guards
/// [`decide_oscillation_advance_action`] applies afterwards: the
/// shared allowance and the engine-forced bypass are already settled
/// before the container starts, and the budget floor becomes a
/// deadline on this container's own clock (budget at launch minus the
/// floor). The at-base-SHA guard is deliberately absent — the agent
/// can self-commit mid-run, so only the post-run decision can judge
/// it.
///
/// `phase_deadline` is the budget remaining as the phase starts;
/// `None` (budget already spent) yields no window.
///
/// `floor_fraction` is expected in `0.0..=1.0` —
/// [`ConfigError::InvalidBudgetFloorFraction`](crate::config::ConfigError)
/// rejects anything else at config-load time. Should one reach here
/// anyway (this is a `pub` entry point), the floor is computed
/// fallibly rather than with the panicking
/// `Duration::from_secs_f64`, and an uncomputable floor yields `None`
/// — the conservative answer, since `None` only ever costs an
/// advance, never a run.
pub fn oscillation_kill_window(
    phase_deadline: Option<std::time::Duration>,
    cap: std::time::Duration,
    floor_fraction: f64,
    advances_used: u8,
    engine_forced: bool,
) -> Option<std::time::Duration> {
    if advances_used > 0 || engine_forced {
        return None;
    }
    let floor = std::time::Duration::try_from_secs_f64(cap.as_secs_f64() * floor_fraction).ok()?;
    phase_deadline?
        .checked_sub(floor)
        .filter(|window| !window.is_zero())
}

/// Render the run-log line for an **Oscillation**-triggered advance.
/// Names the trigger, the engine whose attempt is being abandoned, and
/// the engine the phase is being handed to, so an operator reading the
/// log alone can tell it from a rate-limit advance.
pub fn format_oscillation_advance_log(abandoned: Engine, advancing_to: Engine) -> String {
    format!(
        "bellows: implement oscillation detected with engine={abandoned}; \
         abandoning its attempt at base SHA and in-place-advancing to engine={advancing_to} \
         (max 1 advance per phase invocation, shared with the rate-limit trigger)",
        abandoned = abandoned.as_name(),
        advancing_to = advancing_to.as_name(),
    )
}

/// Render the run-log line for an **Oscillation** bellows declined to
/// act on. `CONTEXT.md`: an advance is bounded, so a wedged run below
/// the floor or past its allowance continues to its existing terminal
/// state — but the operator still gets told what was seen.
pub fn format_oscillation_not_advanced_log(engine: Engine, why: &str) -> String {
    format!(
        "bellows: implement oscillation detected with engine={engine}; not advancing ({why}); \
         the run continues to its existing terminal state",
        engine = engine.as_name(),
    )
}

/// Render the run-log line for an **Idleness** observation.
/// `CONTEXT.md`: "Recorded for the operator, never acted on" — a still
/// workspace is indistinguishable from an engine reasoning about a
/// hard problem or one about to exit cleanly.
pub fn format_idleness_log(samples: usize, interval_seconds: u64) -> String {
    format!(
        "bellows: implement workspace unchanged for {samples} consecutive samples \
         (~{minutes} minutes); recording idleness, not acting on it",
        minutes = (samples as u64 * interval_seconds) / 60,
    )
}

/// Disposition the runner consults after a rate-limit signature
/// match. Unifies the implement and non-implement paths so the
/// runner's phase-exit handler has one shape to match on.
///
/// Issue #164 gave `InPlaceAdvance` a second trigger: an
/// **Oscillation** detected while the implement container runs takes
/// this same disposition, via
/// [`OscillationAdvanceAction::disposition`]. The type name still says
/// "rate limit" only because renaming it would ripple across the
/// runner for no behavioural gain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitDisposition {
    /// Implement-phase only: drop workspace, pick next hot chain
    /// entry, re-run from base. Bounded to max-1 per phase
    /// invocation via the `advances_used` parameter of
    /// `handle_implement_rate_limit`.
    InPlaceAdvance,
    /// Terminate the run as `ExitReason::RateLimited`. The state
    /// file has already been updated by the helper; the next claim
    /// walks the chain afresh under the freshly-read cooldowns.
    Terminate,
}

/// Compose the parse → record → decide flow for an implement-phase
/// rate-limit signature match. Mutates `state` with the parsed (or
/// fallback) `cooling_until` and returns the runner's next action.
///
/// `at_base_sha` is `head_after_implement == head_before_implement`
/// from the runner's perspective; `advances_used` is the count of
/// in-place advances already performed in this phase invocation
/// (capped at 1 by `decide_implement_rate_limit_action`).
pub fn handle_implement_rate_limit(
    state: &mut StateFile,
    engine: Engine,
    stderr: &str,
    now: DateTime<Utc>,
    at_base_sha: bool,
    advances_used: u8,
) -> RateLimitDisposition {
    let parsed = parse_cooling_until(engine, stderr, now);
    state.record_rate_limit(engine, parsed.cooling_until);
    match decide_implement_rate_limit_action(at_base_sha, advances_used) {
        ImplementRateLimitAction::InPlaceAdvance => RateLimitDisposition::InPlaceAdvance,
        ImplementRateLimitAction::Terminate => RateLimitDisposition::Terminate,
    }
}

/// Compose the parse → record → terminate flow for a non-implement-
/// phase rate-limit signature match. Mutates `state` with the parsed
/// (or fallback) `cooling_until` and returns
/// `RateLimitDisposition::Terminate`. The `_phase_name` parameter is
/// consumed by `decide_non_implement_rate_limit_action` (currently
/// always terminates).
pub fn handle_non_implement_rate_limit(
    state: &mut StateFile,
    engine: Engine,
    phase_name: &str,
    stderr: &str,
    now: DateTime<Utc>,
) -> RateLimitDisposition {
    let parsed = parse_cooling_until(engine, stderr, now);
    state.record_rate_limit(engine, parsed.cooling_until);
    match decide_non_implement_rate_limit_action(phase_name) {
        NonImplementRateLimitAction::Terminate => RateLimitDisposition::Terminate,
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
