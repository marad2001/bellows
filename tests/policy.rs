use bellows::policy::{
    build_violation_callout, classify_exit, compute_coverage_violations, has_new_tests,
    is_auth_error_signature, is_rate_limit_signature, parse_agent_notes_sections, parse_findings,
    per_finding_kickoff, render_kickoff, synthesize_no_new_tests_entry,
    synthesize_unaddressed_entries, AgentNoteSection, CheckResult, ExitReason, FindingCoverage,
    GateOutcome, ImplementOutcome, ParsedFinding, PhaseOutcomes, ReviewOutcome, Severity,
    BATCH_REVIEW_FIX_NIT_PROMPT, NO_NEW_TESTS_FINDING_TITLE, REVIEW_FIX_PROMPT, REVIEW_PROMPT,
};

fn check(exit: i64) -> CheckResult {
    CheckResult { exit_code: exit, output: String::new() }
}

#[test]
fn rendered_kickoff_includes_the_agent_brief_body() {
    let brief = "## Agent Brief\n\n**Summary:** Do the thing.";
    let prompt = render_kickoff(brief, "https://github.com/owner/repo", "agent/42-do-thing");
    assert!(prompt.contains(brief), "brief missing from prompt: {prompt}");
}

#[test]
fn rendered_kickoff_includes_branch_name_and_repo_url() {
    let prompt = render_kickoff(
        "any brief",
        "https://github.com/owner/repo",
        "agent/42-do-thing",
    );
    assert!(
        prompt.contains("agent/42-do-thing"),
        "branch name missing: {prompt}"
    );
    assert!(
        prompt.contains("https://github.com/owner/repo"),
        "repo url missing: {prompt}"
    );
}

#[test]
fn rendered_kickoff_includes_stop_conditions_and_tooling_hints() {
    let prompt = render_kickoff("any brief", "https://github.com/owner/repo", "agent/42-x");
    assert!(prompt.contains("tdd"), "tdd skill mention missing: {prompt}");
    assert!(prompt.contains("cargo test"), "cargo test mention missing: {prompt}");
    assert!(prompt.contains("marker"), "marker file mention missing: {prompt}");
}

#[test]
fn classify_exit_returns_success_when_all_phases_clean() {
    // Tracer bullet for slice X1: every phase produced a clean exit and
    // every cargo gate's clippy + test passed. No findings, so review-fix
    // didn't run. Both gates ran (Cargo.toml is at the workspace root).
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: 0,
            stderr_tail: String::new(),
        },
        post_implement_gate: GateOutcome {
            cargo_clippy: Some(check(0)),
            cargo_test: Some(check(0)),
        },
        review: Some(ReviewOutcome {
            findings_text: None,
            exit_code: 0,
        }),
        review_fix: None,
        end_pipeline_gate: Some(GateOutcome {
            cargo_clippy: Some(check(0)),
            cargo_test: Some(check(0)),
        }),
        wall_clock_exceeded: false,
        backstop_violations: Vec::new(),
    };
    assert_eq!(classify_exit(false, &outcomes), ExitReason::Success);
}

/// Helper for migrated tests: an `Outcomes` shape representing the
/// slice-5 path (only the post-implement gate populated, no review,
/// no end gate). Each test tweaks one field to express its scenario.
fn slice5_shaped(implement_exit: i64, cargo_test: Option<i64>) -> PhaseOutcomes {
    PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: implement_exit,
            stderr_tail: String::new(),
        },
        post_implement_gate: GateOutcome {
            cargo_clippy: None,
            cargo_test: cargo_test.map(check),
        },
        review: None,
        review_fix: None,
        end_pipeline_gate: None,
        wall_clock_exceeded: false,
        backstop_violations: Vec::new(),
    }
}

#[test]
fn classify_exit_returns_success_for_clean_run_with_tests_green() {
    assert_eq!(
        classify_exit(false, &slice5_shaped(0, Some(0))),
        ExitReason::Success
    );
}

#[test]
fn classify_exit_returns_success_when_cargo_test_gate_was_skipped() {
    // None means the workspace had no Cargo.toml at root; the runner
    // skipped the cargo test gate. Non-Rust briefs are a valid use case.
    assert_eq!(
        classify_exit(false, &slice5_shaped(0, None)),
        ExitReason::Success
    );
}

#[test]
fn classify_exit_returns_self_reported_failure_when_agent_notes_present() {
    // agent-notes.md presence wins over exit code 0 AND green tests —
    // the agent's voice trumps everything.
    assert_eq!(
        classify_exit(true, &slice5_shaped(0, Some(0))),
        ExitReason::AgentSelfReportedFailure
    );
}

#[test]
fn classify_exit_returns_crash_when_agent_exits_non_zero_without_notes() {
    // Agent process died (claude itself errored, OOM, etc.). No notes
    // file means the agent didn't get to write a structured report.
    assert_eq!(
        classify_exit(false, &slice5_shaped(1, None)),
        ExitReason::Crash
    );
    assert_eq!(
        classify_exit(false, &slice5_shaped(137, Some(0))),
        ExitReason::Crash
    );
}

#[test]
fn classify_exit_returns_wall_clock_exceeded_when_flag_is_set() {
    // Tracer bullet for slice 6: even with otherwise-clean outcomes, the
    // wall_clock_exceeded flag drives WallClockExceeded. Set when the
    // runner kills a container at the deadline OR finds remaining budget
    // <= 0 before launching a phase.
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new() },
        post_implement_gate: GateOutcome {
            cargo_clippy: Some(check(0)),
            cargo_test: Some(check(0)),
        },
        review: None,
        review_fix: None,
        end_pipeline_gate: None,
        wall_clock_exceeded: true,
        backstop_violations: Vec::new(),
    };
    assert_eq!(classify_exit(false, &outcomes), ExitReason::WallClockExceeded);
}

#[test]
fn is_rate_limit_signature_matches_anthropic_json_error_type() {
    // Anthropic's API returns errors with a `"type": "rate_limit_error"`
    // field — that exact string is what claude's stderr surfaces when
    // hitting the rate limit. Match should be case-insensitive on the
    // signature itself, but the typical surface is exactly this form.
    assert!(is_rate_limit_signature(
        r#"{"error":{"type":"rate_limit_error","message":"This request would exceed the rate limit"}}"#
    ));
}

