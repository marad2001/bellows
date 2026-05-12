//! Integration tests pinning the load-bearing content of
//! `docs/adr/0005-multi-engine-support.md`.
//!
//! ADR-0005 is the contract that the multi-engine feature slices
//! (#81 chain-walking + persisted state, #82 implementation) read
//! before they touch code. The tests below pin each acceptance
//! criterion from issue #80's brief to a checkable assertion against
//! the document's body, so the ADR can evolve in prose without
//! silently losing a load-bearing decision.
//!
//! Shape mirrors `tests/readme.rs`: assert the load-bearing nouns and
//! phrases (chain names, label strings, env-var names, file paths)
//! are present rather than verbatim sentences, so the prose around
//! them can flex without breaking the tests.

use std::fs;
use std::path::PathBuf;

fn adr_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("adr")
        .join("0005-multi-engine-support.md")
}

fn read_adr() -> String {
    fs::read_to_string(adr_path()).unwrap_or_else(|e| {
        panic!(
            "ADR-0005 must exist at {}: {}",
            adr_path().display(),
            e,
        )
    })
}

fn assert_contains_all(body: &str, needles: &[&str], context: &str) {
    let mut missing = Vec::new();
    for needle in needles {
        if !body.contains(needle) {
            missing.push(*needle);
        }
    }
    assert!(
        missing.is_empty(),
        "ADR-0005 is missing {} in section {:?}: {:?}",
        if missing.len() == 1 { "value" } else { "values" },
        context,
        missing,
    );
}

/// AC#1 — `docs/adr/0005-multi-engine-support.md` exists and follows
/// the same shape as ADR-0001/0002/0003/0004.
///
/// "Same shape" in this repo means: a single H1 title sentence
/// stating the decision, a body paragraph, a `## Considered
/// alternatives` section, and a `## Consequences` section. Any
/// regression on those structural sections is caught here.
#[test]
fn adr_0005_exists_and_follows_shape_of_other_adrs() {
    let path = adr_path();
    assert!(
        path.is_file(),
        "ADR-0005 must exist at {} (the multi-engine design is the \
         contract for slices #81/#82)",
        path.display(),
    );
    let body = read_adr();
    assert!(
        !body.trim().is_empty(),
        "ADR-0005 must not be empty",
    );
    // Title line: ADRs in this repo open with an H1 line stating the
    // decision in a single sentence (no `# ADR-0005:` numbering — see
    // ADR-0001..0004 for the convention).
    let first_line = body.lines().next().unwrap_or("");
    assert!(
        first_line.starts_with("# "),
        "ADR-0005 must open with an H1 title line (got: {:?})",
        first_line,
    );
    assert_contains_all(
        &body,
        &[
            "## Considered alternatives",
            "## Consequences",
        ],
        "ADR shape (sections that ADR-0001..0004 also carry)",
    );
}
