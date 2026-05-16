//! Unit tests for the `**Blocked by:**` brief-parser used by the
//! polling loop's re-loop sweep (ADR-0007). Each documented input
//! case gets its own test that asserts on the resulting parsed
//! `BlockedBySection` shape — the variant the runner branches on
//! when deciding whether to strip the `blocked-by` label.

use bellows::tracker::{parse_blocked_by_section, BlockedBySection};

const BRIEF_HEADER: &str = "## Agent Brief";

fn brief_with(blocked_by_line: &str) -> String {
    format!(
        "{header}\n\nSome preamble.\n\n{line}\n\nFurther brief body.\n",
        header = BRIEF_HEADER,
        line = blocked_by_line,
    )
}

#[test]
fn parses_single_hash_reference() {
    // **Blocked by:** #95 -> Blockers(vec![95])
    let brief = brief_with("**Blocked by:** #95");
    assert_eq!(
        parse_blocked_by_section(&brief),
        BlockedBySection::Blockers(vec![95]),
    );
}

#[test]
fn parses_comma_separated_references() {
    // **Blocked by:** #95, #96 -> Blockers(vec![95, 96])
    let brief = brief_with("**Blocked by:** #95, #96");
    assert_eq!(
        parse_blocked_by_section(&brief),
        BlockedBySection::Blockers(vec![95, 96]),
    );
}

#[test]
fn parses_reference_with_trailing_parenthetical_rationale() {
    // **Blocked by:** #95 (rationale) -> Blockers(vec![95]); the
    // annotation in parens is dropped.
    let brief = brief_with("**Blocked by:** #95 (waiting on test harness)");
    assert_eq!(
        parse_blocked_by_section(&brief),
        BlockedBySection::Blockers(vec![95]),
    );
}

#[test]
fn parses_explicit_none() {
    // **Blocked by:** None -> NoBlockers
    let brief = brief_with("**Blocked by:** None");
    assert_eq!(
        parse_blocked_by_section(&brief),
        BlockedBySection::NoBlockers,
    );
}

#[test]
fn parses_verbose_none_variant() {
    // **Blocked by:** None — can start immediately -> NoBlockers
    let brief = brief_with("**Blocked by:** None — can start immediately");
    assert_eq!(
        parse_blocked_by_section(&brief),
        BlockedBySection::NoBlockers,
    );
}

#[test]
fn missing_section_within_brief_returns_no_blockers() {
    // Brief is present but has no `**Blocked by:**` line -> NoBlockers.
    let brief = format!(
        "{header}\n\nSome body without a blocked-by line.\n",
        header = BRIEF_HEADER,
    );
    assert_eq!(
        parse_blocked_by_section(&brief),
        BlockedBySection::NoBlockers,
    );
}

#[test]
fn cross_repo_reference_is_ignored_and_dropped_to_no_blockers() {
    // **Blocked by:** owner/name#95 -> NoBlockers (logged + ignored;
    // v1 is same-repo only per ADR-0007 known limitations).
    let brief = brief_with("**Blocked by:** marad2001/other-repo#95");
    assert_eq!(
        parse_blocked_by_section(&brief),
        BlockedBySection::NoBlockers,
    );
}

#[test]
fn malformed_token_alone_drops_to_no_blockers() {
    // **Blocked by:** #NaN -> NoBlockers (the only token is
    // unparseable; the dependent should be treated as having no
    // remaining blockers).
    let brief = brief_with("**Blocked by:** #NaN");
    assert_eq!(
        parse_blocked_by_section(&brief),
        BlockedBySection::NoBlockers,
    );
}

#[test]
fn mixed_parseable_and_malformed_keeps_the_parseable_one() {
    // **Blocked by:** #95, #NaN -> Blockers(vec![95]); the malformed
    // token is logged and ignored, the parseable token stays.
    let brief = brief_with("**Blocked by:** #95, #NaN");
    assert_eq!(
        parse_blocked_by_section(&brief),
        BlockedBySection::Blockers(vec![95]),
    );
}

#[test]
fn empty_brief_string_returns_unverifiable() {
    // No `## Agent Brief` header anywhere -> Unverifiable (caller
    // leaves label in place, logs a warning naming the issue).
    let brief = "Some random comment body with no agent brief header.";
    assert_eq!(
        parse_blocked_by_section(brief),
        BlockedBySection::Unverifiable,
    );
}
