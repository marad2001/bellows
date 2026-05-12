use serde::Deserialize;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("toml: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("[[repo]] list must not be empty; configure at least one repo")]
    EmptyRepoList,
    #[error(
        "[phases.{phase}].cli_chain must contain at least one entry; \
         configure one engine or remove the table entirely to use the default `[\"claude\"]`"
    )]
    EmptyCliChain { phase: &'static str },
    #[error("[phases.{phase}].cli_chain entry {index}: {source}")]
    InvalidChainEntry {
        phase: &'static str,
        index: usize,
        #[source]
        source: EngineChainParseError,
    },
}

/// One of the two engines bellows can dispatch to. Wired through the
/// per-phase `cli_chain`, the `BELLOWS_ENGINE` env var the runner sets
/// per-phase, the per-issue `engine:<name>` label override, and the
/// per-engine credentials volume in `[auth.<name>]`. Adding a third
/// engine is data, not code shape — the chain config, label parser, and
/// auth config all key on engine name strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Engine {
    Claude,
    Codex,
}

impl Engine {
    /// Lower-case canonical name string — load-bearing for the
    /// `BELLOWS_ENGINE=<name>` env-var dispatch, the chain entry
    /// `<engine>:<model>` parser, and the `engine:<name>` label
    /// match.
    pub fn as_name(&self) -> &'static str {
        match self {
            Engine::Claude => "claude",
            Engine::Codex => "codex",
        }
    }

    /// Inverse of `as_name`. Case-sensitive on purpose — the
    /// operator's config and labels are lower-case by convention, so
    /// surfacing a typo (`"Claude"` etc.) as `None` keeps the
    /// failure operator-legible rather than silently matching.
    pub fn from_name(name: &str) -> Option<Engine> {
        match name {
            "claude" => Some(Engine::Claude),
            "codex" => Some(Engine::Codex),
            _ => None,
        }
    }
}

/// One entry in a phase's `cli_chain`. Carries the engine choice plus
/// an optional model pin. `model: None` means "the CLI's default model
/// — bellows omits the `-m` flag." A `Some` value is opaque
/// pass-through (no allow-list) since the available models depend on
/// subscription tier and shift over time; the CLI reports unknown-model
/// at run time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainEntry {
    pub engine: Engine,
    pub model: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineChainParseError {
    #[error("chain entry is empty")]
    Empty,
    #[error("unknown engine `{0}` (expected `claude` or `codex`)")]
    UnknownEngine(String),
}

impl FromStr for ChainEntry {
    type Err = EngineChainParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(EngineChainParseError::Empty);
        }
        // Split on first `:` so model strings that themselves contain
        // `:` (e.g. an organisation-prefixed model name) round-trip
        // verbatim through the model side. Brief: "Split on the first
        // `:`."
        let (engine_part, model_part) = match s.split_once(':') {
            Some((engine, model)) => (engine, Some(model.to_string())),
            None => (s, None),
        };
        let engine = Engine::from_name(engine_part)
            .ok_or_else(|| EngineChainParseError::UnknownEngine(engine_part.to_string()))?;
        Ok(ChainEntry {
            engine,
            model: model_part,
        })
    }
}

/// Refuse-to-claim signal from `EngineLabelOverride::parse` — parallel
/// to `RunError::MissingAgentBrief` but produced upstream of
/// claim-time. Carrying the issue number is the parser's caller's job
/// (the parser only sees a label list).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EngineLabelOverrideError {
    #[error(
        "both `engine:claude` and `engine:codex` are present on this issue; \
         operator must pick one. Refusing to claim."
    )]
    AmbiguousEngineLabels,
}

/// Pre-claim engine-override resolution from the issue's labels. Returns
/// `Ok(Some(Engine))` when exactly one `engine:<name>` label is present,
/// `Ok(None)` when no engine label is present (the chain walk drives the
/// pick), and `Err(AmbiguousEngineLabels)` when both engine labels are
/// present. The `Err` shape is intentionally parallel to
/// `RunError::MissingAgentBrief`: the polling tick refuses to claim and
/// surfaces the verdict so the operator can resolve the ambiguity.
pub struct EngineLabelOverride;