#[test]
fn is_rate_limit_signature_rejects_ordinary_panic_stderr() {
    // A run-of-the-mill panic should NOT match — different operator
    // response (investigate vs wait-and-retry).
    let panic_stderr =
        "thread 'main' panicked at src/main.rs:42:5: index out of bounds: the len is 3 but the index is 5";
    assert!(!is_rate_limit_signature(panic_stderr));
}

#[test]
fn is_rate_limit_signature_does_not_false_positive_on_unrelated_rate_mention() {
    // The word "rate" appearing in unrelated contexts (e.g. naming a
    // variable, a test fixture, a comment) must not trigger the
    // detector. Specificity comes from the underscore-style identifiers
    // Anthropic uses (`rate_limit_error`, `rate_limited`), not the bare
    // word "rate."
    let benign_stderr = "Computing rate at which the simulation converges. Result: 0.42";
    assert!(!is_rate_limit_signature(benign_stderr));
}

#[test]
fn is_auth_error_signature_matches_anthropic_refresh_token_expired_response() {
    // Anthropic-style auth-error stderr after a refresh token expires.
    // The canonical shape is a 401 with an underscore-style identifier;
    // match should be case-insensitive on the signature.
    assert!(is_auth_error_signature(
        r#"401 Unauthorized: {"error":{"type":"authentication_error","message":"refresh_token_expired"}}"#
    ));
}

#[test]
fn is_auth_error_signature_rejects_ordinary_panic_stderr() {
    // A run-of-the-mill panic should NOT match — different operator
    // response (investigate vs run refresh-auth and retry).
    let panic_stderr =
        "thread 'main' panicked at src/main.rs:42:5: index out of bounds: the len is 3 but the index is 5";
    assert!(!is_auth_error_signature(panic_stderr));
}

#[test]
fn is_auth_error_signature_does_not_false_positive_on_benign_auth_word_mention() {
    // The bare word "auth" or "authentication" appearing in unrelated
    // contexts (e.g. test fixtures, variable names, documentation
    // strings) must not trigger the detector. Specificity comes from the
    // underscore-style identifiers and the literal "401 unauthorized"
    // shape, not the standalone word "auth".
    let benign_stderr =
        "Wrote auth helper to src/auth.rs and added a doc comment for the authentication module.";
    assert!(!is_auth_error_signature(benign_stderr));
}

#[test]
fn classify_exit_returns_rate_limited_when_stderr_matches_signature_and_implement_exit_non_zero() {
    // Implement crashed (non-zero exit) AND its captured stderr tail
    // contains an Anthropic rate-limit signature. Operator-wise this
    // is meaningfully different from a generic crash — the response is
    // "wait for the rate-limit window to clear and re-run", not
    // "investigate". So classify as RateLimited, not Crash.
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: 1,
            stderr_tail:
                r#"Error: API request failed: {"type":"rate_limit_error","message":"slow down"}"#
                    .to_string(),
        },
        post_implement_gate: GateOutcome::default(),
        review: None,
        review_fix: None,
        end_pipeline_gate: None,
        wall_clock_exceeded: false,
        backstop_violations: Vec::new(),
    };
    assert_eq!(classify_exit(false, &outcomes), ExitReason::RateLimited);
}

#[test]
fn classify_exit_does_not_return_rate_limited_when_signature_present_but_exit_was_zero() {
    // Signature alone is NOT enough — the run must have actually exited
    // non-zero. A clean run that happened to print "rate_limit_error"
    // somewhere benign (e.g. as part of a documentation string the
    // agent committed) shouldn't classify as RateLimited.
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: 0,
            stderr_tail:
                "Wrote example handling for rate_limit_error to docs.md.".to_string(),
        },
        post_implement_gate: GateOutcome {
            cargo_clippy: Some(check(0)),
            cargo_test: Some(check(0)),
        },
        review: None,
        review_fix: None,
        end_pipeline_gate: None,
        wall_clock_exceeded: false,
        backstop_violations: Vec::new(),
    };
    assert_eq!(classify_exit(false, &outcomes), ExitReason::Success);
}

#[test]
fn classify_exit_self_reported_failure_wins_over_wall_clock_exceeded() {
    // Notes-precedence: even when the runner halted due to wall-clock,
    // an agent-notes.md present in the workspace still classifies as
    // AgentSelfReportedFailure. The agent's voice trumps tooling
    // signals, including the wall-clock kill — if claude got far enough
    // to write structured notes about why it couldn't finish, those
    // notes are the operator's most useful artifact.
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new() },
        post_implement_gate: GateOutcome::default(),
        review: None,
        review_fix: None,
        end_pipeline_gate: None,
        wall_clock_exceeded: true,
        backstop_violations: Vec::new(),
    };
    assert_eq!(
        classify_exit(true, &outcomes),
        ExitReason::AgentSelfReportedFailure,
    );
}

#[test]
fn classify_exit_returns_final_tests_red_when_post_implement_gate_clippy_failed() {
    // Implement run was clean (exit 0, no notes) and cargo test passed,
    // but clippy flagged something — gate fails on clippy alone.
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new() },
        post_implement_gate: GateOutcome {
            cargo_clippy: Some(check(101)),
            cargo_test: Some(check(0)),
        },
        review: None,
        review_fix: None,
        end_pipeline_gate: None,
        wall_clock_exceeded: false,
        backstop_violations: Vec::new(),
    };
    assert_eq!(classify_exit(false, &outcomes), ExitReason::FinalTestsRed);
}

#[test]
fn classify_exit_returns_final_tests_red_when_end_pipeline_gate_failed() {
    // Post-implement gate was clean. Review ran and produced findings,
    // review-fix addressed them, but the fixups broke a test — caught
    // by the end-of-pipeline gate.
    let outcomes = PhaseOutcomes {
        implement: ImplementOutcome { exit_code: 0, stderr_tail: String::new() },
        post_implement_gate: GateOutcome {
            cargo_clippy: Some(check(0)),
            cargo_test: Some(check(0)),
        },
        review: Some(ReviewOutcome { findings_text: Some("found stuff".to_string()), exit_code: 0 }),
        review_fix: Some(bellows::policy::FixOutcome { exit_code: 0 }),
        end_pipeline_gate: Some(GateOutcome {
            cargo_clippy: Some(check(0)),
            cargo_test: Some(check(101)),
        }),
        wall_clock_exceeded: false,
        backstop_violations: Vec::new(),
    };
    assert_eq!(classify_exit(false, &outcomes), ExitReason::FinalTestsRed);
}

