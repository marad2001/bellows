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

#[derive(Debug, Deserialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub method: AuthMethod,
    #[serde(default = "default_credentials_volume")]
    pub credentials_volume: String,
    /// Name of the Docker volume holding per-repo SSH deploy keys
    /// (issue #69 / ADR-0002). Populated via
    /// `bellows setup-deploy-keys add` and mounted read-only at
    /// `/home/bellows/.ssh/` into agent and cargo-checks containers
    /// spawned for `[[repo]]` blocks whose `deploy_keys` list is
    /// non-empty. Parallel to but distinct from `credentials_volume`
    /// — separate lifecycle, separate setup command, separate purpose.
    #[serde(default = "default_ssh_keys_volume")]
    pub ssh_keys_volume: String,
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
            credentials_volume: default_credentials_volume(),
            ssh_keys_volume: default_ssh_keys_volume(),
        }
    }
}

fn default_credentials_volume() -> String {
    "bellows-claude-credentials".to_string()
}

fn default_ssh_keys_volume() -> String {
    "bellows-deploy-keys".to_string()
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
    auth: AuthConfig,
    #[serde(default)]
    agent: AgentConfig,
    #[serde(default)]
    gates: GatesConfig,
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
            auth: raw.auth,
            agent: raw.agent,
            gates: raw.gates,
        })
    }
}