impl EngineLabelOverride {
    pub fn parse<S: AsRef<str>>(
        labels: &[S],
    ) -> Result<Option<Engine>, EngineLabelOverrideError> {
        let mut has_claude = false;
        let mut has_codex = false;
        for label in labels {
            match label.as_ref() {
                "engine:claude" => has_claude = true,
                "engine:codex" => has_codex = true,
                _ => {}
            }
        }
        match (has_claude, has_codex) {
            (true, true) => Err(EngineLabelOverrideError::AmbiguousEngineLabels),
            (true, false) => Ok(Some(Engine::Claude)),
            (false, true) => Ok(Some(Engine::Codex)),
            (false, false) => Ok(None),
        }
    }
}

#[derive(Debug)]
pub struct Config {
    /// Configured repos to poll. May have one element (legacy `[repo]`
    /// table form) or many (the slice `[[repo]]` array-of-tables form
    /// added by issue #35). Always non-empty — `FromStr` rejects an
    /// empty list at parse time.
    pub repos: Vec<RepoConfig>,
    pub github: GithubConfig,
    pub polling: PollingConfig,
    pub runtime_labels: RuntimeLabelsConfig,
    pub logging: LoggingConfig,
    pub auth: AuthConfig,
    pub agent: AgentConfig,
    pub gates: GatesConfig,
    /// Per-phase engine selection chain (issue #81 / ADR-0005). Every
    /// agent-invoking phase has its own `cli_chain: Vec<ChainEntry>`
    /// declaring the preferred engine order (with optional per-entry
    /// model pins). Defaults to `["claude"]` for each phase so existing
    /// v1 operator configs see no behaviour change.
    pub phases: PhasesConfig,
}

#[derive(Debug, Deserialize)]
pub struct RepoConfig {
    pub url: String,
    /// Names of deploy keys (issue #69 / ADR-0002) the agent and
    /// cargo-checks containers spawned for THIS repo should be able to
    /// see. Each name must correspond to a regular file in the
    /// `[auth].ssh_keys_volume` Docker volume. Empty / unset means no
    /// SSH credentials are mounted — preserving the "no creds in
    /// sandbox by default" posture across every repo that doesn't
    /// explicitly opt in.
    #[serde(default)]
    pub deploy_keys: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct GithubConfig {
    pub pat_env_var: String,
}

#[derive(Debug, Deserialize)]
pub struct PollingConfig {
    #[serde(default = "default_interval_seconds")]
    pub interval_seconds: u64,
    #[serde(default = "default_pickup_label")]
    pub pickup_label: String,
}

impl Default for PollingConfig {
    fn default() -> Self {
        Self {
            interval_seconds: default_interval_seconds(),
            pickup_label: default_pickup_label(),
        }
    }
}

fn default_interval_seconds() -> u64 {
    45
}

fn default_pickup_label() -> String {
    "ready-for-agent".to_string()
}

#[derive(Debug, Deserialize)]
pub struct RuntimeLabelsConfig {
    #[serde(default = "default_agent_in_progress")]
    pub agent_in_progress: String,
    #[serde(default = "default_agent_done")]
    pub agent_done: String,
    #[serde(default = "default_agent_failed")]
    pub agent_failed: String,
    #[serde(default = "default_agent_rate_limited")]
    pub agent_rate_limited: String,
    #[serde(default = "default_agent_cancelled")]
    pub agent_cancelled: String,
}

impl Default for RuntimeLabelsConfig {
    fn default() -> Self {
        Self {
            agent_in_progress: default_agent_in_progress(),
            agent_done: default_agent_done(),
            agent_failed: default_agent_failed(),
            agent_rate_limited: default_agent_rate_limited(),
            agent_cancelled: default_agent_cancelled(),
        }
    }
}

fn default_agent_in_progress() -> String {
    "agent-in-progress".to_string()
}

fn default_agent_done() -> String {
    "agent-done".to_string()
}

fn default_agent_failed() -> String {
    "agent-failed".to_string()
}

fn default_agent_rate_limited() -> String {
    "agent-rate-limited".to_string()
}

fn default_agent_cancelled() -> String {
    "agent-cancelled".to_string()
}

#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_logging_path")]
    pub path: PathBuf,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            path: default_logging_path(),
        }
    }
}