#[test]
fn classify_exit_returns_final_tests_red_when_cargo_test_failed() {
    // Agent thought it was done (exit 0, no notes), but the cargo test
    // gate caught failing tests.
    assert_eq!(
        classify_exit(false, &slice5_shaped(0, Some(1))),
        ExitReason::FinalTestsRed
    );
    assert_eq!(
        classify_exit(false, &slice5_shaped(0, Some(101))),
        ExitReason::FinalTestsRed
    );
}

#[test]
fn review_prompt_locks_severity_vocabulary_as_closed_set() {
    // The review prompt must declare the severity vocabulary as a closed
    // set of exactly three values. Without this, the implement-side agent
    // can invent its own severity tags ("medium", "minor", "follow-up")
    // and the review-fix agent's address-OR-explain rule — keyed on
    // `blocker` and `important` — silently fails to bind.
    assert!(
        REVIEW_PROMPT.contains("blocker | important | nit"),
        "REVIEW_PROMPT must declare the severity vocabulary blocker|important|nit: {REVIEW_PROMPT}"
    );
    assert!(
        REVIEW_PROMPT.contains("use exactly one of these three values"),
        "REVIEW_PROMPT must instruct exactly-one-of-three: {REVIEW_PROMPT}"
    );
}

#[test]
fn review_prompt_example_demonstrates_each_severity() {
    // The example findings block in the prompt must show one of each
    // severity so the agent has a concrete template, not just an abstract
    // grammar. Without an example, agents tend to default to one severity
    // (usually the harshest available) and the gradient collapses.
    assert!(
        REVIEW_PROMPT.contains("— blocker"),
        "REVIEW_PROMPT example must include a blocker-tagged finding: {REVIEW_PROMPT}"
    );
    assert!(
        REVIEW_PROMPT.contains("— important"),
        "REVIEW_PROMPT example must include an important-tagged finding: {REVIEW_PROMPT}"
    );
    assert!(
        REVIEW_PROMPT.contains("— nit"),
        "REVIEW_PROMPT example must include a nit-tagged finding: {REVIEW_PROMPT}"
    );
}

#[test]
fn review_prompt_locks_title_format_for_deterministic_parser_extraction() {
    // Slice 9.6: the parser-as-backstop matches verbatim titles between
    // findings and agent-notes sections. For that to be deterministic
    // the review prompt must instruct the agent that the title line
    // (a) is on ONE line, (b) ends with ` — <tag>`, and (c) contains no
    // markdown links or backticks that would break extraction. Without
    // these locks the parser would silently miss findings whose title
    // formatting drifts.
    assert!(
        REVIEW_PROMPT.contains("title MUST be on one line"),
        "REVIEW_PROMPT must lock the one-line title rule: {REVIEW_PROMPT}"
    );
    assert!(
        REVIEW_PROMPT.contains("MUST end with ` — `"),
        "REVIEW_PROMPT must lock the em-dash separator suffix: {REVIEW_PROMPT}"
    );
    assert!(
        REVIEW_PROMPT.contains("MUST NOT contain markdown links or backticks"),
        "REVIEW_PROMPT must forbid markdown links/backticks in titles: {REVIEW_PROMPT}"
    );
}

#[test]
fn review_fix_prompt_locks_per_finding_scope_not_every_finding_language() {
    // Slice 9.6 rewrites REVIEW_FIX_PROMPT for the per-finding shape:
    // the agent sees exactly ONE finding per invocation, not a list.
    // The "every finding marked blocker or important" phrasing from the
    // slice-9.5 prompt MUST be gone — it is the precise wording that
    // allowed agents to decide "I'll skip all of them in one breath."
    //
    // This test is the load-bearing replacement for the slice-9.5
    // "makes_blocker_and_important_findings_mandatory" test. The SPIRIT
    // (lock the address-OR-explain contract against future weakening)
    // is preserved with equally-pinned wording on the per-finding shape.
    assert!(
        !REVIEW_FIX_PROMPT.contains("every finding marked"),
        "REVIEW_FIX_PROMPT must NOT use the slice-9.5 every-finding phrasing — \
         slice 9.6 scopes invocations to a single finding so that wording is no \
         longer a valid description of the contract: {REVIEW_FIX_PROMPT}"
    );
    // The new mandate names the single-finding shape so the agent
    // literally cannot read this prompt as "decide which of N to do."
    assert!(
        REVIEW_FIX_PROMPT.contains("ONE finding") || REVIEW_FIX_PROMPT.contains("one finding"),
        "REVIEW_FIX_PROMPT must scope the agent to a single finding: {REVIEW_FIX_PROMPT}"
    );
}

#[test]
fn review_fix_prompt_locks_address_or_explain_for_the_single_finding() {
    // The address-OR-explain contract survives the rewrite, restated
    // in the per-finding shape: address this finding in code OR write
    // an agent-notes section. Silent skip is prompt-out-of-bounds.
    //
    // Load-bearing replacement for the slice-9.5
    // "permits_silent_skip_of_nit_findings" inverse — that test moves
    // to BATCH_REVIEW_FIX_NIT_PROMPT. Here we lock the OPPOSITE rule
    // for the per-finding (blocker/important) path: silent skip is NOT
    // permitted.
    let lower = REVIEW_FIX_PROMPT.to_lowercase();
    assert!(
        lower.contains("silent skip") && lower.contains("out-of-bounds"),
        "REVIEW_FIX_PROMPT must literally frame silent skip as prompt-out-of-bounds: \
         {REVIEW_FIX_PROMPT}"
    );
    // The two options must be explicit, in this order.
    assert!(
        REVIEW_FIX_PROMPT.contains("Address") || REVIEW_FIX_PROMPT.contains("address"),
        "REVIEW_FIX_PROMPT must spell out option 1 (address in code): {REVIEW_FIX_PROMPT}"
    );
    assert!(
        REVIEW_FIX_PROMPT.contains("## Unaddressed finding:"),
        "REVIEW_FIX_PROMPT must spell out option 2 (## Unaddressed finding section): \
         {REVIEW_FIX_PROMPT}"
    );
}

#[test]
fn review_fix_prompt_demands_verbatim_title_for_unaddressed_finding_section() {
    // The bellows parser-as-backstop matches the section title against
    // the finding title verbatim. The prompt MUST tell the agent to use
    // the EXACT verbatim title — otherwise the agent will paraphrase
    // ("# Unaddressed: short version") and the backstop silently fails
    // to match, defeating the whole mechanism.
    let lower = REVIEW_FIX_PROMPT.to_lowercase();
    assert!(
        lower.contains("verbatim"),
        "REVIEW_FIX_PROMPT must demand the verbatim title — otherwise the parser-as-backstop \
         cannot cross-reference sections to findings: {REVIEW_FIX_PROMPT}"
    );
}

