//! Pins the `## Where to write things down` section of
//! `policy-image/CLAUDE.md` (issue #178).
//!
//! Before this section existed the operating context gave the agent
//! exactly one place to record anything: `bellows-agent-notes.md`. That
//! file is ephemeral by design — per ADR-0006 Bellows captures it, posts
//! it as a PR comment, removes it from the workspace and commits the
//! deletion before the final push — so a durable fact an agent worked
//! out about the repo it was in had nowhere to land, and the next run
//! rediscovered it from scratch.
//!
//! The section names both destinations, makes the ephemeral-vs-durable
//! contrast explicit, and states a deliberately high bar for writing to
//! the target repo's own context file. The bar is the load-bearing part:
//! without it every PR grows a speculative `CLAUDE.md` edit and the
//! repo's context file rots, which is exactly the failure this route was
//! chosen to avoid.
//!
//! The section must also survive the codex path. `src/policy.rs`
//! `include_str!`s this same file as `CODEX_INLINED_OPERATING_CONTEXT`
//! and runs it through `neutralise_claude_phrasing_for_codex` before
//! inlining it into every codex kickoff, so the wording must not depend
//! on phrasing that pass rewrites, and it must not tell a codex agent to
//! write to a file codex will never read back.
//!
//! A drive-by edit that drops any of the load-bearing phrases must flip
//! these tests red.

use bellows::config::Engine;
use bellows::policy::{wrap_phase_prompt_for_engine, Phase};

const SECTION_HEADING: &str = "## Where to write things down";

/// The next `## ` heading after the new section in `policy-image/CLAUDE.md`.
/// Pinned so the extractor terminates at the same place in the raw file
/// and in the codex-rendered kickoff, which appends baked-skill headings
/// after the operating-context body.
const NEXT_HEADING: &str = "## What Bellows does after you exit";

const BODY: &str = "PHASE BODY SENTINEL\n";

fn read_policy_image_claude_md() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("policy-image")
        .join("CLAUDE.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e))
}

/// The operating context as the given engine actually receives it.
///
/// Claude and opencode auto-discover the baked `CLAUDE.md` from disk, so
/// the rendered context for them *is* the file. Codex has no equivalent
/// discovery mechanism (ADR-0005), so its context is whatever
/// `wrap_phase_prompt_for_engine` prepends to the phase body — including
/// the neutralisation pass.
fn rendered_operating_context(engine: Engine) -> String {
    match engine {
        Engine::Claude | Engine::Opencode => read_policy_image_claude_md(),
        Engine::Codex => wrap_phase_prompt_for_engine(engine, Phase::Implement, BODY),
    }
}

fn where_to_write_section(body: &str) -> &str {
    let (_, after_heading) = body.split_once(SECTION_HEADING).unwrap_or_else(|| {
        panic!("operating context must contain `{SECTION_HEADING}`: {body}")
    });
    let (section, _) = after_heading.split_once(NEXT_HEADING).unwrap_or_else(|| {
        panic!("`{SECTION_HEADING}` must be followed by `{NEXT_HEADING}`: {after_heading}")
    });
    section
}

#[test]
fn claude_md_has_a_where_to_write_things_down_section() {
    let body = read_policy_image_claude_md();
    assert!(
        body.contains(SECTION_HEADING),
        "policy-image/CLAUDE.md must gain a `{SECTION_HEADING}` section per issue #178: {body}",
    );
}

#[test]
fn rendered_operating_context_for_claude_contains_durable_findings_guidance() {
    // AC: "the rendered operating context for `Engine::Claude` contains
    // the durable-findings guidance."
    let rendered = rendered_operating_context(Engine::Claude);
    let section = where_to_write_section(&rendered);
    let lower = section.to_lowercase();
    assert!(
        lower.contains("bellows-agent-notes.md"),
        "Claude's operating context must name `bellows-agent-notes.md` as the run-scoped \
         destination: {section}",
    );
    assert!(
        lower.contains("claude.md"),
        "Claude's operating context must name the target repo's own `CLAUDE.md` as the durable \
         destination: {section}",
    );
}

#[test]
fn rendered_operating_context_for_codex_contains_durable_findings_guidance() {
    // AC: "the rendered operating context for `Engine::Codex` also
    // contains it". Codex gets the operating-context body inlined into
    // the kickoff rather than discovered from disk, so the section has to
    // survive `wrap_phase_prompt_for_engine`.
    let rendered = rendered_operating_context(Engine::Codex);
    let section = where_to_write_section(&rendered);
    let lower = section.to_lowercase();
    assert!(
        lower.contains("bellows-agent-notes.md") && lower.contains("claude.md"),
        "Codex's inlined operating context must carry the durable-findings guidance naming both \
         destinations: {section}",
    );
    assert!(
        rendered.contains(BODY),
        "The codex wrapper must still append the phase body after the operating context",
    );
}

#[test]
fn codex_neutralisation_leaves_the_section_untouched() {
    // AC: "the neutralisation pass leaves it coherent". The strongest
    // form of that: the section codex receives is byte-identical to the
    // section in the source file, i.e. no phrase in it collides with a
    // `neutralise_claude_phrasing_for_codex` rewrite (notably the
    // "Claude Code" -> "the agent" replacement, which would turn a
    // sentence about which engines read `CLAUDE.md` into nonsense).
    let source = read_policy_image_claude_md();
    let rendered = rendered_operating_context(Engine::Codex);
    assert_eq!(
        where_to_write_section(&source),
        where_to_write_section(&rendered),
        "The `{SECTION_HEADING}` section must pass through \
         `neutralise_claude_phrasing_for_codex` unchanged — reword it so it does not contain a \
         phrase that pass rewrites",
    );
}