fn default_logging_path() -> PathBuf {
    PathBuf::from("bellows.log")
}

/// Top-level `[auth]` block. Per-engine credentials volumes live in
/// `[auth.claude]` / `[auth.codex]` subtables (issue #81 / ADR-0005);
/// the previously flat `auth.credentials_volume` key continues to work
/// and is rewritten to `auth.claude.credentials_volume` at config-load
/// time for backwards compatibility.
#[derive(Debug)]
pub struct AuthConfig {
    pub method: AuthMethod,
    /// Claude's credentials volume + setup. Required only when some
    /// phase's `cli_chain` (or a forced-single-engine `engine:claude`
    /// label) dispatches to Claude — lazy validation per ADR-0005.
    pub claude: EngineAuthConfig,
    /// Codex's credentials volume + setup. Required only when some
    /// phase's `cli_chain` (or a forced-single-engine `engine:codex`
    /// label) dispatches to Codex — lazy validation per ADR-0005.
    pub codex: EngineAuthConfig,
    /// Name of the Docker volume holding per-repo SSH deploy keys
    /// (issue #69 / ADR-0002). Mounted into containers regardless of
    /// engine choice.
    pub ssh_keys_volume: String,
}

impl AuthConfig {
    /// Per-engine credentials-volume lookup. Centralised here so the
    /// runner and `bellows setup-auth --engine` share one source of
    /// truth.
    pub fn for_engine(&self, engine: Engine) -> &EngineAuthConfig {
        match engine {
            Engine::Claude => &self.claude,
            Engine::Codex => &self.codex,
        }
    }
}

/// One engine's credentials-volume settings. Currently a single field;
/// kept as its own struct so a future per-engine setting (e.g. session
/// timeout, model allowlist) can land without re-flattening the wire
/// shape.
#[derive(Debug, Deserialize, Clone)]
pub struct EngineAuthConfig {
    pub credentials_volume: String,
}

#[derive(Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    #[default]
    Subscription,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            method: AuthMethod::default(),
            claude: EngineAuthConfig {
                credentials_volume: default_claude_credentials_volume(),
            },
            codex: EngineAuthConfig {
                credentials_volume: default_codex_credentials_volume(),
            },
            ssh_keys_volume: default_ssh_keys_volume(),
        }
    }
}

fn default_claude_credentials_volume() -> String {
    "bellows-claude-credentials".to_string()
}

fn default_codex_credentials_volume() -> String {
    "bellows-codex-credentials".to_string()
}

fn default_ssh_keys_volume() -> String {
    "bellows-deploy-keys".to_string()
}

/// Wire shape for `[auth]` and its `[auth.claude]` / `[auth.codex]`
/// subtables. Held at deserialize time only; `FromStr` normalises this
/// (and rewrites a top-level `credentials_volume` to claude's) into the
/// public `AuthConfig`.
#[derive(Debug, Deserialize, Default)]
struct RawAuthConfig {
    #[serde(default)]
    method: AuthMethod,
    /// Backwards-compat flat key. When present (and `auth.claude` is
    /// omitted), rewritten into `auth.claude.credentials_volume`.
    credentials_volume: Option<String>,
    /// `None` → default. Held as `Option` rather than a serde-default
    /// because `RawAuthConfig::default()` (used when the `[auth]`
    /// section is omitted entirely) would otherwise produce an empty
    /// string here; the public `AuthConfig` always carries the
    /// resolved default.
    ssh_keys_volume: Option<String>,
    claude: Option<EngineAuthConfig>,
    codex: Option<EngineAuthConfig>,
}

impl RawAuthConfig {
    fn normalise(self) -> AuthConfig {
        // Backwards-compat: flat `auth.credentials_volume` rewrites to
        // `auth.claude.credentials_volume`. Explicit per-engine
        // `[auth.claude]` wins over the flat key when both are
        // configured (operator opted into the new shape, so the new
        // shape is the authoritative one).
        let claude = self.claude.unwrap_or_else(|| EngineAuthConfig {
            credentials_volume: self
                .credentials_volume
                .clone()
                .unwrap_or_else(default_claude_credentials_volume),
        });
        let codex = self.codex.unwrap_or_else(|| EngineAuthConfig {
            credentials_volume: default_codex_credentials_volume(),
        });
        AuthConfig {
            method: self.method,
            claude,
            codex,
            ssh_keys_volume: self
                .ssh_keys_volume
                .unwrap_or_else(default_ssh_keys_volume),
        }
    }
}