#[test]
fn review_fix_prompt_documents_agent_self_reported_failure_routing() {
    // Survives the rewrite: the agent must understand that appending an
    // unaddressed-finding section routes the run to
    // agent-self-reported-failure (draft PR with the agent-failed
    // label). Without this, the prompt reads as "write a note when
    // stuck" which understates the signal — appending IS the
    // escalation, and the agent should reach for it deliberately.
    let lower = REVIEW_FIX_PROMPT.to_lowercase();
    assert!(
        lower.contains("agent-self-reported-failure")
            || lower.contains("draft pr with agent-failed label")
            || lower.contains("agent-failed"),
        "REVIEW_FIX_PROMPT must surface the agent-self-reported-failure routing consequence: {REVIEW_FIX_PROMPT}"
    );
}

// ---- Backstop helpers: compute_coverage_violations, synthesize_unaddressed_entries,
//      build_violation_callout ----

fn finding(title: &str, severity: Severity) -> ParsedFinding {
    ParsedFinding {
        title: title.to_string(),
        severity,
        body: "irrelevant body".to_string(),
    }
}

fn coverage(title: &str, severity: Severity, commit_landed: bool) -> FindingCoverage {
    FindingCoverage {
        finding: finding(title, severity),
        commit_landed,
    }
}

fn note(title: &str) -> AgentNoteSection {
    AgentNoteSection {
        title: title.to_string(),
        body: "irrelevant body".to_string(),
    }
}

#[test]
fn compute_coverage_violations_reports_no_violations_when_all_findings_addressed_in_code() {
    // Happy path: every blocker/important finding produced a commit in
    // its per-finding invocation. No agent-notes sections needed; no
    // violations.
    let cov = vec![
        coverage("blocker title", Severity::Blocker, true),
        coverage("important title", Severity::Important, true),
    ];
    let violations = compute_coverage_violations(&cov, &[]);
    assert!(violations.is_empty(), "no violations expected: {:?}", violations);
}

#[test]
fn compute_coverage_violations_reports_no_violations_when_uncommitted_findings_are_explained() {
    // The agent declined to address a blocker in code but DID append a
    // matching `## Unaddressed finding:` section. That's the
    // address-OR-explain contract — explained, so no violation. The
    // backstop fires only when neither code nor explanation is present.
    let cov = vec![
        coverage("blocker title", Severity::Blocker, false),
        coverage("important title", Severity::Important, true),
    ];
    let sections = vec![note("blocker title")];
    let violations = compute_coverage_violations(&cov, &sections);
    assert!(violations.is_empty(), "explained finding is not a violation: {:?}", violations);
}

#[test]
fn compute_coverage_violations_flags_blocker_without_commit_and_without_note() {
    // The core silent-skip case: agent exited 0 with no commit AND no
    // agent-notes section. The backstop must surface this so the runner
    // forces agent-self-reported-failure rather than shipping it as
    // Success — the exact failure mode that 4 consecutive bellows-on-
    // bellows runs demonstrated cannot be closed by prompt language
    // alone.
    let cov = vec![
        coverage("blocker title", Severity::Blocker, false),
        coverage("important title", Severity::Important, true),
    ];
    let violations = compute_coverage_violations(&cov, &[]);
    assert_eq!(violations.len(), 1, "exactly the unaddressed blocker should violate: {:?}", violations);
    assert_eq!(violations[0].title, "blocker title");
    assert_eq!(violations[0].severity, Severity::Blocker);
}

#[test]
fn compute_coverage_violations_flags_important_without_commit_and_without_note() {
    // Same shape as the blocker case but with `important` — the rule
    // binds the top TWO severities, not just blocker, because important
    // findings were the exact category the 4-PR silent-skip pattern
    // exploited.
    let cov = vec![coverage("important title", Severity::Important, false)];
    let violations = compute_coverage_violations(&cov, &[]);
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].severity, Severity::Important);
}

#[test]
fn compute_coverage_violations_does_not_flag_unaddressed_nits() {
    // `nit` findings are operator-discretionary. A nit with no commit
    // and no agent-notes section is NOT a violation — silent skip is
    // explicitly permitted for nits. The backstop must not over-fire
    // on cosmetic findings, otherwise every run with a skipped nit
    // would route to agent-self-reported-failure.
    let cov = vec![coverage("nit title", Severity::Nit, false)];
    let violations = compute_coverage_violations(&cov, &[]);
    assert!(violations.is_empty(), "unaddressed nits are not violations: {:?}", violations);
}

#[test]
fn compute_coverage_violations_title_comparison_is_verbatim_case_sensitive() {
    // The parser-as-backstop matches titles character-for-character.
    // A paraphrased section title ("blocker title" vs "Blocker title")
    // does NOT count as an explanation — otherwise an agent could
    // shorten or capitalise the title and the backstop would silently
    // accept it.
    let cov = vec![coverage("blocker title", Severity::Blocker, false)];
    let sections = vec![note("Blocker title")]; // capitalisation differs
    let violations = compute_coverage_violations(&cov, &sections);
    assert_eq!(
        violations.len(),
        1,
        "verbatim title match required; capitalisation drift must not be accepted: {:?}",
        violations
    );
}

#[test]
fn synthesize_unaddressed_entries_produces_appendable_markdown_with_verbatim_titles() {
    // When the backstop fires, bellows appends an `## Unaddressed
    // finding:` section per violation so the existing `has_agent_notes
    // → AgentSelfReportedFailure` precedence in classify_exit takes
    // effect. The synthesized markdown must (a) use the verbatim
    // finding title, (b) be appendable (no leading whitespace issues),
    // and (c) carry a body explaining bellows synthesized this — so a
    // human reading agent-notes.md later can see it wasn't written by
    // claude.
    let violations = vec![
        finding("first violation", Severity::Blocker),
        finding("second violation", Severity::Important),
    ];
    let appended = synthesize_unaddressed_entries(&violations);
    assert!(
        appended.contains("## Unaddressed finding: first violation"),
        "synthesised markdown must include verbatim title #1: {appended}"
    );
    assert!(
        appended.contains("## Unaddressed finding: second violation"),
        "synthesised markdown must include verbatim title #2: {appended}"
    );
    // Bellows must distinguish synthesised entries from agent-written
    // ones so a reader knows where the entry came from.
    let lower = appended.to_lowercase();
    assert!(
        lower.contains("bellows") && (lower.contains("synthes") || lower.contains("backstop")),
        "synthesised entry must identify bellows as the author: {appended}"
    );
}

