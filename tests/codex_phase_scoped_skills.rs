//! Issue #169: the Codex context inlining is phase-scoped.
//!
//! `wrap_phase_prompt_for_engine` used to prepend a fixed block to
//! every Codex prompt: the operating context plus *all three* baked
//! skill bodies (tdd, diagnose, triage). That is phase-blind — the
//! security-review prompt, the merger prompt and every per-finding
//! review-fix prompt carried the `tdd` skill even though none of
//! those phases write tests, and the `triage` skill rode along on all
//! seven call sites despite triage running through a separate
//! subcommand path (`src/triage.rs`) that never calls this wrapper.
//!
//! The wrapper now takes the phase and inlines only the skills that
//! phase can actually use:
//!
//! | Phase           | Inlined skills    |
//! | ---             | ---               |
//! | implement       | `tdd`, `diagnose` |
//! | review          | none              |
//! | review-fix      | `tdd`             |
//! | security-review | none              |
//! | security-fix    | `tdd`             |
//! | merger          | none              |
//!
//! The operating context is still prepended for *every* phase — it
//! carries the headless/no-user constraint, the workspace-trust
//! language and the large-file `Read` guidance, all of which every
//! phase needs. Claude and OpenCode remain the identity function.

use bellows::config::Engine;
use bellows::policy::{wrap_phase_prompt_for_engine, Phase};

/// A distinctive line from `policy-image/CLAUDE.md`, chosen because it
/// survives `neutralise_claude_phrasing_for_codex` untouched.
const OPERATING_CONTEXT_MARKER: &str = "**You cannot ask the user.**";

/// A distinctive line from `policy-image/skills/tdd/SKILL.md`.
const TDD_MARKER: &str = "Tests should verify behavior through public interfaces";

/// A distinctive line from `policy-image/skills/diagnose/SKILL.md`.
const DIAGNOSE_MARKER: &str = "A discipline for hard bugs.";

/// A distinctive line from `policy-image/skills/triage/SKILL.md`.
const TRIAGE_MARKER: &str = "Read one issue carefully. Pick a state.";

const BODY: &str = "## Phase body\n\nDo the thing, then stop.\n";

// ---------------------------------------------------------------------
// AC1 — Claude and OpenCode stay the identity function, every phase.
// ---------------------------------------------------------------------

#[test]
fn claude_and_opencode_are_identity_for_every_phase() {
    for engine in [Engine::Claude, Engine::Opencode] {
        for phase in Phase::ALL {
            let wrapped = wrap_phase_prompt_for_engine(engine, phase, BODY);
            assert_eq!(
                wrapped, BODY,
                "wrap_phase_prompt_for_engine({engine:?}, {phase:?}, body) must be the \
                 identity function — both engines auto-discover their operating context \
                 and skills from disk",
            );
        }
    }
}

// ---------------------------------------------------------------------
// AC2 — Codex gets the operating context at every phase.
// ---------------------------------------------------------------------

#[test]
fn codex_inlines_the_operating_context_for_every_phase() {
    for phase in Phase::ALL {
        let wrapped = wrap_phase_prompt_for_engine(Engine::Codex, phase, BODY);
        assert!(
            wrapped.contains("# Operating context"),
            "codex prompt for {phase:?} must carry the operating-context heading",
        );
        assert!(
            wrapped.contains(OPERATING_CONTEXT_MARKER),
            "codex prompt for {phase:?} must carry the operating-context body — it holds \
             the headless/no-user constraint every phase needs",
        );
        assert!(
            wrapped.ends_with(BODY),
            "codex prompt for {phase:?} must still end with the phase body verbatim",
        );
    }
}

// ---------------------------------------------------------------------
// AC3 — implement gets both `tdd` and `diagnose`.
// ---------------------------------------------------------------------

#[test]
fn codex_implement_phase_inlines_tdd_and_diagnose() {
    let wrapped = wrap_phase_prompt_for_engine(Engine::Codex, Phase::Implement, BODY);
    assert!(
        wrapped.contains(TDD_MARKER),
        "the implement phase writes tests — it must carry the tdd skill body",
    );
    assert!(
        wrapped.contains(DIAGNOSE_MARKER),
        "the implement phase is the one that hits hard bugs with budget left to work \
         them — it must carry the diagnose skill body",
    );
}

// ---------------------------------------------------------------------
// AC4 — review, security-review and merger get neither skill.
// ---------------------------------------------------------------------

#[test]
fn codex_read_only_phases_inline_neither_tdd_nor_diagnose() {
    for phase in [Phase::Review, Phase::SecurityReview, Phase::Merger] {
        let wrapped = wrap_phase_prompt_for_engine(Engine::Codex, phase, BODY);
        assert!(
            !wrapped.contains(TDD_MARKER),
            "{phase:?} does not write tests — it must not pay to carry the tdd skill body",
        );
        assert!(
            !wrapped.contains(DIAGNOSE_MARKER),
            "{phase:?} must not carry the diagnose skill body — diagnose is implement-only",
        );
        assert!(
            !wrapped.contains("# Baked skills"),
            "{phase:?} inlines no skills, so it must not emit an empty baked-skills section",
        );
    }
}

