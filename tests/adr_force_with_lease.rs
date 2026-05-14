//! Issue #113 AC6: ADR-0003's "Consequences" section closes the
//! forward reference to "force-push as a recovery primitive at push
//! time" by naming the new `push_branch` lease policy as the
//! realisation of its separable design.
//!
//! The forward reference originated in ADR-0003's "Considered
//! alternatives" section, which rejected force-push *at the deletion
//! slice's timing* but explicitly flagged it as a separable design for
//! later. Issue #113 lands that separable design; ADR-0003's
//! "Consequences" section must now name it so a future reader walks
//! from the deletion slice's alternatives section to the realised
//! push-time policy without an external dangling pointer.

use std::path::PathBuf;

fn adr_0003_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("adr")
        .join("0003-pre-claim-stale-agent-branch-deletion.md")
}

fn read_adr_0003() -> String {
    std::fs::read_to_string(adr_0003_path()).expect("ADR-0003 must be readable")
}

/// Split ADR-0003 into its top section and its `## Consequences`
/// section so an assertion can target the Consequences section
/// specifically — a stray mention of `push_branch` anywhere else in
/// the ADR (e.g. inside `## Considered alternatives`) would not
/// satisfy the AC.
fn consequences_section() -> String {
    let body = read_adr_0003();
    let idx = body
        .find("## Consequences")
        .expect("ADR-0003 must contain a `## Consequences` section");
    body[idx..].to_string()
}

#[test]
fn adr_0003_consequences_names_push_branch_lease_policy() {
    // AC6: the Consequences section must name the new push_branch
    // lease policy as the realisation of the forward-referenced
    // "separable design" from the alternatives section. The literal
    // string `push_branch` is the canonical hook a future reader
    // grep()s for; if it doesn't appear in Consequences, the
    // cross-reference is broken.
    let consequences = consequences_section();
    assert!(
        consequences.contains("push_branch"),
        "ADR-0003 Consequences section must name `push_branch` as the realisation of the \
         force-push-at-push-time separable design (issue #113). Got: {consequences:?}"
    );
}

#[test]
fn adr_0003_consequences_names_force_with_lease_as_the_policy() {
    // AC6 (substance): the AC says the Consequences section names the
    // new push_branch *lease policy* specifically. The literal
    // `force-with-lease` substring is the canonical hook — both
    // because it is the git-side flag the policy uses and because a
    // future reader scanning Consequences for the realisation should
    // find the flag spelled out, not paraphrased.
    let consequences = consequences_section();
    assert!(
        consequences.contains("force-with-lease"),
        "ADR-0003 Consequences section must spell out `force-with-lease` so a future reader \
         scanning for the realisation of the separable design finds the canonical flag (issue \
         #113). Got: {consequences:?}"
    );
}

#[test]
fn adr_0003_alternatives_forward_reference_is_still_present() {
    // Pins the *forward* reference too. ADR-0003's "Considered
    // alternatives" section rejected force-push at the pre-claim
    // slice's timing but flagged it as a separable design for later.
    // The realisation in the Consequences section is only meaningful
    // if the forward reference still exists; if a future edit removes
    // the "separable design" sentence from alternatives, this test
    // flips red and the Consequences entry is left dangling.
    let body = read_adr_0003();
    let alternatives_idx = body
        .find("## Considered alternatives")
        .expect("ADR-0003 must contain a `## Considered alternatives` section");
    let consequences_idx = body
        .find("## Consequences")
        .expect("ADR-0003 must contain a `## Consequences` section");
    assert!(
        alternatives_idx < consequences_idx,
        "Considered alternatives must come before Consequences in ADR-0003"
    );
    let alternatives = &body[alternatives_idx..consequences_idx];
    assert!(
        alternatives.contains("separable design"),
        "ADR-0003 alternatives section must keep the `separable design` forward-reference \
         phrase that the Consequences section closes (issue #113). Got: {alternatives:?}"
    );
}