#[test]
fn synthesize_unaddressed_entries_returns_empty_when_no_violations() {
    // Defensive guard: the runner only calls synthesize_... when there
    // are violations, but a zero-violation call must produce empty
    // output rather than a header-only "## Unaddressed finding: " stub
    // (which would itself satisfy parse_agent_notes_sections and route
    // a clean run to agent-self-reported-failure).
    let appended = synthesize_unaddressed_entries(&[]);
    assert!(appended.is_empty() || appended.trim().is_empty(),
        "no violations must produce empty (or whitespace-only) output: {appended:?}");
}

#[test]
fn build_violation_callout_names_each_offending_finding_under_named_section() {
    // The log comment must surface a `### Address-or-explain contract
    // violated` callout naming the offending findings, so the operator
    // reading the PR comment sees explicitly that the run was forced to
    // agent-self-reported-failure by the bellows-side check (not by the
    // agent itself).
    let violations = vec![
        finding("blocker with silent skip", Severity::Blocker),
        finding("important also silently skipped", Severity::Important),
    ];
    let callout = build_violation_callout(&violations);
    assert!(
        callout.contains("### Address-or-explain contract violated"),
        "callout must use the canonical heading: {callout}"
    );
    assert!(
        callout.contains("blocker with silent skip"),
        "callout must name the first violation: {callout}"
    );
    assert!(
        callout.contains("important also silently skipped"),
        "callout must name the second violation: {callout}"
    );
    // Severity should be surfaced too so the operator can prioritise.
    assert!(
        callout.contains("blocker") && callout.contains("important"),
        "callout must surface each violation's severity: {callout}"
    );
}

#[test]
fn batch_review_fix_nit_prompt_permits_silent_skip_of_nits() {
    // Slice 9.6: `nit` findings go through a separate batched
    // invocation with a permissive prompt. Silent skip IS allowed for
    // nits — the operator already sees every nit in the review-findings
    // PR comment and can decide whether to follow up. The prompt MUST
    // literally permit skipping; without that, a tightening of the
    // per-finding prompt (which is imperative) could bleed into the nit
    // path and the agent would burn time on cosmetic findings.
    //
    // This test is the load-bearing successor to slice-9.5's
    // `review_fix_prompt_permits_silent_skip_of_nit_findings`, which
    // pinned the permission on the old combined REVIEW_FIX_PROMPT.
    // Slice 9.6 splits the two paths, so the permission for nits
    // moves here.
    assert!(
        BATCH_REVIEW_FIX_NIT_PROMPT.contains("MAY skip a `nit`"),
        "BATCH_REVIEW_FIX_NIT_PROMPT must literally permit skipping nits: {BATCH_REVIEW_FIX_NIT_PROMPT}"
    );
    assert!(
        BATCH_REVIEW_FIX_NIT_PROMPT.contains("operator-discretionary"),
        "BATCH_REVIEW_FIX_NIT_PROMPT must frame nits as operator-discretionary: {BATCH_REVIEW_FIX_NIT_PROMPT}"
    );
}

#[test]
fn batch_review_fix_nit_prompt_does_not_route_through_unaddressed_finding_path() {
    // Nits MUST NOT use the `## Unaddressed finding:` escalation path —
    // appending such a section routes the run to
    // agent-self-reported-failure, which is far too heavy a signal for
    // a nit the agent simply chose not to do. The prompt must explicitly
    // tell the agent NOT to append for nits; otherwise a careful agent
    // might apply the per-finding contract by analogy and escalate
    // every skipped nit.
    let lower = BATCH_REVIEW_FIX_NIT_PROMPT.to_lowercase();
    assert!(
        lower.contains("do not append to agent-notes.md for nits"),
        "BATCH_REVIEW_FIX_NIT_PROMPT must tell the agent not to append unaddressed-finding \
         sections for nits: {BATCH_REVIEW_FIX_NIT_PROMPT}"
    );
}

#[test]
fn review_fix_prompt_preserves_commit_per_finding_convention() {
    // The "one commit per finding" convention from the prior prompt must
    // survive this rewrite — operator-side review depends on per-finding
    // commits to map fixes back to the review-findings PR comment.
    assert!(
        REVIEW_FIX_PROMPT.contains("commit per finding")
            || REVIEW_FIX_PROMPT.contains("one commit per finding"),
        "REVIEW_FIX_PROMPT must preserve the commit-per-finding convention: {REVIEW_FIX_PROMPT}"
    );
}

// ---- Slice 9.6: per-finding parser + parser-as-backstop ----

#[test]
fn parse_findings_extracts_all_three_severities_from_review_prompt_example_block() {
    // The REVIEW_PROMPT vendored example shows one of each severity. The
    // parser must recognise the three-element closed vocabulary AND keep
    // them in source order so the runner can iterate blocker→important
    // →nit in a predictable shape.
    let text = "\
## Findings

### 1. status file leaks busy state — important

The early-returns skip cleanup.

**Suggestion:** wrap in a guard.

### 2. unwrap on parsed config can panic — blocker

Panics inside serde_json::from_str.

**Suggestion:** map to ConfigError::Parse.

### 3. helper function name shadows std::cmp::min — nit

Reads fine locally but conflicts elsewhere.

**Suggestion:** rename to min_nonzero.
";
    let result = parse_findings(text);
    assert!(result.malformed_titles.is_empty());
    let severities: Vec<Severity> = result.findings.iter().map(|f| f.severity).collect();
    assert_eq!(severities, vec![Severity::Important, Severity::Blocker, Severity::Nit]);
    let titles: Vec<&str> = result.findings.iter().map(|f| f.title.as_str()).collect();
    assert_eq!(
        titles,
        vec![
            "status file leaks busy state",
            "unwrap on parsed config can panic",
            "helper function name shadows std::cmp::min",
        ]
    );
}

