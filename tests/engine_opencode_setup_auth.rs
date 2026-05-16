//! AC9 of issue #120: `bellows setup-auth --engine opencode` writes
//! the DeepSeek API key to a 0600-mode env-file at the configured
//! `api_key_env_file` path (default `~/.config/bellows/opencode.env`).
//! No docker container is spawned (opencode is API-key, not OAuth) —
//! the env-file is host-side and is later passed to the agent
//! container via `docker run --env-file` (AC11).
//!
//! The on-disk content is canonical `DEEPSEEK_API_KEY=<key>\n` so the
//! env-file is consumable both by `docker run --env-file` and by a
//! human inspecting it. The 0600 mode keeps the key from leaking via
//! `ls -l` to other host users (an operator running bellows under
//! their own UID).
//!
//! Pure helper `write_opencode_env_file(path, api_key)` lets these
//! tests pin the on-disk shape without driving stdin.

use std::os::unix::fs::PermissionsExt;

use bellows::main_helpers::write_opencode_env_file;

#[test]
fn write_opencode_env_file_writes_canonical_key_value_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("opencode.env");
    write_opencode_env_file(&path, "sk-test-deepseek-key").expect("write");
    let raw = std::fs::read_to_string(&path).expect("read");
    assert_eq!(
        raw, "DEEPSEEK_API_KEY=sk-test-deepseek-key\n",
        "the env-file must contain exactly one canonical KEY=VALUE line",
    );
}

#[test]
fn write_opencode_env_file_sets_mode_0600() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("opencode.env");
    write_opencode_env_file(&path, "sk-mode-test").expect("write");
    let meta = std::fs::metadata(&path).expect("metadata");
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "the env-file must be 0600 so other host users cannot read the API key",
    );
}

#[test]
fn write_opencode_env_file_creates_parent_directory_if_missing() {
    // The default path lives at `~/.config/bellows/opencode.env`. A
    // fresh bellows install may not have `~/.config/bellows` yet; the
    // helper must `mkdir -p` the parent (same shape as the credentials
    // volume helpers).
    let dir = tempfile::tempdir().expect("tempdir");
    let nested = dir.path().join("nested").join("dir").join("opencode.env");
    write_opencode_env_file(&nested, "sk-nested").expect("write");
    assert!(nested.exists());
}

#[test]
fn write_opencode_env_file_overwrites_existing_file() {
    // Re-running setup-auth must replace the previous key, not
    // append. The on-disk shape stays exactly one KEY=VALUE line.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("opencode.env");
    write_opencode_env_file(&path, "sk-first").expect("write 1");
    write_opencode_env_file(&path, "sk-second").expect("write 2");
    let raw = std::fs::read_to_string(&path).expect("read");
    assert_eq!(raw, "DEEPSEEK_API_KEY=sk-second\n");
}

#[test]
fn write_opencode_env_file_rejects_empty_api_key() {
    // An empty key is operator error — surface it loudly rather than
    // write a `DEEPSEEK_API_KEY=\n` line that opencode would later
    // reject with a confusing auth error.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("opencode.env");
    let err = write_opencode_env_file(&path, "").expect_err("empty key must error");
    let msg = format!("{err:#}");
    assert!(
        msg.to_lowercase().contains("empty"),
        "error must mention `empty`: {msg}",
    );
    assert!(
        !path.exists(),
        "no env-file should be written when the key is empty",
    );
}

#[test]
fn write_opencode_env_file_trims_whitespace_around_key() {
    // Operator paste from a terminal often picks up trailing whitespace
    // / newlines. The on-disk shape must be the trimmed key only.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("opencode.env");
    write_opencode_env_file(&path, "  sk-trim-me  \n").expect("write");
    let raw = std::fs::read_to_string(&path).expect("read");
    assert_eq!(raw, "DEEPSEEK_API_KEY=sk-trim-me\n");
}
