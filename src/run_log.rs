//! Issue #195: timestamp discipline for `bellows.log`.
//!
//! Everything bellows narrates about a run — claims, phase transitions,
//! engine picks, advances, gate results, classifications, errors — used
//! to be written as bare text, while the engine and cargo output teed
//! around it carried timestamps from the tools that produced it. The
//! orchestrator's own narration was the only part of its own log that
//! could not be placed in time.
//!
//! ## What gets stamped
//!
//! Narration, and only narration. The container tee streams the agent's
//! and cargo's stdout verbatim through the same writer, and that output
//! is the bulk of the file — stamping it would inflate a 31 MB log by
//! roughly half, and would prefix every line of every diff and code
//! block the agent emits. Log size is explicitly out of scope for this
//! work, so the tee stays verbatim.
//!
//! ## Continuation lines
//!
//! A single logical event is often several physical lines: the
//! `caused by:` chain under an error, the resolved clippy/test commands
//! under a gate announcement, the large-file pre-scan under a phase
//! header. Those are all written indented, so the rule is simply that a
//! line beginning with whitespace is a continuation and passes through
//! unstamped. That keeps a multi-line event visually one event rather
//! than several competing ones.
//!
//! ## Stdout
//!
//! Stdout is what an operator watches live, where the timestamp is
//! redundant — they are reading it as it happens. The console keeps the
//! bare lines; the file always carries the stamp.

use std::io::Write;

use chrono::{DateTime, SecondsFormat, Utc};

/// Render one narration line as it should appear in `bellows.log`.
///
/// `line` may itself contain newlines — `format_error_chain` produces a
/// head line plus an indented `caused by:` chain, and the whole thing
/// arrives here as one string. The rule is applied per physical line, so
/// the head is stamped and the chain is not.
///
/// Returns the body without a trailing newline; the caller supplies it.
pub fn stamped(now: DateTime<Utc>, line: &str) -> String {
    let prefix = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    line.split('\n')
        .map(|physical| {
            if is_continuation(physical) {
                physical.to_string()
            } else {
                format!("{prefix} {physical}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a physical line continues the event above it rather than
/// starting a new one. Indented lines continue; so does a blank line,
/// which carries no event and would otherwise become a bare timestamp.
fn is_continuation(physical: &str) -> bool {
    physical.is_empty() || physical.starts_with(|c: char| c.is_whitespace())
}

/// Write one narration line to the run log, stamped.
///
/// **This is the only sanctioned way to narrate into the log file.**
/// `tests/run_log_timestamps.rs` fails the build if a `writeln!` to a
/// log writer appears anywhere else in `src/`, so a call site added
/// later cannot quietly produce an unstamped line.
///
/// Best-effort, like every other write to the run log: a failure here
/// must never change how a run ends. Flushes so an operator tailing the
/// file sees the line as it happens.
/// Generic over the writer rather than taking `&mut dyn Write`, because
/// some narration paths are themselves generic over an unsized writer
/// (`W: Write + ?Sized`) and cannot coerce to a trait object.
pub fn narrate<W: Write + ?Sized>(log_writer: &mut W, line: &str) {
    let _ = writeln!(log_writer, "{}", stamped(Utc::now(), line));
    let _ = log_writer.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn a_narration_line_is_prefixed_with_a_utc_timestamp() {
        let out = stamped(
            at("2026-07-29T11:02:03Z"),
            "bellows: claimed issue #192 — pipeline starting",
        );
        assert_eq!(
            out,
            "2026-07-29T11:02:03Z bellows: claimed issue #192 — pipeline starting",
        );
    }

    #[test]
    fn the_stamp_is_utc_regardless_of_the_input_offset() {
        // A non-UTC instant must still render as UTC with a `Z` suffix,
        // so the whole file sorts as one timeline.
        let out = stamped(at("2026-07-29T12:02:03+01:00"), "bellows: idle");
        assert_eq!(out, "2026-07-29T11:02:03Z bellows: idle");
    }

    #[test]
    fn indented_continuation_lines_are_not_stamped() {
        // The `caused by:` chain is one event, not three. Stamping each
        // line would present a single error as three separate ones.
        let out = stamped(
            at("2026-07-29T11:02:03Z"),
            "bellows: error: sandbox: docker: dropped\n    caused by: docker: dropped\n    caused by: dropped",
        );
        assert_eq!(
            out,
            "2026-07-29T11:02:03Z bellows: error: sandbox: docker: dropped\n    caused by: docker: dropped\n    caused by: dropped",
        );
    }

    #[test]
    fn gate_detail_lines_are_not_stamped() {
        // The resolved clippy/test commands are written indented under
        // the phase header and belong to it.
        let out = stamped(
            at("2026-07-29T11:02:03Z"),
            "bellows: phase 2/8 — cargo checks gate\n  clippy: cargo clippy --locked\n  test:   cargo test --locked",
        );
        let lines: Vec<&str> = out.split('\n').collect();
        assert!(lines[0].starts_with("2026-07-29T11:02:03Z bellows:"), "{out}");
        assert_eq!(lines[1], "  clippy: cargo clippy --locked");
        assert_eq!(lines[2], "  test:   cargo test --locked");
    }

    #[test]
    fn a_blank_line_does_not_become_a_bare_timestamp() {
        let out = stamped(at("2026-07-29T11:02:03Z"), "bellows: done\n\nbellows: next");
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines[1], "", "a blank line carries no event: {out}");
        assert!(lines[2].starts_with("2026-07-29T11:02:03Z "), "{out}");
    }

    #[test]
    fn the_stamp_sorts_lexicographically_in_chronological_order() {
        // The point of a fixed-width RFC3339 UTC prefix: `sort` on the
        // file is a chronological sort.
        let earlier = stamped(at("2026-07-29T09:00:00Z"), "bellows: first");
        let later = stamped(at("2026-07-29T11:00:00Z"), "bellows: second");
        assert!(earlier < later, "{earlier} should sort before {later}");
    }

    #[test]
    fn existing_operator_grep_patterns_still_match() {
        // Anchored `^bellows:` cannot survive a prefix, and no format
        // that places the timestamp first could keep it. What must keep
        // working is the unanchored search an operator actually uses to
        // find claims, finalisations and errors.
        let claimed = stamped(at("2026-07-29T11:02:03Z"), "bellows: claimed issue #192");
        let finalised = stamped(
            at("2026-07-29T11:02:03Z"),
            "bellows: finalised issue #192 -> PR #201 (Success)",
        );
        let error = stamped(at("2026-07-29T11:02:03Z"), "bellows: error: sandbox: docker");
        assert!(claimed.contains("bellows: claimed issue #"));
        assert!(finalised.contains("bellows: finalised issue #"));
        assert!(error.contains("bellows: error: "));
    }

    #[test]
    fn narrate_writes_a_stamped_line_terminated_by_a_newline() {
        let mut sink: Vec<u8> = Vec::new();
        narrate(&mut sink, "bellows: idle (no ready-for-agent issues)");
        let written = String::from_utf8(sink).expect("utf-8");
        assert!(written.ends_with('\n'), "{written:?}");
        let body = written.trim_end_matches('\n');
        assert!(
            body.ends_with(" bellows: idle (no ready-for-agent issues)"),
            "{body:?}",
        );
        // Prefix must be a parseable RFC3339 instant, not a hand-rolled
        // format that only looks like one.
        let (prefix, _) = body.split_once(' ').expect("prefix and body");
        DateTime::parse_from_rfc3339(prefix)
            .unwrap_or_else(|e| panic!("prefix {prefix:?} must be RFC3339: {e}"));
    }
}