#[test]
fn per_finding_kickoff_interpolates_title_severity_and_body_into_the_prompt() {
    // The per-finding agent must see the specific finding it's there to
    // handle. The kickoff renders the slice-9.6 single-finding prompt
    // with the title / severity / body interpolated; the agent has no
    // way to drift into "address everything" or "skip everything" because
    // there is no list — only this one finding.
    let finding = ParsedFinding {
        title: "config parser panics on empty input".to_string(),
        severity: Severity::Blocker,
        body: "`Config::from_str(\"\")` panics inside serde_json.\n\n**Suggestion:** map to ConfigError::Parse.".to_string(),
    };
    let kickoff = per_finding_kickoff(&finding, ".bellows-review-diff.patch", "agent-notes.md");

    assert!(
        kickoff.contains("config parser panics on empty input"),
        "title must appear in the kickoff body: {kickoff}"
    );
    assert!(
        kickoff.contains("blocker"),
        "severity tag must appear in the kickoff body: {kickoff}"
    );
    assert!(
        kickoff.contains("**Suggestion:** map to ConfigError::Parse"),
        "finding body must be interpolated verbatim: {kickoff}"
    );
}

#[test]
fn per_finding_kickoff_instructs_exact_verbatim_unaddressed_finding_header() {
    // The parser-as-backstop matches `## Unaddressed finding: <title>`
    // verbatim. The kickoff must spell out the exact header the agent
    // should append, with the SAME verbatim title — otherwise the agent
    // might paraphrase ("# Unaddressed: short title") and the backstop
    // would silently fail to match.
    let finding = ParsedFinding {
        title: "title with — em dashes — in it".to_string(),
        severity: Severity::Important,
        body: "body".to_string(),
    };
    let kickoff = per_finding_kickoff(&finding, ".bellows-review-diff.patch", "agent-notes.md");
    assert!(
        kickoff.contains("## Unaddressed finding: title with — em dashes — in it"),
        "kickoff must show the exact `## Unaddressed finding: <verbatim title>` header the agent should append: {kickoff}"
    );
    // The address-OR-explain framing must be present — the agent must
    // see that there are exactly two options.
    assert!(
        kickoff.to_lowercase().contains("address") || kickoff.contains("code fix"),
        "kickoff must mention the address-in-code option: {kickoff}"
    );
}

#[test]
fn per_finding_kickoff_carries_severity_tone_distinguishing_blocker_from_important() {
    // The brief: "Severity-aware tone (blocker tone may be more urgent
    // than important; nits don't go through this path — they stay in a
    // batch)". The blocker kickoff must literally say "blocker" while
    // the important one literally says "important", AND the urgency
    // wording must differ — otherwise the gradient collapses and the
    // top severity becomes indistinguishable from the second.
    let blocker = ParsedFinding {
        title: "t".into(),
        severity: Severity::Blocker,
        body: "b".into(),
    };
    let important = ParsedFinding {
        title: "t".into(),
        severity: Severity::Important,
        body: "b".into(),
    };
    let blocker_kickoff = per_finding_kickoff(&blocker, "d", "n");
    let important_kickoff = per_finding_kickoff(&important, "d", "n");
    assert_ne!(
        blocker_kickoff, important_kickoff,
        "blocker and important kickoffs must differ in urgency wording, not just the severity tag"
    );
    assert!(blocker_kickoff.contains("blocker"));
    assert!(important_kickoff.contains("important"));
}

#[test]
fn per_finding_kickoff_silent_skip_is_explicitly_out_of_bounds() {
    // The whole point of the per-finding shape: silent skip is
    // prompt-out-of-bounds. The agent must see that doing nothing is
    // NOT an option — only "address in code" or "write the unaddressed-
    // finding section" are.
    let finding = ParsedFinding {
        title: "t".into(),
        severity: Severity::Blocker,
        body: "b".into(),
    };
    let kickoff = per_finding_kickoff(&finding, "d", "n");
    let lower = kickoff.to_lowercase();
    assert!(
        lower.contains("out-of-bounds") || lower.contains("out of bounds"),
        "kickoff must surface the prompt-out-of-bounds framing so the agent cannot read silent skip as permitted: {kickoff}"
    );
}

#[test]
fn parse_agent_notes_sections_extracts_unaddressed_finding_sections_by_verbatim_title() {
    // The per-finding agent appends a `## Unaddressed finding: <title>`
    // section per finding it deliberately chose not to address in code.
    // The parser-as-backstop reads them to verify the address-OR-explain
    // contract. Title comparison is verbatim — the section's title must
    // match the finding's title character-for-character.
    let text = "\
# agent-notes.md

Some preamble.

## Unaddressed finding: unwrap on parsed config can panic on empty input

Would need a redesign of the config parser path; out of scope for this PR.

## Unaddressed finding: status file leaks busy state on Rust error returns

Requires a guard-pattern refactor in run_one; deferred to a follow-up.
";
    let sections = parse_agent_notes_sections(text);
    assert_eq!(sections.len(), 2);
    assert_eq!(sections[0].title, "unwrap on parsed config can panic on empty input");
    assert!(sections[0].body.contains("redesign of the config parser"));
    assert_eq!(sections[1].title, "status file leaks busy state on Rust error returns");
    assert!(sections[1].body.contains("guard-pattern refactor"));
}

#[test]
fn parse_agent_notes_sections_ignores_other_headings_at_same_level() {
    // agent-notes.md often carries general notes from the implement or
    // review phases under unrelated `## ...` headings. The parser must
    // only collect Unaddressed-finding sections — others end the
    // current section (if any) but do NOT contribute a phantom entry.
    let text = "\
## Implement-phase notes

Could not complete the foo refactor; left a TODO in src/foo.rs.

## Unaddressed finding: real finding title here

Body of the unaddressed-finding section.

## Some other random heading

Unrelated content that must not become a section.
";
    let sections = parse_agent_notes_sections(text);
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].title, "real finding title here");
    assert!(sections[0].body.contains("Body of the unaddressed-finding section"));
}

#[test]
fn parse_agent_notes_sections_returns_empty_for_file_with_no_unaddressed_sections() {
    // A typical implement-phase agent-notes.md (general notes, no
    // unaddressed-finding sections) must parse to an empty list — the
    // parser-as-backstop will then see "no explained findings" and apply
    // the address-OR-explain rule accordingly.
    let text = "Just some notes from earlier phases.\nNothing structured.\n";
    let sections = parse_agent_notes_sections(text);
    assert!(sections.is_empty());
}

