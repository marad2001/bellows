use bollard::models::{Mount, MountType};

/// Where Claude Code's home directory ends up inside the agent container.
/// The credentials volume mounts here; the policy-image entrypoint copies
/// the baked skills + CLAUDE.md into the same path at container start so
/// they layer with the persisted OAuth tokens.
pub const CLAUDE_HOME_IN_CONTAINER: &str = "/root/.claude";

pub enum Auth {
    Subscription { credentials_volume_name: String },
    ApiKey,
}

impl Auth {
    pub fn extra_mounts(&self) -> Vec<Mount> {
        match self {
            Auth::Subscription {
                credentials_volume_name,
            } => vec![Mount {
                target: Some(CLAUDE_HOME_IN_CONTAINER.to_string()),
                source: Some(credentials_volume_name.clone()),
                typ: Some(MountType::VOLUME),
                ..Default::default()
            }],
            Auth::ApiKey => todo!("Auth::ApiKey is not implemented in v1"),
        }
    }

    pub fn extra_env(&self) -> Vec<String> {
        match self {
            Auth::Subscription { .. } => Vec::new(),
            Auth::ApiKey => todo!("Auth::ApiKey is not implemented in v1"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_returns_volume_mount_pointing_at_credentials_volume() {
        let auth = Auth::Subscription {
            credentials_volume_name: "my-creds".to_string(),
        };
        let mounts = auth.extra_mounts();
        assert_eq!(mounts.len(), 1);
        let m = &mounts[0];
        assert_eq!(m.typ, Some(MountType::VOLUME));
        assert_eq!(m.source.as_deref(), Some("my-creds"));
        assert_eq!(m.target.as_deref(), Some(CLAUDE_HOME_IN_CONTAINER));
    }

    #[test]
    fn subscription_returns_no_extra_env() {
        let auth = Auth::Subscription {
            credentials_volume_name: "any".to_string(),
        };
        assert!(auth.extra_env().is_empty());
    }
}
