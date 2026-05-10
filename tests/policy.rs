use bellows::policy::{
    classify_exit, is_auth_error_signature, is_rate_limit_signature, render_kickoff, CheckResult,
    ExitReason, GateOutcome, ImplementOutcome, PhaseOutcomes, ReviewOutcome, REVIEW_FIX_PROMPT,
    REVIEW_PROMPT,
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
fn review_fix_prompt_makes_blocker_and_important_findings_mandatory() {
    // The address-OR-explain contract: imperative MUST language for the
    // top two severities so silent skip is prompt-out-of-bounds. PR #26
    // motivated this — the agent silently skipped an `important` finding
    // and committed nothing; the maintainer caught it manually.
    //
    // PR #28 review finding #1: a permissive substring match like
    // .contains("MUST") would still pass if the load-bearing sentence
    // got silently downgraded to "SHOULD" elsewhere (there are several
    // other MUSTs in the prompt — title-line format, etc.). Pin the
    // exact mandate clause so the regression this slice prevents is
    // actually locked.
    assert!(
        REVIEW_FIX_PROMPT
            .contains("MUST address every finding marked `blocker` or `important`"),
        "REVIEW_FIX_PROMPT must contain the literal address-OR-explain mandate clause: \
         {REVIEW_FIX_PROMPT}"
    );
    assert!(
        REVIEW_FIX_PROMPT.contains("Silent skip is not an option for these severities"),
        "REVIEW_FIX_PROMPT must spell out that silent skip is not allowed for \
         blocker/important: {REVIEW_FIX_PROMPT}"
    );
}

#[test]
fn review_fix_prompt_permits_silent_skip_of_nit_findings() {
    // The address-OR-explain rule binds blocker and important. `nit`
    // findings are explicitly opt-in: skipping one without a note is
    // fine because the operator already sees the review-findings PR
    // comment and can decide whether to follow up. The prompt must say
    // so — otherwise the imperative language bleeds into nit territory
    // and the agent burns time on cosmetic findings.
    //
    // PR #28 review finding #2: a substring match like "skip" or
    // "without explanation" doesn't pin the DIRECTION of the rule —
    // a reversed clause ("every nit must be addressed; do not skip")
    // would still match. Pin the literal permission clause so a
    // future edit can't silently flip the rule.
    assert!(
        REVIEW_FIX_PROMPT.contains("MAY skip a `nit`"),
        "REVIEW_FIX_PROMPT must literally permit skipping nit findings: \
         {REVIEW_FIX_PROMPT}"
    );
    assert!(
        REVIEW_FIX_PROMPT.contains("operator-discretionary"),
        "REVIEW_FIX_PROMPT must frame nit findings as operator-discretionary: \
         {REVIEW_FIX_PROMPT}"
    );
}

#[test]
fn review_fix_prompt_documents_agent_self_reported_failure_routing() {
    // The agent must understand the consequence of appending to
    // agent-notes.md: it routes the run to AgentSelfReportedFailure
    // (draft PR with the agent-failed label). Without this, the prompt
    // reads as "write a note when stuck" which understates the signal —
    // appending IS the escalation, and the agent should reach for it
    // deliberately.
    let lower = REVIEW_FIX_PROMPT.to_lowercase();
    assert!(
        lower.contains("agent-self-reported-failure")
            || lower.contains("draft pr with agent-failed label"),
        "REVIEW_FIX_PROMPT must surface the agent-self-reported-failure routing consequence: {REVIEW_FIX_PROMPT}"
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
