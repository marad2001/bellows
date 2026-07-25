//! Deterministic, dependency-free pre-scan of a freshly cloned
//! workspace for over-large text files (issue #161).
//!
//! Bellows runs this at clone time so the implement-phase kickoff can
//! name, per-repo and per-run, exactly which files sit over the `Read`
//! tool's ~25k-token cap. A headless agent that reads such a file whole
//! crashes the run (the cap is hardcoded and the error is fatal under
//! `claude -p`); told up front which files are large, the agent uses
//! `Grep` + ranged `Read` instead. This is the version-independent
//! backstop to the claude-code bump in #162 — it ships with the bellows
//! binary and needs no policy-image rebuild.
//!
//! Pure and synchronous: a `std::fs` recursion with no `walkdir`
//! dependency (the crate has none and this does not warrant adding one).

use std::path::{Path, PathBuf};

/// Estimated-token threshold above which a file is flagged. Set
/// deliberately under the ~25k `Read` cap so borderline files are
/// caught too. The estimate is `bytes / 4` (the ~4-bytes-per-token rule
/// of thumb — no tokenizer dependency), so a file is flagged when it
/// exceeds `LARGE_FILE_TOKEN_THRESHOLD * 4` bytes (80_000).
pub const LARGE_FILE_TOKEN_THRESHOLD: u64 = 20_000;

/// A single over-large file discovered by [`scan_large_files`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargeFile {
    /// Path relative to the scan root (the workspace clone dir).
    pub path: PathBuf,
    /// Size in bytes, from filesystem metadata.
    pub bytes: u64,
    /// `bytes / 4` — the rule-of-thumb token estimate.
    pub estimated_tokens: u64,
}

/// Directory names skipped wholesale: version-control internals and
/// canonical build-output trees. `.git/` is excluded here rather than by
/// a blanket dot-directory skip precisely so `.github/` (legitimately
/// interesting) is still walked.
const SKIP_DIRS: [&str; 3] = [".git", "target", "node_modules"];

/// How many leading bytes to inspect for a NUL (binary) marker.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// Walk `root` and return every text file whose estimated token count
/// exceeds [`LARGE_FILE_TOKEN_THRESHOLD`].
///
/// Excludes `.git/`, `target/`, `node_modules/`, and any file with a
/// NUL byte in its first 8 KiB (treated as binary). Does not follow
/// symlinks (cycle safety). The result is sorted deterministically —
/// descending by `bytes`, ties broken lexicographically by path — so
/// two scans of the same tree return an identical `Vec`, which is what
/// makes both the tests and the operator log reproducible.
pub fn scan_large_files(root: &Path) -> Vec<LargeFile> {
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<LargeFile>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // An unreadable directory is skipped rather than aborting the
        // whole scan — a missing permission on one subtree must not cost
        // the operator the guidance for the rest of the repo.
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        // Do not follow symlinks (cycle safety): neither descend into a
        // symlinked directory nor stat a symlinked file.
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            let skip = entry
                .file_name()
                .to_str()
                .is_some_and(|name| SKIP_DIRS.contains(&name));
            if !skip {
                walk(root, &path, out);
            }
        } else if file_type.is_file() {
            consider_file(root, &entry, &path, out);
        }
    }
}

fn consider_file(root: &Path, entry: &std::fs::DirEntry, path: &Path, out: &mut Vec<LargeFile>) {
    let bytes = match entry.metadata() {
        Ok(md) => md.len(),
        Err(_) => return,
    };
    let estimated_tokens = bytes / 4;
    if estimated_tokens <= LARGE_FILE_TOKEN_THRESHOLD {
        return;
    }
    // Only sniff for binary content once the file is known to be large,
    // so the read is paid for on the handful of candidate files, not
    // every file in the tree.
    if looks_binary(path) {
        return;
    }
    let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
    out.push(LargeFile {
        path: rel,
        bytes,
        estimated_tokens,
    });
}

/// Whether the first 8 KiB of `path` contains a NUL byte. A file we
/// cannot open is treated as binary (skipped): the pre-scan exists to
/// steer the agent toward files it can usefully `Grep`, and an
/// unreadable file is not one of those.
fn looks_binary(path: &Path) -> bool {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return true,
    };
    let mut buf = [0u8; BINARY_SNIFF_BYTES];
    match file.read(&mut buf) {
        Ok(n) => buf[..n].contains(&0),
        Err(_) => true,
    }
}
