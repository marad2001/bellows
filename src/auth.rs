use bollard::models::{Mount, MountType};

use crate::config::Engine;

/// Where Claude Code's home directory ends up inside the agent container.
/// The credentials volume mounts here; the policy-image entrypoint copies
/// the baked skills + CLAUDE.md into the same path at container start so
/// they layer with the persisted OAuth tokens.
///
/// Owned by the non-root `bellows` user inside the container. Anthropic
/// blocks `--dangerously-skip-permissions` when claude runs as root, so
/// the policy image runs as a constrained account; the credentials home
/// has to be under that user's HOME for permissions to work.
pub const CLAUDE_HOME_IN_CONTAINER: &str = "/home/bellows/.claude";

/// Where Codex's `$CODEX_HOME` directory ends up inside the agent
/// container. Sibling of `CLAUDE_HOME_IN_CONTAINER` — same per-user home,
/// different subdirectory. The codex credentials volume mounts here; the
/// codex CLI reads `auth.json`, `cap_sid`, and `config.toml` from
/// `$CODEX_HOME`, which is `~/.codex/` by default.
pub const CODEX_HOME_IN_CONTAINER: &str = "/home/bellows/.codex";

pub enum Auth {
    Subscription {
        /// Which engine this auth bundle targets. Drives the mount
        /// target (`/home/bellows/.claude` vs `/home/bellows/.codex`)
        /// and the `BELLOWS_ENGINE` env var the runner sets on the
        /// spawned container.
        engine: Engine,
        /// Optional model pin from the chain entry. When `Some`, the
        /// runner exports `BELLOWS_MODEL=<name>` and the policy
        /// image's `run-agent` script appends `-m <name>` to the CLI
        /// invocation. When `None`, the CLI uses its default model.
        model: Option<String>,
        credentials_volume_name: String,
    },
    ApiKey,
}

impl Auth {
    /// Engine this auth bundle dispatches to. Centralises the
    /// `Auth → Engine` mapping so the runner doesn't have to know about
    /// the internal `Auth` variants.
    pub fn engine(&self) -> Engine {
        match self {
            Auth::Subscription { engine, .. } => *engine,
            Auth::ApiKey => todo!("Auth::ApiKey is not implemented in v1"),
        }
    }

    /// Model pin for this phase, if any. Used by the runner to render
    /// the per-phase log line (`<CLI default>` when `None`).
    pub fn model(&self) -> Option<&str> {
        match self {
            Auth::Subscription { model, .. } => model.as_deref(),
            Auth::ApiKey => None,
        }
    }

    pub fn extra_mounts(&self) -> Vec<Mount> {
        match self {
            Auth::Subscription {
                engine,
                credentials_volume_name,
                ..
            } => {
                let target = match engine {
                    Engine::Claude => CLAUDE_HOME_IN_CONTAINER,
                    Engine::Codex => CODEX_HOME_IN_CONTAINER,
                    Engine::Opencode => unreachable!(
                        "Auth::Subscription is OAuth-volume; opencode is API-key-env-file and \
                         must not construct a Subscription variant"
                    ),
                };
                vec![Mount {
                    target: Some(target.to_string()),
                    source: Some(credentials_volume_name.clone()),
                    typ: Some(MountType::VOLUME),
                    ..Default::default()
                }]
            }
            Auth::ApiKey => todo!("Auth::ApiKey is not implemented in v1"),
        }
    }

    pub fn extra_env(&self) -> Vec<String> {
        match self {
            Auth::Subscription { engine, model, .. } => {
                let mut env = vec![format!("BELLOWS_ENGINE={}", engine.as_name())];
                if let Some(m) = model {
                    env.push(format!("BELLOWS_MODEL={m}"));
                }
                env
            }
            Auth::ApiKey => todo!("Auth::ApiKey is not implemented in v1"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_claude_mounts_at_claude_home() {
        let auth = Auth::Subscription {
            engine: Engine::Claude,
            model: None,
            credentials_volume_name: "my-claude-creds".to_string(),
        };
        let mounts = auth.extra_mounts();
        assert_eq!(mounts.len(), 1);
        let m = &mounts[0];
        assert_eq!(m.typ, Some(MountType::VOLUME));
        assert_eq!(m.source.as_deref(), Some("my-claude-creds"));
        assert_eq!(m.target.as_deref(), Some(CLAUDE_HOME_IN_CONTAINER));
    }

    #[test]
    fn subscription_codex_mounts_at_codex_home() {
        // Issue #81 / ADR-0005: the codex credentials volume mounts
        // at `$CODEX_HOME` inside the container (= /home/bellows/.codex),
        // sibling of /home/bellows/.claude. Same per-user home,
        // different engine-specific subdirectory.
        let auth = Auth::Subscription {
            engine: Engine::Codex,
            model: None,
            credentials_volume_name: "my-codex-creds".to_string(),
        };
        let mounts = auth.extra_mounts();
        assert_eq!(mounts.len(), 1);
        let m = &mounts[0];
        assert_eq!(m.typ, Some(MountType::VOLUME));
        assert_eq!(m.source.as_deref(), Some("my-codex-creds"));
        assert_eq!(m.target.as_deref(), Some(CODEX_HOME_IN_CONTAINER));
    }

    #[test]
    fn subscription_sets_bellows_engine_env_var() {
        // The runner sets BELLOWS_ENGINE per-phase by handing each
        // phase's Auth to run_agent; the env var flows through
        // `auth.extra_env()` so the policy image's `run-agent` script
        // can branch on it. Pin the contract here.
        let claude = Auth::Subscription {
            engine: Engine::Claude,
            model: None,
            credentials_volume_name: "_".to_string(),
        };
        assert!(claude
            .extra_env()
            .iter()
            .any(|e| e == "BELLOWS_ENGINE=claude"));
        let codex = Auth::Subscription {
            engine: Engine::Codex,
            model: None,
            credentials_volume_name: "_".to_string(),
        };
        assert!(codex
            .extra_env()
            .iter()
            .any(|e| e == "BELLOWS_ENGINE=codex"));
    }

    #[test]
    fn subscription_with_model_pin_sets_bellows_model_env_var() {
        // The runner exports BELLOWS_MODEL=<name> when a chain entry
        // pinned a model, and the policy image's run-agent script
        // appends `-m <name>` to the CLI invocation. No env var means
        // the CLI uses its default model.
        let pinned = Auth::Subscription {
            engine: Engine::Claude,
            model: Some("opus-4-7".to_string()),
            credentials_volume_name: "_".to_string(),
        };
        assert!(pinned
            .extra_env()
            .iter()
            .any(|e| e == "BELLOWS_MODEL=opus-4-7"));
        let unpinned = Auth::Subscription {
            engine: Engine::Claude,
            model: None,
            credentials_volume_name: "_".to_string(),
        };
        assert!(!unpinned
            .extra_env()
            .iter()
            .any(|e| e.starts_with("BELLOWS_MODEL=")));
    }
}