// ---------------------------------------------------------------------
// AC5 — the `triage` skill is inlined nowhere.
// ---------------------------------------------------------------------

#[test]
fn codex_never_inlines_the_triage_skill() {
    for phase in Phase::ALL {
        let wrapped = wrap_phase_prompt_for_engine(Engine::Codex, phase, BODY);
        assert!(
            !wrapped.contains(TRIAGE_MARKER),
            "no pipeline phase runs triage — `bellows triage` goes through src/triage.rs, \
             which never calls this wrapper — so {phase:?} must not carry its body",
        );
        assert!(
            !wrapped.contains("## Skill: triage"),
            "{phase:?} must not emit a triage skill heading",
        );
    }
}

// ---------------------------------------------------------------------
// AC6 — review-fix and security-fix get `tdd` but not `diagnose`.
// ---------------------------------------------------------------------

#[test]
fn codex_fix_phases_inline_tdd_but_not_diagnose() {
    for phase in [Phase::ReviewFix, Phase::SecurityFix] {
        let wrapped = wrap_phase_prompt_for_engine(Engine::Codex, phase, BODY);
        assert!(
            wrapped.contains(TDD_MARKER),
            "{phase:?} often means adding or amending a test — it must carry the tdd body",
        );
        assert!(
            !wrapped.contains(DIAGNOSE_MARKER),
            "{phase:?} must not carry the diagnose skill body — diagnose is implement-only",
        );
    }
}

// ---------------------------------------------------------------------
// AC7 — the change actually shrinks the payload.
// ---------------------------------------------------------------------

#[test]
fn codex_review_prompt_is_strictly_shorter_than_the_implement_prompt() {
    let review = wrap_phase_prompt_for_engine(Engine::Codex, Phase::Review, BODY);
    let implement = wrap_phase_prompt_for_engine(Engine::Codex, Phase::Implement, BODY);
    assert!(
        review.len() < implement.len(),
        "the phase-scoped inlining has to reduce the payload, not just reorganise it: \
         review={} bytes, implement={} bytes",
        review.len(),
        implement.len(),
    );
}

#[test]
fn codex_fix_prompt_sits_between_review_and_implement() {
    // The mapping is a gradient, not a binary: review-fix carries one
    // skill, so it must be strictly longer than the skill-free review
    // prompt and strictly shorter than the two-skill implement prompt.
    // Pins the whole table against a regression that collapses it back
    // to a constant.
    let review = wrap_phase_prompt_for_engine(Engine::Codex, Phase::Review, BODY);
    let review_fix = wrap_phase_prompt_for_engine(Engine::Codex, Phase::ReviewFix, BODY);
    let implement = wrap_phase_prompt_for_engine(Engine::Codex, Phase::Implement, BODY);
    assert!(
        review.len() < review_fix.len() && review_fix.len() < implement.len(),
        "expected review < review-fix < implement: {} / {} / {}",
        review.len(),
        review_fix.len(),
        implement.len(),
    );
}

// ---------------------------------------------------------------------
// Prompt coherence: the operating context tells the agent where to find
// skill bodies. That pointer must not dangle on a phase that inlines
// none.
// ---------------------------------------------------------------------

#[test]
fn codex_skill_free_phases_do_not_point_at_a_missing_baked_skills_section() {
    let review = wrap_phase_prompt_for_engine(Engine::Codex, Phase::Review, BODY);
    assert!(
        !review.contains("baked-skills section above"),
        "the review prompt inlines no skills, so the operating context must not tell the \
         agent to look for a baked-skills section that is not there: {review}",
    );
    let implement = wrap_phase_prompt_for_engine(Engine::Codex, Phase::Implement, BODY);
    assert!(
        implement.contains("baked-skills section above"),
        "the implement prompt does inline skills, so the pointer must still resolve",
    );
}

#[test]
fn codex_neutralises_claude_phrasing_for_every_phase() {
    // Pre-existing contract (issue #81): the inlined policy-image
    // content is authored in claude's voice. Re-pinned per phase so the
    // phase-scoped rewrite cannot reintroduce it on one branch of the
    // match.
    for phase in Phase::ALL {
        let wrapped = wrap_phase_prompt_for_engine(Engine::Codex, phase, BODY);
        assert!(
            !wrapped.contains("Claude Code"),
            "codex prompt for {phase:?} must not call the agent \"Claude Code\"",
        );
        assert!(
            !wrapped.contains("your skills directory"),
            "codex prompt for {phase:?} must not point at a skills directory it lacks",
        );
    }
}
