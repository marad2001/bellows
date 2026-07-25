//! Tests for the bellows-side large-file pre-scan (issue #161).
//!
//! The scanner walks a freshly cloned workspace and reports every text
//! file whose estimated token count (`bytes / 4`) exceeds
//! `LARGE_FILE_TOKEN_THRESHOLD`, so the implement kickoff can name those
//! files up front and steer the agent to `Grep` + ranged `Read` instead
//! of a whole-file read that would crash a headless run.

use std::fs;
use std::path::{Path, PathBuf};

use bellows::large_files::{scan_large_files, LargeFile, LARGE_FILE_TOKEN_THRESHOLD};
use tempfile::TempDir;

/// Bytes that estimate to just over the threshold (`bytes / 4 > 20_000`
/// ⇒ `bytes > 80_000`). 84_000 bytes ⇒ ~21_000 tokens.
const OVER_THRESHOLD_BYTES: usize = 84_000;
/// Bytes that estimate to under the threshold. 40_000 bytes ⇒ ~10_000
/// tokens, comfortably below the 20k flag.
const UNDER_THRESHOLD_BYTES: usize = 40_000;

fn write_file(root: &Path, rel: &str, bytes: usize) {
    let abs = root.join(rel);
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    // ASCII filler so the file is unambiguously text (no NUL bytes).
    fs::write(&abs, "a".repeat(bytes)).unwrap();
}

fn paths(files: &[LargeFile]) -> Vec<PathBuf> {
    files.iter().map(|f| f.path.clone()).collect()
}

#[test]
fn over_threshold_files_are_reported_under_threshold_are_not() {
    // AC1: files over the threshold come back with repo-relative paths
    // and estimated token counts; files under it do not.
    let tmp = TempDir::new().unwrap();
    write_file(tmp.path(), "big.rs", OVER_THRESHOLD_BYTES);
    write_file(tmp.path(), "small.rs", UNDER_THRESHOLD_BYTES);

    let found = scan_large_files(tmp.path());

    assert_eq!(paths(&found), vec![PathBuf::from("big.rs")]);
    let big = &found[0];
    assert_eq!(big.path, PathBuf::from("big.rs"));
    assert_eq!(big.bytes, OVER_THRESHOLD_BYTES as u64);
    // Token estimate is bytes / 4, and it must clear the threshold.
    assert_eq!(big.estimated_tokens, OVER_THRESHOLD_BYTES as u64 / 4);
    assert!(big.estimated_tokens > LARGE_FILE_TOKEN_THRESHOLD);
}

#[test]
fn nested_paths_are_reported_relative_to_the_scan_root() {
    // AC1 (path shape): a file in a subdirectory is reported with its
    // repo-relative path, not an absolute one.
    let tmp = TempDir::new().unwrap();
    write_file(tmp.path(), "src/deep/module.rs", OVER_THRESHOLD_BYTES);

    let found = scan_large_files(tmp.path());

    assert_eq!(paths(&found), vec![PathBuf::from("src/deep/module.rs")]);
}

#[test]
fn build_and_vcs_dirs_and_binaries_are_excluded() {
    // AC2: contents of .git/, target/ and node_modules/, plus
    // NUL-containing binary files, are excluded even when over the
    // threshold.
    let tmp = TempDir::new().unwrap();
    write_file(tmp.path(), ".git/objects/pack.idx", OVER_THRESHOLD_BYTES);
    write_file(tmp.path(), "target/debug/build.log", OVER_THRESHOLD_BYTES);
    write_file(tmp.path(), "node_modules/dep/index.js", OVER_THRESHOLD_BYTES);

    // A large binary file: NUL byte within the first 8 KiB.
    let mut binary = vec![b'a'; OVER_THRESHOLD_BYTES];
    binary[10] = 0;
    fs::write(tmp.path().join("blob.bin"), &binary).unwrap();

    // A legitimately-interesting large file that must survive.
    write_file(tmp.path(), "src/real.rs", OVER_THRESHOLD_BYTES);

    let found = scan_large_files(tmp.path());

    assert_eq!(paths(&found), vec![PathBuf::from("src/real.rs")]);
}

#[test]
fn dot_github_is_not_blanket_skipped() {
    // AC2 corollary: dot-directories are not blanket-skipped — only
    // .git/ is excluded. A large file under .github/ is legitimately
    // interesting and must be reported.
    let tmp = TempDir::new().unwrap();
    write_file(tmp.path(), ".github/workflows/huge.yml", OVER_THRESHOLD_BYTES);

    let found = scan_large_files(tmp.path());

    assert_eq!(paths(&found), vec![PathBuf::from(".github/workflows/huge.yml")]);
}

#[test]
fn ordering_is_descending_by_size_with_lexicographic_tiebreak() {
    // AC3: ordering is descending-by-size with a lexicographic path
    // tiebreak, so two scans of the same tree return an identical Vec.
    let tmp = TempDir::new().unwrap();
    write_file(tmp.path(), "medium.rs", OVER_THRESHOLD_BYTES + 1_000);
    write_file(tmp.path(), "largest.rs", OVER_THRESHOLD_BYTES + 5_000);
    // Two files of identical size — the tiebreak must order them by path.
    write_file(tmp.path(), "z_tie.rs", OVER_THRESHOLD_BYTES);
    write_file(tmp.path(), "a_tie.rs", OVER_THRESHOLD_BYTES);

    let first = scan_large_files(tmp.path());
    let second = scan_large_files(tmp.path());

    assert_eq!(
        paths(&first),
        vec![
            PathBuf::from("largest.rs"),
            PathBuf::from("medium.rs"),
            PathBuf::from("a_tie.rs"),
            PathBuf::from("z_tie.rs"),
        ],
    );
    // Determinism: a second scan of the same tree is byte-identical.
    assert_eq!(first, second);
}