#[test]
fn guidance_routes_each_engine_to_a_context_file_it_will_read() {
    // AC: "no dangling reference to a file codex will not read." Codex
    // discovers `AGENTS.md`, not `CLAUDE.md`, so merely naming both files
    // is insufficient: the instruction must unambiguously route each
    // active engine to a file that engine will discover on its next run.
    let rendered = rendered_operating_context(Engine::Codex);
    let section = where_to_write_section(&rendered);
    let lower = section.to_lowercase();
    assert!(
        lower.contains(
            "if the active engine is codex, update or create `/workspace/agents.md`"
        ),
        "The section must route codex specifically to `AGENTS.md`, even when the repo already \
         keeps only `CLAUDE.md`: {section}",
    );
    assert!(
        lower.contains(
            "if the active engine is claude, update or create `/workspace/claude.md`"
        ),
        "The section must route claude specifically to `CLAUDE.md`: {section}",
    );
    assert!(
        lower.contains(
            "if the active engine is opencode, update the first existing local context file in \
             its discovery order (`/workspace/agents.md`, then `/workspace/claude.md`); if neither \
             exists, create `/workspace/agents.md`"
        ),
        "The section must route opencode according to its documented local discovery order: \
         {section}",
    );
}

#[test]
fn guidance_states_the_notes_file_is_deleted_before_the_push() {
    // AC: "the guidance explicitly states that `bellows-agent-notes.md`
    // is deleted before the push, so the contrast is unambiguous."
    let body = read_policy_image_claude_md();
    let section = where_to_write_section(&body);
    let lower = section.to_lowercase();
    assert!(
        lower.contains("delet") && lower.contains("before the push"),
        "The section must state explicitly that `bellows-agent-notes.md` is deleted before the \
         push (ADR-0006), so the ephemeral-vs-durable contrast is unambiguous: {section}",
    );
}

#[test]
fn guidance_makes_the_ephemeral_versus_durable_contrast_explicit() {
    // AC: "names both destinations and makes the ephemeral-vs-durable
    // distinction explicit."
    let body = read_policy_image_claude_md();
    let section = where_to_write_section(&body);
    let lower = section.to_lowercase();
    assert!(
        lower.contains("this run"),
        "The section must scope `bellows-agent-notes.md` to *this run*: {section}",
    );
    assert!(
        lower.contains("this repo"),
        "The section must scope the repo's context file to *this repo*: {section}",
    );
    assert!(
        lower.contains("durable"),
        "The section must name the durable destination as durable: {section}",
    );
    assert!(
        lower.contains("ephemeral"),
        "The section must name `bellows-agent-notes.md` as ephemeral: {section}",
    );
}

#[test]
fn guidance_states_all_four_bar_conditions() {
    // AC: "The stated bar includes all four conditions above."
    let body = read_policy_image_claude_md();
    let section = where_to_write_section(&body);
    let lower = section.to_lowercase();

    // 1. about *this repo*, not Bellows / the engine / the issue —
    //    Bellows' own defects belong in the notes file for the operator
    //    to raise as a `harness-fault` Correction.
    assert!(
        lower.contains("not about bellows") || lower.contains("not about the harness"),
        "Bar condition 1: the fact must be about this repo, explicitly NOT about Bellows: \
         {section}",
    );
    assert!(
        lower.contains("harness-fault"),
        "Bar condition 1: Bellows' own defects must be routed to `bellows-agent-notes.md` for the \
         operator to raise as a `harness-fault` Correction: {section}",
    );

    // 2. would have saved *this* run real time.
    assert!(
        lower.contains("saved") && lower.contains("real time"),
        "Bar condition 2: knowing it at the start must have saved THIS run real time, not \
         hypothetically helped a future one: {section}",
    );

    // 3. stable — a property of the repo, not of the branch or issue.
    assert!(
        lower.contains("stable"),
        "Bar condition 3: the fact must be stable — a property of the repo, not of the current \
         branch or issue: {section}",
    );

    // 4. not already stated in CLAUDE.md / CONTEXT.md / README.
    assert!(
        lower.contains("not already"),
        "Bar condition 4: the fact must not already be stated in the repo's existing docs: \
         {section}",
    );
    assert!(
        lower.contains("context.md") && lower.contains("readme"),
        "Bar condition 4 must name `CONTEXT.md` and the README as places to check first: \
         {section}",
    );
}

#[test]
fn guidance_states_default_to_not_writing() {
    // AC: "and an explicit 'default to not writing.'"
    let body = read_policy_image_claude_md();
    let section = where_to_write_section(&body);
    let lower = section.to_lowercase();
    assert!(
        lower.contains("default to not writing"),
        "The bar must end with an explicit `Default to not writing.`: {section}",
    );
    assert!(
        lower.contains("if any of those fail, do not write it"),
        "The bar must state that failing any single condition means not writing: {section}",
    );
}

#[test]
fn guidance_bounds_the_size_of_the_durable_write() {
    // Brief: "One or two sentences appended to the existing file. If the
    // target repo has no `CLAUDE.md`, the agent may create one containing
    // only the finding — it must not author a general-purpose template."
    // Without this bound the high bar still permits a page-long essay.
    let body = read_policy_image_claude_md();
    let section = where_to_write_section(&body);
    let lower = section.to_lowercase();
    assert!(
        lower.contains("one or two sentences"),
        "The section must bound the durable write to one or two sentences: {section}",
    );
    assert!(
        lower.contains("template"),
        "The section must forbid authoring a general-purpose template when the repo has no \
         context file yet: {section}",
    );
}
