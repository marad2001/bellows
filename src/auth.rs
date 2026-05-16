use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
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
    /// ADR-0008 / issue #120 AC11: API-key auth via a host-side
    /// `KEY=VALUE` env-file (the moral equivalent of
    /// `docker run --env-file <path>`, except bellows uses bollard's
    /// structured `env: Vec<String>` rather than spawning the docker
    /// CLI). The env-file is parsed at container-create time and
    /// each line is appended to the container's env array.
    EnvFile {
        /// Engine this auth bundle dispatches to (opencode for v1).
        engine: Engine,
        /// Optional model pin from the chain entry, same shape as
        /// `Subscription::model`.
        model: Option<String>,
        /// Host-side path to the env-file written by `setup-auth`.
        env_file_path: PathBuf,
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
            Auth::EnvFile { engine, .. } => *engine,
            Auth::ApiKey => todo!("Auth::ApiKey is not implemented in v1"),
        }
    }

    /// Model pin for this phase, if any. Used by the runner to render
    /// the per-phase log line (`<CLI default>` when `None`).
    pub fn model(&self) -> Option<&str> {
        match self {
            Auth::Subscription { model, .. } => model.as_deref(),
            Auth::EnvFile { model, .. } => model.as_deref(),
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
            // The env-file path is host-side only; it is read at
            // container-create time and its contents are passed in
            // via the env array (see `try_extra_env`). No bind mount
            // and no volume mount — the API key never lives on disk
            // inside the container.
            Auth::EnvFile { .. } => Vec::new(),
            Auth::ApiKey => todo!("Auth::ApiKey is not implemented in v1"),
        }
    }

    /// Engine env (and any pinned model env) plus, for `Auth::EnvFile`,
    /// the parsed env-file lines. Panics if the env-file is missing
    /// or permissively-readable — production callers should prefer
    /// `try_extra_env` which surfaces the same conditions as `Err`.
    pub fn extra_env(&self) -> Vec<String> {
        match self {
            Auth::Subscription { engine, model, .. } => {
                let mut env = vec![format!("BELLOWS_ENGINE={}", engine.as_name())];
                if let Some(m) = model {
                    env.push(format!("BELLOWS_MODEL={m}"));
                }
                env
            }
            Auth::EnvFile { .. } => self
                .try_extra_env()
                .expect("Auth::EnvFile::extra_env: env-file missing or world-readable"),
            Auth::ApiKey => todo!("Auth::ApiKey is not implemented in v1"),
        }
    }

    /// Fallible variant of `extra_env` that surfaces env-file
    /// problems as `Err` rather than panicking. Production callers
    /// (the runner) should use this so missing / loose-permission
    /// env-files reach the operator log as a real error instead of
    /// crashing the dispatcher.
    pub fn try_extra_env(&self) -> Result<Vec<String>> {
        match self {
            Auth::Subscription { .. } => Ok(self.extra_env()),
            Auth::ApiKey => Err(anyhow!("Auth::ApiKey is not implemented in v1")),
            Auth::EnvFile {
                engine,
                model,
                env_file_path,
            } => {
                check_env_file_permissions(env_file_path)?;
                let raw = std::fs::read_to_string(env_file_path).with_context(|| {
                    format!(
                        "read opencode env-file at {} (did you run \
                         `bellows setup-auth --engine opencode`?)",
                        env_file_path.display(),
                    )
                })?;
                let mut env = vec![format!("BELLOWS_ENGINE={}", engine.as_name())];
                if let Some(m) = model {
                    env.push(format!("BELLOWS_MODEL={m}"));
                }
                env.extend(parse_env_file_lines(&raw)?);
                Ok(env)
            }
        }
    }
}

/// Parse a `KEY=VALUE` env-file body into a `Vec<String>` of
/// `KEY=VALUE` lines. Blank lines and `#`-prefixed comment lines are
/// skipped. Lines without an `=` produce an `Err` that names the
/// physical line number without echoing the line content, since a
/// malformed line may contain secret material.
pub fn parse_env_file_lines(body: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (index, line) in body.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !trimmed.contains('=') {
            return Err(anyhow!(
                "malformed env-file line (no `=`) at line {}; \
                 rerun `bellows setup-auth --engine opencode` to rewrite the file",
                index + 1,
            ));
        }
        out.push(trimmed.to_string());
    }
    Ok(out)
}

/// Reject env-files that are readable by other host users. Defence-
/// in-depth on top of `setup-auth`'s 0600 write — an operator (or
/// backup-restore) may have loosened the perms between setup and
/// dispatch. Refusing to read the file is safer than shipping the
/// API key to a container while the key sits on disk under permissive
/// perms.
#[cfg(unix)]
fn check_env_file_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path)
        .with_context(|| format!("stat opencode env-file at {}", path.display()))?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(anyhow!(
            "opencode env-file at {} is mode 0{:o}; refuse to read (must be 0600). \
             Re-run `bellows setup-auth --engine opencode` to rewrite at 0600.",
            path.display(),
            mode,
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_env_file_permissions(_path: &std::path::Path) -> Result<()> {
    Ok(())
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

    #[test]
    fn apikey_try_extra_env_returns_error_instead_of_panicking() {
        let err = Auth::ApiKey
            .try_extra_env()
            .expect_err("fallible env path must report unsupported API-key auth");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Auth::ApiKey is not implemented"),
            "error should explain that API-key auth is unsupported: {msg}",
        );
    }
}
