use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub repo: RepoConfig,
    pub github: GithubConfig,
    #[serde(default)]
    pub polling: PollingConfig,
    #[serde(default)]
    pub runtime_labels: RuntimeLabelsConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Deserialize)]
pub struct RepoConfig {
    pub url: String,
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

impl Config {
    pub fn from_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }
}
