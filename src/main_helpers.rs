//! Helpers that the `bellows` binary uses and that integration tests
//! need to reach. Surfacing them through `pub mod main_helpers;` in
//! `lib.rs` lets the inline binary tests stay close to the binary code
//! while still letting `tests/` integration tests pin the same
//! contracts.
//!
//! Issue #120 AC9: opencode setup-auth helpers.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// Expand a leading `~/` in a path string into the operator's
/// `$HOME`. Other paths pass through verbatim. Used for paths
/// surfaced in operator-facing config (e.g. the opencode
/// `api_key_env_file` default `~/.config/bellows/opencode.env`).
pub fn expand_tilde_path(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(raw)
}

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
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create env-file parent directory at {}", parent.display()))?;
    }
    let body = format!("DEEPSEEK_API_KEY={trimmed}\n");
    write_env_file_atomically_0600(path, body.as_bytes())
        .with_context(|| format!("write opencode env-file at {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn write_env_file_atomically_0600(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::ffi::OsString;
    use std::io::{Error, ErrorKind, Write};
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            "opencode env-file path must include a file name",
        )
    })?;

    use std::os::unix::fs::PermissionsExt;

    for _ in 0..16 {
        let mut temp_name = OsString::from(".");
        temp_name.push(file_name);
        temp_name.push(".");
        temp_name.push(uuid::Uuid::new_v4().to_string());
        temp_name.push(".tmp");
        let temp_path = parent.join(temp_name);

        let mut temp_file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp_path)
        {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        };

        let result = (|| {
            temp_file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            temp_file.write_all(body)?;
            temp_file.sync_all()?;
            std::fs::rename(&temp_path, path)
        })();

        if result.is_err() {
            let _ = std::fs::remove_file(&temp_path);
        }
        return result;
    }

    Err(Error::new(
        ErrorKind::AlreadyExists,
        "could not create unique opencode env-file temporary path",
    ))
}

#[cfg(not(unix))]
fn write_env_file_atomically_0600(path: &Path, body: &[u8]) -> std::io::Result<()> {
    // Non-unix targets do not have POSIX permission bits in this
    // shape; bellows only ships unix builds, so this is a no-op for
    // cross-platform compile cleanliness.
    std::fs::write(path, body)
}
