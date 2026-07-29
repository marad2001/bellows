//! Issue #195 AC: "Applying the timestamp happens in one place. Adding
//! a new log call site later cannot produce an untimestamped line."
//!
//! A doc comment cannot enforce that; a build failure can. `run_log`
//! owns the only `writeln!` to a log writer in the tree, so a call site
//! added later either goes through `run_log::narrate` and is stamped, or
//! fails this test.
//!
//! Structural tests over the source tree are an established pattern in
//! this repo — see `tests/readme.rs` and `tests/prompt_invariants.rs`.

use std::path::{Path, PathBuf};

/// The one module allowed to write directly to a log writer.
const SANCTIONED: &str = "run_log.rs";

/// Direct writes to a log writer. `log_writer` is the parameter name
/// used by every narration path in `runner` and `sandbox` (22 signatures
/// at the time of writing), so matching it catches the realistic way a
/// new unstamped line gets added.
const FORBIDDEN_PATTERNS: [&str; 3] = [
    "writeln!(log_writer",
    "write!(log_writer",
    "log_writer.write_all",
];

/// Opt-out marker for a write that is deliberately raw. Today there is
/// exactly one — the container tee, which relays the agent's and cargo's
/// own stdout verbatim and must not be prefixed. A future raw write has
/// to say so in a comment directly above it, which makes the carve-out a
/// decision on the record rather than a gap in the pattern.
const RAW_MARKER: &str = "run-log-raw:";

fn strip_ws(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            out.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out
}

#[test]
fn only_run_log_writes_directly_to_a_log_writer() {
    let mut offenders: Vec<String> = Vec::new();

    for path in rust_sources(&src_dir()) {
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if file_name == SANCTIONED {
            continue;
        }
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

        let lines: Vec<&str> = body.lines().collect();
        let mut last_offender_line: Option<usize> = None;
        for (idx, line) in lines.iter().enumerate() {
            // Skip prose about the rule, so this file's own doc
            // comments and the module's don't read as violations.
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            // rustfmt splits a long macro call across lines, so
            // `writeln!(` and `log_writer,` routinely land on separate
            // ones — the shape most narration writes in `runner` and
            // `sandbox` actually took. Match against a whitespace-
            // stripped window, so how a call is formatted cannot decide
            // whether it is caught. A single-line scan missed 35 of
            // them on the first attempt at this issue.
            let window: String = lines[idx..lines.len().min(idx + 3)]
                .iter()
                .flat_map(|l| l.chars())
                .filter(|c| !c.is_whitespace())
                .collect();
            if !FORBIDDEN_PATTERNS
                .iter()
                .any(|p| window.contains(&strip_ws(p)))
            {
                continue;
            }
            // A raw write must be justified in the comment block
            // immediately above it. Scan back over contiguous comment
            // lines looking for the marker.
            let justified = lines[..idx]
                .iter()
                .rev()
                .take_while(|prior| prior.trim_start().starts_with("//"))
                .any(|prior| prior.contains(RAW_MARKER));
            if !justified {
                // A 3-line window means one multi-line write matches on
                // consecutive starting lines. Report it once.
                let contiguous =
                    last_offender_line.is_some_and(|prev: usize| idx.saturating_sub(prev) < 3);
                if !contiguous {
                    offenders.push(format!("{}:{}: {}", file_name, idx + 1, line.trim()));
                }
                last_offender_line = Some(idx);
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "narration must go through `run_log::narrate` so it carries a \
         timestamp (issue #195). Found {} direct write(s) to a log \
         writer outside `{}`:\n  {}",
        offenders.len(),
        SANCTIONED,
        offenders.join("\n  "),
    );
}

#[test]
fn the_sanctioned_module_actually_stamps() {
    // Guards against the previous test being satisfied by a `run_log`
    // that forwards without stamping — the invariant is "one place",
    // and this pins that the one place does the job.
    let body = std::fs::read_to_string(src_dir().join(SANCTIONED))
        .expect("read run_log.rs");
    assert!(
        body.contains("pub fn narrate"),
        "run_log must expose `narrate` as the narration entry point",
    );
    assert!(
        body.contains("stamped(Utc::now()"),
        "run_log::narrate must stamp the line it writes",
    );
}