/// Per-issue agent budget. Currently just the wall-clock cap; later
/// slices may add per-phase budgets, retry policy, etc.
#[derive(Debug, Deserialize)]
pub struct AgentConfig {
    /// `NonZeroU64` rather than `u64` so a misconfigured `0` is
    /// rejected at config load time rather than silently producing an
    /// always-exceeded budget that bypasses the cap entirely. The
    /// runner converts this to `Duration` via `.get() * 60`.
    #[serde(default = "default_wall_clock_minutes")]
    pub wall_clock_minutes: NonZeroU64,
    /// Slice 8: when an issue carries this label, the post-implement
    /// weak-test guard is short-circuited entirely. The cargo gate
    /// still runs as normal. Default `"refactor"` — appropriate for
    /// briefs that legitimately produce no new tests (renames, pure
    /// refactors, dependency bumps).
    #[serde(default = "default_weak_test_guard_skip_label")]
    pub weak_test_guard_skip_label: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            wall_clock_minutes: default_wall_clock_minutes(),
            weak_test_guard_skip_label: default_weak_test_guard_skip_label(),
        }
    }
}

fn default_wall_clock_minutes() -> NonZeroU64 {
    NonZeroU64::new(60).expect("60 is non-zero")
}

fn default_weak_test_guard_skip_label() -> String {
    "refactor".to_string()
}

/// ADR-0004 fallback flags for the cargo-checks gate. Used when bellows
/// cannot parse the target repo's `.github/workflows/*.yml` to extract
/// the verbatim `cargo clippy` / `cargo test` commands. Defaults
/// preserve today's strict bar so any existing operator
/// `orchestrator.toml` that omits the `[gates]` table sees no change in
/// behaviour.
#[derive(Debug, Deserialize)]
pub struct GatesConfig {
    #[serde(default = "default_clippy_flags")]
    pub clippy_flags: String,
    #[serde(default = "default_test_flags")]
    pub test_flags: String,
}

impl Default for GatesConfig {
    fn default() -> Self {
        Self {
            clippy_flags: default_clippy_flags(),
            test_flags: default_test_flags(),
        }
    }
}

fn default_clippy_flags() -> String {
    "--all-targets --all-features -- -D warnings".to_string()
}

fn default_test_flags() -> String {
    "--all-targets --all-features".to_string()
}

/// Per-phase engine-selection chain (issue #81 / ADR-0005). One
/// `cli_chain: Vec<ChainEntry>` per agent-invoking phase; each chain
/// defaults to `["claude"]` when the phase's `[phases.X]` table is
/// omitted, so existing v1 single-engine operator configs see no
/// behaviour change.
#[derive(Debug, Clone)]
pub struct PhasesConfig {
    pub implement: PhaseChain,
    pub review: PhaseChain,
    pub review_fix: PhaseChain,
    pub security_review: PhaseChain,
    pub security_fix: PhaseChain,
}

impl Default for PhasesConfig {
    fn default() -> Self {
        Self {
            implement: PhaseChain::default(),
            review: PhaseChain::default(),
            review_fix: PhaseChain::default(),
            security_review: PhaseChain::default(),
            security_fix: PhaseChain::default(),
        }
    }
}

/// One phase's chain. The default chain is `[ClaimChainEntry::claude]`
/// — i.e. the v1 single-engine claude-only behaviour. The chain is
/// always non-empty (an empty `cli_chain = []` is rejected at
/// config-load time).
#[derive(Debug, Clone)]
pub struct PhaseChain {
    pub cli_chain: Vec<ChainEntry>,
}

impl Default for PhaseChain {
    fn default() -> Self {
        Self {
            cli_chain: vec![ChainEntry {
                engine: Engine::Claude,
                model: None,
            }],
        }
    }
}

