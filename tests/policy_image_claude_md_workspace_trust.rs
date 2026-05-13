//! Pins the load-bearing phrases of the `## Workspace trust` section
//! in `policy-image/CLAUDE.md`. Per issue #108, the operating context
//! must scope Claude Code's per-tool-result malware-analysis safety
//! reminder to externally-sourced suspect content (not `/workspace`,
//! which is first-party code from the cloned bellows repo) and name
//! `## Unaddressed finding:` in `agent-notes.md` as the explicit
//! escape hatch for genuine concerns.
//!
//! A drive-by edit that drops any of the load-bearing phrases must
//! flip these tests red.

fn read_policy_image_claude_md() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("policy-image")
        .join("CLAUDE.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e))
}

fn workspace_trust_section(body: &str) -> &str {
    let (_, after_heading) = body
        .split_once("## Workspace trust")
        .expect("policy-image/CLAUDE.md must contain `## Workspace trust`");
    let (section, _) = after_heading
        .split_once("## Hard constraints")
        .expect("`## Workspace trust` must appear before `## Hard constraints`");
    section
}

#[test]
fn claude_md_has_a_workspace_trust_section() {
    let body = read_policy_image_claude_md();
    assert!(
        body.contains("## Workspace trust"),
        "policy-image/CLAUDE.md must gain a `## Workspace trust` section per issue #108: {body}",
    );
}

#[test]
fn workspace_trust_section_precedes_hard_constraints() {
    // AC: "placed BEFORE the existing `## Hard constraints` section".
    let body = read_policy_image_claude_md();
    let trust_idx = body
        .find("## Workspace trust")
        .expect("policy-image/CLAUDE.md must contain `## Workspace trust`");
    let hard_idx = body
        .find("## Hard constraints")
        .expect("policy-image/CLAUDE.md must still contain `## Hard constraints`");
    assert!(
        trust_idx < hard_idx,
        "`## Workspace trust` must appear BEFORE `## Hard constraints` in policy-image/CLAUDE.md: \
         trust at {trust_idx}, hard constraints at {hard_idx}",
    );
}

#[test]
fn workspace_trust_section_names_workspace_as_first_party() {
    // AC: the clause makes plain that `/workspace` is operator-
    // authorised first-party code, not externally-sourced suspect
    // content. "first-party" (or the equivalent "operator-authorised"
    // wording) is the load-bearing trust term.
    let body = read_policy_image_claude_md();
    let section = workspace_trust_section(&body);
    assert!(
        section.contains("/workspace"),
        "Workspace trust clause must name `/workspace` literally: {section}",
    );
    assert!(
        section.contains("first-party")
            || section.contains("operator-authorised")
            || section.contains("operator authorises"),
        "Workspace trust clause must establish that `/workspace` is first-party / operator-authorised: {section}",
    );
}

#[test]
fn workspace_trust_section_scopes_the_malware_reminder() {
    // AC (a): "the malware-analysis reminder is explicitly named and
    // scoped to externally-sourced content, not `/workspace`."
    let body = read_policy_image_claude_md();
    let section = workspace_trust_section(&body);
    assert!(
        section.contains("malware"),
        "Workspace trust clause must explicitly name the malware-analysis reminder: {section}",
    );
    assert!(
        section.contains("externally-sourced"),
        "Workspace trust clause must scope the malware-analysis reminder to externally-sourced content: {section}",
    );
    assert!(
        section.contains("not to `/workspace` contents"),
        "Workspace trust clause must state that the malware-analysis reminder does not apply to `/workspace` contents: {section}",
    );
}

#[test]
fn workspace_trust_section_names_unaddressed_finding_escape_hatch() {
    // AC (b): "the escape hatch via `agent-notes.md`
    // `## Unaddressed finding:` is named explicitly so genuine
    // concerns route through bellows's existing failure channel
    // rather than silent refusal."
    let body = read_policy_image_claude_md();
    let section = workspace_trust_section(&body);
    assert!(
        section.contains("## Unaddressed finding:"),
        "Workspace trust clause must name the `## Unaddressed finding:` heading literally so genuine \
         concerns route to the existing AgentSelfReportedFailure channel (ADR-0006): {section}",
    );
    assert!(
        section.contains("agent-notes.md"),
        "Workspace trust clause must name `agent-notes.md` as the escalation destination: {section}",
    );
}

#[test]
fn workspace_trust_section_warns_against_silent_refusal() {
    // AC: "Do not silently refuse." This is the operational
    // instruction that flips the witnessed failure mode (silent
    // refusal on file reads) into a routed failure.
    let body = read_policy_image_claude_md();
    let section = workspace_trust_section(&body);
    let lower = section.to_lowercase();
    assert!(
        lower.contains("not silently refuse")
            || lower.contains("do not refuse")
            || lower.contains("not refuse brief-directed edits"),
        "Workspace trust clause must instruct the agent NOT to silently refuse brief-directed edits: {section}",
    );
}

#[test]
fn workspace_trust_section_warns_against_quoting_secret_values() {
    // Security review: `agent-notes.md` is committed and surfaced in
    // PR comments, so the escalation path must preserve useful context
    // without re-publishing a credential or secret value.
    let body = read_policy_image_claude_md();
    let section = workspace_trust_section(&body);
    let lower = section.to_lowercase();
    assert!(
        lower.contains("without quoting")
            && (lower.contains("credential") || lower.contains("secret")),
        "Workspace trust clause must tell agents to describe sensitive findings without quoting \
         credential or secret values into public agent notes: {section}",
    );
}