#[test]
fn parse_findings_rejects_off_vocabulary_severity_tags_as_malformed() {
    // The closed vocabulary lock means "medium" / "minor" / "follow-up"
    // are off-list. The parser must NOT silently demote them to a
    // ParsedFinding (that would let agents invent severities again and
    // collapse the gradient back to "everything looks the same"). Instead
    // they surface in malformed_titles so the runner can log the
    // breakdown rather than silently dropping content.
    let text = "\
## Findings

### 1. severity-typo finding — medium

Body irrelevant to the test.

### 2. real finding — important

Body irrelevant.

### 3. another bad one — follow-up

Body irrelevant.
";
    let result = parse_findings(text);
    assert_eq!(
        result.findings.len(),
        1,
        "only the well-formed `important` finding should parse: {:?}",
        result.findings
    );
    assert_eq!(result.findings[0].severity, Severity::Important);
    assert_eq!(result.malformed_titles.len(), 2, "two malformed titles: {:?}", result.malformed_titles);
    // Each malformed title is surfaced verbatim so the operator can see
    // exactly what the review agent produced.
    let combined = result.malformed_titles.join(" | ");
    assert!(combined.contains("medium"), "raw `medium` line missing: {combined}");
    assert!(combined.contains("follow-up"), "raw `follow-up` line missing: {combined}");
}

#[test]
fn parse_findings_treats_title_without_em_dash_separator_as_malformed() {
    // If the agent forgot the ` — <tag>` suffix entirely (just wrote a
    // bare title), the parser must not guess a severity. Such a line is
    // recorded as malformed.
    let text = "\
## Findings

### 1. forgot the severity tag entirely

Some description.
";
    let result = parse_findings(text);
    assert!(result.findings.is_empty(), "no finding should parse: {:?}", result.findings);
    assert_eq!(result.malformed_titles.len(), 1);
}

#[test]
fn parse_findings_returns_empty_result_for_no_findings_marker() {
    // The review prompt instructs the agent to write `(no findings)`
    // when nothing is worth flagging. The parser must return zero
    // findings and zero malformed-titles for that input.
    let result = parse_findings("(no findings)\n");
    assert!(result.findings.is_empty());
    assert!(result.malformed_titles.is_empty());
}

#[test]
fn parse_findings_extracts_a_single_well_formed_blocker() {
    // Tracer bullet for slice 9.6 parser. Findings file with one finding
    // whose title ends in ` — blocker` per the locked grammar. Parser
    // returns one ParsedFinding with the title (sans severity tag) and
    // the severity classified into the Severity enum.
    let text = "\
## Findings

### 1. unwrap on parsed config can panic on empty input — blocker

`Config::from_str(\"\")` panics inside serde_json::from_str rather than returning the typed error.

**Suggestion:** map the serde error into ConfigError::Parse.
";
    let result = parse_findings(text);
    assert!(result.malformed_titles.is_empty(), "no malformed titles expected: {:?}", result.malformed_titles);
    assert_eq!(result.findings.len(), 1, "exactly one finding expected: {:?}", result.findings);
    let f = &result.findings[0];
    assert_eq!(f.title, "unwrap on parsed config can panic on empty input");
    assert_eq!(f.severity, Severity::Blocker);
    assert!(f.body.contains("Config::from_str"), "body must include description: {:?}", f.body);
    assert!(f.body.contains("Suggestion"), "body must include suggestion block: {:?}", f.body);
}

// ---- Slice 8: weak-test guard (has_new_tests + synthesize_no_new_tests_entry) ----

#[test]
fn has_new_tests_returns_true_for_added_plain_test_attribute() {
    // Acceptance criterion: a diff that adds a new `#[test]` line is
    // recognised as having new tests. Standard unified-diff shape:
    // file headers + hunk header + a single added line.
    let diff = "\
diff --git a/tests/new.rs b/tests/new.rs
index 0000000..1111111 100644
--- a/tests/new.rs
+++ b/tests/new.rs
@@ -0,0 +1,4 @@
+#[test]
+fn my_new_test() {
+    assert_eq!(1, 1);
+}
";
    assert!(
        has_new_tests(diff),
        "added `#[test]` line must register as a new test: {diff}"
    );
}

#[test]
fn has_new_tests_returns_true_for_added_tokio_test_attribute() {
    // The repo's existing tests use `#[tokio::test]` heavily — recognising
    // it is essential for the guard to be useful here.
    let diff = "\
diff --git a/tests/new.rs b/tests/new.rs
--- a/tests/new.rs
+++ b/tests/new.rs
@@ -0,0 +1,4 @@
+#[tokio::test]
+async fn my_async_test() {
+    assert_eq!(1, 1);
+}
";
    assert!(
        has_new_tests(diff),
        "added `#[tokio::test]` line must register as a new test: {diff}"
    );
}

#[test]
fn has_new_tests_returns_true_for_tokio_test_with_attribute_arguments() {
    // `#[tokio::test(flavor = "multi_thread")]` is a common variant. The
    // detector should still match even when the attribute carries args.
    let diff = "\
diff --git a/tests/new.rs b/tests/new.rs
--- a/tests/new.rs
+++ b/tests/new.rs
@@ -0,0 +1,2 @@
+#[tokio::test(flavor = \"multi_thread\", worker_threads = 2)]
+async fn parameterised() {}
";
    assert!(
        has_new_tests(diff),
        "parameterised `#[tokio::test(..)]` must register as a new test: {diff}"
    );
}

#[test]
fn has_new_tests_returns_false_for_diff_with_no_test_attributes() {
    // The core silent-skip case: agent wrote implementation code only,
    // no new tests. The guard must fire to force the run to
    // agent-self-reported-failure.
    let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,5 @@
 pub fn existing() {}
+
+pub fn new_function() -> i32 {
+    42
+}
";
    assert!(
        !has_new_tests(diff),
        "diff with only implementation code must NOT register as having new tests: {diff}"
    );
}

#[test]
fn has_new_tests_returns_false_when_a_test_attribute_was_only_removed() {
    // Negative-test for the +/- prefix discipline: a removed `#[test]`
    // line is NOT a new test. Without this check, a refactor that
    // renames a test by deleting one declaration and adding a different
    // (non-test) one would falsely pass.
    let diff = "\
diff --git a/tests/old.rs b/tests/old.rs
--- a/tests/old.rs
+++ b/tests/old.rs
@@ -1,4 +1,1 @@
-#[test]
-fn was_a_test() {
-    assert_eq!(1, 1);
-}
+pub fn now_a_plain_function() {}
";
    assert!(
        !has_new_tests(diff),
        "removed-only `#[test]` line must NOT register as a new test: {diff}"
    );
}