impl PhaseChain {
    /// First chain entry. In this slice (#81) bellows always uses the
    /// first entry; chain walking + soft-diversity + rate-limit state
    /// land in slice #82. Centralising the access here keeps slice-#82's
    /// addition a single-call-site change.
    pub fn first_entry(&self) -> &ChainEntry {
        self.cli_chain
            .first()
            .expect("cli_chain non-empty by config-load invariant")
    }
}

/// Wire shape for `[phases.X]` tables. Bare strings parse via
/// `ChainEntry::from_str` after deserialization — `serde` only gives us
/// `Vec<String>` here since the chain entry grammar is bellows-internal,
/// not TOML-native.
#[derive(Debug, Deserialize, Default)]
struct RawPhaseChain {
    #[serde(default)]
    cli_chain: Option<Vec<String>>,
}

impl RawPhaseChain {
    fn normalise(self, phase: &'static str) -> Result<PhaseChain, ConfigError> {
        let Some(raw) = self.cli_chain else {
            return Ok(PhaseChain::default());
        };
        if raw.is_empty() {
            return Err(ConfigError::EmptyCliChain { phase });
        }
        let mut entries = Vec::with_capacity(raw.len());
        for (index, raw_entry) in raw.into_iter().enumerate() {
            let entry: ChainEntry = raw_entry
                .parse()
                .map_err(|source| ConfigError::InvalidChainEntry {
                    phase,
                    index,
                    source,
                })?;
            entries.push(entry);
        }
        Ok(PhaseChain { cli_chain: entries })
    }
}

#[derive(Debug, Deserialize, Default)]
struct RawPhasesConfig {
    #[serde(default)]
    implement: RawPhaseChain,
    #[serde(default)]
    review: RawPhaseChain,
    #[serde(default)]
    review_fix: RawPhaseChain,
    #[serde(default)]
    security_review: RawPhaseChain,
    #[serde(default)]
    security_fix: RawPhaseChain,
}

impl RawPhasesConfig {
    fn normalise(self) -> Result<PhasesConfig, ConfigError> {
        Ok(PhasesConfig {
            implement: self.implement.normalise("implement")?,
            review: self.review.normalise("review")?,
            review_fix: self.review_fix.normalise("review_fix")?,
            security_review: self.security_review.normalise("security_review")?,
            security_fix: self.security_fix.normalise("security_fix")?,
        })
    }
}

/// Wire-shape used only at deserialize time. The `repo` key accepts
/// either a single `[repo]` table (legacy single-repo form) or a
/// `[[repo]]` array-of-tables (multi-repo form added in issue #35).
/// `FromStr` normalises both into `Config.repos: Vec<RepoConfig>`.
#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(rename = "repo")]
    repo_field: RepoField,
    github: GithubConfig,
    #[serde(default)]
    polling: PollingConfig,
    #[serde(default)]
    runtime_labels: RuntimeLabelsConfig,
    #[serde(default)]
    logging: LoggingConfig,
    #[serde(default)]
    auth: RawAuthConfig,
    #[serde(default)]
    agent: AgentConfig,
    #[serde(default)]
    gates: GatesConfig,
    #[serde(default)]
    phases: RawPhasesConfig,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RepoField {
    /// `[repo]\nurl = "..."` — the legacy single-repo shape. Continues
    /// to parse for backward compatibility; normalised into a
    /// one-element list at `FromStr` time.
    Single(RepoConfig),
    /// `[[repo]]\nurl = "..."` — array-of-tables form for the
    /// multi-repo polling slice (#35).
    Multiple(Vec<RepoConfig>),
}

impl FromStr for Config {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let raw: RawConfig = toml::from_str(s)?;
        let repos = match raw.repo_field {
            RepoField::Single(r) => vec![r],
            RepoField::Multiple(v) => v,
        };
        if repos.is_empty() {
            return Err(ConfigError::EmptyRepoList);
        }
        Ok(Config {
            repos,
            github: raw.github,
            polling: raw.polling,
            runtime_labels: raw.runtime_labels,
            logging: raw.logging,
            auth: raw.auth.normalise(),
            agent: raw.agent,
            gates: raw.gates,
            phases: raw.phases.normalise()?,
        })
    }
}
