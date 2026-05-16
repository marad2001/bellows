//! AC10 of issue #120: `bellows refresh-auth --engine opencode`
//! re-runs the same disk-write path as `setup-auth --engine opencode`
//! (because `Command::RefreshAuth` maps to the same `setup_auth`
//! function in main.rs — there is no separate "refresh" entry point;
//! refresh-auth is just a friendlier subcommand name for the
//! re-running case). The on-disk env-file is overwritten, not
//! appended to.
//!
//! Note on commit shape: AC10 is satisfied as data by AC9's commit —
//! `write_opencode_env_file` already overwrites (one of AC9's pinned
//! behaviours). This test locks in the overwrite contract under the
//! refresh-auth name so a future change cannot silently turn refresh-
//! auth into an append.

use bellows::main_helpers::write_opencode_env_file;

#[test]
fn refresh_auth_opencode_overwrites_existing_env_file_with_new_key() {
    // Mirror the refresh-auth flow: an env-file already exists from a
    // prior setup-auth run; the operator runs `bellows refresh-auth
    // --engine opencode` to rotate the key. The on-disk shape after
    // the rotation is exactly the new key — no append, no leftover
    // line from the previous key.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("opencode.env");
    write_opencode_env_file(&path, "sk-original").expect("initial write");
    let original = std::fs::read_to_string(&path).expect("read original");
    assert_eq!(original, "DEEPSEEK_API_KEY=sk-original\n");

    // Operator re-runs setup-auth / refresh-auth with a fresh key.
    write_opencode_env_file(&path, "sk-rotated").expect("rotation write");
    let rotated = std::fs::read_to_string(&path).expect("read rotated");
    assert_eq!(
        rotated, "DEEPSEEK_API_KEY=sk-rotated\n",
        "refresh-auth must overwrite, not append, the env-file",
    );
}

#[test]
fn refresh_auth_opencode_does_not_write_new_key_into_loosened_file() {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("opencode.env");
    write_opencode_env_file(&path, "sk-original").expect("initial write");

    // Operator (or backup-restore) accidentally loosens the existing
    // env-file. A reader that could open the loosened file before
    // rotation must not observe the new key through the same file
    // descriptor after refresh-auth runs.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("loosen");
    let mut stale_reader = std::fs::File::open(&path).expect("open loose file");

    write_opencode_env_file(&path, "sk-rotated").expect("rotation write");

    stale_reader
        .seek(SeekFrom::Start(0))
        .expect("rewind stale reader");
    let mut stale_body = String::new();
    stale_reader
        .read_to_string(&mut stale_body)
        .expect("read stale reader");
    assert_eq!(
        stale_body, "DEEPSEEK_API_KEY=sk-original\n",
        "refresh-auth must replace a loose env-file instead of writing the new key into it",
    );

    let rotated = std::fs::read_to_string(&path).expect("read rotated");
    assert_eq!(rotated, "DEEPSEEK_API_KEY=sk-rotated\n");
}

#[test]
fn refresh_auth_opencode_overwrite_preserves_0600_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("opencode.env");
    write_opencode_env_file(&path, "sk-first").expect("write 1");
    // Operator (or `chmod 0644` accident) loosens permissions
    // between runs — refresh-auth must tighten them back to 0600.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("loosen");
    write_opencode_env_file(&path, "sk-second").expect("rotate");
    let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "refresh-auth must restore 0600 even if the file was loosened to 0644",
    );
}
