//! Regression tests for load-bearing ADR-0006 provenance decisions.
//!
//! ADR-0006 is expected to guide a later classifier change. These
//! assertions pin the part of the design that prevents agent-writable
//! `agent-notes.md` text from becoming trusted provenance.

use std::fs;
use std::path::PathBuf;

fn adr_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("adr")
        .join("0006-agent-notes-informational-vs-escalation.md")
}

fn read_adr() -> String {
    fs::read_to_string(adr_path()).unwrap_or_else(|e| {
        panic!(
            "ADR-0006 must exist at {}: {}",
            adr_path().display(),
            e,
        )
    })
}

#[test]
fn adr_0006_requires_out_of_band_synth_provenance() {
    let body = read_adr();

    for required in [
        "structured pipeline state",
        "write site",
        "HTML comments are human-readable provenance only",
        "not trusted for routing",
        "known outside the file text",
        "byte boundaries",
    ] {
        assert!(
            body.contains(required),
            "ADR-0006 should pin provenance wording {:?}",
            required,
        );
    }

    for forbidden in [
        "recognises Bellows-authored synth material by the existing `<!-- bellows ... -->` HTML-comment marker",
        "strips marked Bellows synth blocks",
        "provenance-aware stripping of `<!-- bellows ... -->` synth blocks",
    ] {
        assert!(
            !body.contains(forbidden),
            "ADR-0006 must not rely on marker text as provenance: {:?}",
            forbidden,
        );
    }
}