#[test]
fn has_new_tests_returns_false_for_context_lines_containing_test_attribute() {
    // Context lines (those starting with a single space) are unchanged
    // surroundings, not additions. The detector must scan only `+`
    // lines — otherwise an edit that touches code near an existing
    // `#[test]` block would falsely pass.
    let diff = "\
diff --git a/tests/existing.rs b/tests/existing.rs
--- a/tests/existing.rs
+++ b/tests/existing.rs
@@ -1,5 +1,6 @@
 #[test]
 fn existing_test() {
+    // an added line that is not itself a test attribute
     assert_eq!(1, 1);
 }
";
    assert!(
        !has_new_tests(diff),
        "context-line test attribute must NOT register as a new test: {diff}"
    );
}

#[test]
fn has_new_tests_returns_false_when_test_attribute_appears_only_inside_a_line_comment() {
    // False-positive case explicitly called out by the brief: a line
    // like `// #[test]` inside a comment is documentation, not a real
    // test attribute. The detector must skip lines whose first
    // non-whitespace content is `//`.
    let diff = "\
diff --git a/src/notes.rs b/src/notes.rs
--- a/src/notes.rs
+++ b/src/notes.rs
@@ -0,0 +1,3 @@
+// Example usage in tests:
+// #[test]
+// fn example() {}
";
    assert!(
        !has_new_tests(diff),
        "test attributes inside line comments must NOT register as new tests: {diff}"
    );
}

#[test]
fn has_new_tests_ignores_file_header_plus_plus_plus_lines() {
    // A unified diff's `+++ b/path` file-header line starts with `+`
    // and may end in `test.rs`. The detector must NOT treat it as an
    // added content line — otherwise every diff that touches a file
    // named `*test*` (e.g. `tests/foo.rs`, `src/test_helpers.rs`)
    // would falsely pass.
    let diff = "\
diff --git a/src/test_helpers.rs b/src/test_helpers.rs
--- a/src/test_helpers.rs
+++ b/src/test_helpers.rs
@@ -0,0 +1,1 @@
+pub fn helper() {}
";
    assert!(
        !has_new_tests(diff),
        "file-header `+++ b/...test*` line must NOT count as a new test attribute: {diff}"
    );
}

#[test]
fn has_new_tests_returns_true_for_test_case_parametric_variant() {
    // `#[test_case]` (and its parametric form `#[test_case(arg)]`) is
    // a common third-party test attribute. The detector should accept
    // it so a brief that asks for parametric coverage isn't penalised
    // by the guard.
    let diff = "\
diff --git a/tests/p.rs b/tests/p.rs
--- a/tests/p.rs
+++ b/tests/p.rs
@@ -0,0 +1,3 @@
+#[test_case(1 => 1; \"identity\")]
+#[test_case(2 => 4; \"doubled\")]
+fn parametric(input: u32) -> u32 { input * input.min(2) }
";
    assert!(
        has_new_tests(diff),
        "added `#[test_case(...)]` line must register as a new test: {diff}"
    );
}

#[test]
fn synthesize_no_new_tests_entry_uses_canonical_unaddressed_finding_title() {
    // Acceptance criterion: the synthesised markdown must use the
    // canonical title `no new tests added` so a future parser-as-
    // backstop iteration can cross-reference it deterministically (the
    // same verbatim-title contract the slice-9.6 backstop established).
    let entry = synthesize_no_new_tests_entry();
    assert!(
        entry.contains(&format!("## Unaddressed finding: {NO_NEW_TESTS_FINDING_TITLE}")),
        "synthesised entry must use the canonical `## Unaddressed finding: {NO_NEW_TESTS_FINDING_TITLE}` header: {entry}"
    );
    assert_eq!(
        NO_NEW_TESTS_FINDING_TITLE, "no new tests added",
        "title constant must match the brief's spelling verbatim",
    );
}

#[test]
fn synthesize_no_new_tests_entry_identifies_bellows_as_the_author() {
    // Sibling contract to synthesize_unaddressed_entries: a human
    // reading agent-notes.md must be able to tell that the entry was
    // synthesised by bellows, not written by claude. Otherwise the
    // operator could mistake a guard-driven failure for an agent-
    // initiated handoff.
    let entry = synthesize_no_new_tests_entry();
    let lower = entry.to_lowercase();
    assert!(
        lower.contains("bellows") && (lower.contains("synthes") || lower.contains("guard")),
        "synthesised entry must identify bellows as the author: {entry}"
    );
}

#[test]
fn synthesize_no_new_tests_entry_routes_through_classify_exit_to_self_reported_failure() {
    // Integration of the slice-8 guard with the existing slice-9.6
    // precedence: appending the synthesised entry to agent-notes.md
    // must, in turn, make `parse_agent_notes_sections` see an
    // Unaddressed-finding section with the canonical title. Without
    // that, `classify_exit(has_agent_notes=true, ...)` would still
    // fire (notes present), but the per-finding cross-reference any
    // future caller might run would silently miss the section. Pin
    // the round-trip here so a future "clean up the wording" PR
    // cannot accidentally break it.
    let entry = synthesize_no_new_tests_entry();
    let sections = parse_agent_notes_sections(&entry);
    assert_eq!(
        sections.len(),
        1,
        "synthesised entry must parse to exactly one Unaddressed-finding section: {sections:?}"
    );
    assert_eq!(sections[0].title, NO_NEW_TESTS_FINDING_TITLE);
}

#[test]
fn weak_test_guard_and_parser_as_backstop_entries_coexist_in_a_single_agent_notes_file() {
    // Acceptance criterion: "The slice-9.6 parser-as-backstop continues
    // to function — the weak-test guard's synthesis path does not
    // interfere with the per-finding loop's coverage-violation
    // synthesis." Both synthesis helpers produce `## Unaddressed
    // finding:` sections; the parser must see them all when both
    // pathways have appended to the same file.
    let mut notes = synthesize_no_new_tests_entry();
    notes.push_str(&synthesize_unaddressed_entries(&[
        finding("blocker silently skipped", Severity::Blocker),
        finding("important silently skipped", Severity::Important),
    ]));
    let sections = parse_agent_notes_sections(&notes);
    let titles: Vec<&str> = sections.iter().map(|s| s.title.as_str()).collect();
    assert!(
        titles.contains(&NO_NEW_TESTS_FINDING_TITLE),
        "weak-test guard section must survive coexistence: {titles:?}"
    );
    assert!(
        titles.contains(&"blocker silently skipped"),
        "parser-as-backstop section #1 must survive coexistence: {titles:?}"
    );
    assert!(
        titles.contains(&"important silently skipped"),
        "parser-as-backstop section #2 must survive coexistence: {titles:?}"
    );
    assert_eq!(sections.len(), 3, "exactly three sections expected: {sections:?}");
}
