//! Helpers that the `bellows` binary uses and that integration tests
//! need to reach. Surfacing them through `pub mod main_helpers;` in
//! `lib.rs` lets the inline binary tests stay close to the binary code
//! while still letting `tests/` integration tests pin the same
//! contracts.
//!
//! Issue #120 AC9: opencode setup-auth helpers.

use std::path::Path;

use anyhow::{anyhow, Context, Result};

/// Write the DeepSeek API key to `path` as a single canonical
/// `DEEPSEEK_API_KEY=<key>\n` line, with permissions 0600 so other
/// host users can't read the key via `ls -l`. Creates the parent
/// directory if missing. Overwrites the file if it exists.
///
/// Trims surrounding whitespace from the API key (terminal paste
/// often picks up trailing newlines). Refuses to write an empty
/// key so the on-disk shape never becomes `DEEPSEEK_API_KEY=\n`,
/// which opencode would later reject with a confusing auth error.
///
/// ADR-0008 / issue #120 AC9.
pub fn write_opencode_env_file(path: &Path, api_key: &str) -> Result<()> {
    let trimmed = api_key.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(
            "refusing to write empty DEEPSEEK_API_KEY to env-file at {}",
            path.display(),
        ));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("create env-file parent directory at {}", parent.display())
            })?;
        }
    }
    let body = format!("DEEPSEEK_API_KEY={trimmed}\n");
    std::fs::write(path, body)
        .with_context(|| format!("write opencode env-file at {}", path.display()))?;
    set_mode_0600(path)
        .with_context(|| format!("chmod 0600 on env-file at {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn set_mode_0600(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn set_mode_0600(_path: &Path) -> std::io::Result<()> {
    // Non-unix targets do not have POSIX permission bits in this
    // shape; bellows only ships unix builds, so this is a no-op for
    // cross-platform compile cleanliness.
    Ok(())
}
