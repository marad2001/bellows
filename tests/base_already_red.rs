//! Issue #196: a cargo-gate failure is only the diff's fault if the base
//! commit was not already failing the same check.
//!
//! Four consecutive runs on `marad2001/workboard-financial-advice` —
//! issues #52, #271, #293 and #606 — died on a byte-identical
//! `doc_lazy_continuation` lint in `financial-advice-dtos`, in a file
//! none of the four concerned. #271's implement phase exited 0: the
//! agent did its job and was still labelled `agent-failed`.
//!
//! The three-state contract is what these tests mostly pin. "Could not
//! establish" must never collapse into "green", because that
//! reintroduces the misattribution in the direction nobody notices.

use bellows::policy::{
    self, BaseHealth, CheckResult, ExitReason, GateOutcome, ImplementOutcome, PhaseOutcomes,
};
use bellows::tracker::{classify_base_health, CheckRun};

fn check(name: &str, conclusion: Option<&str>) -> CheckRun {
    // `CheckRun` is deserialise-only in production; go through JSON so
    // these fixtures exercise the same shape GitHub actually returns.
    serde_json::from_value(serde_json::json!({
        "name": name,
        "conclusion": conclusion,
    }))
    .expect("CheckRun fixture")
}

fn mirrored() -> Vec<String> {
    vec!["cargo clippy".to_string(), "cargo test".to_string()]
}

const BASE: &str = "7d7ab7364443c665743fa2f3ba0f9e1eaf9baff4";

// ---------------------------------------------------------------------
// The three states
// ---------------------------------------------------------------------

#[test]
fn a_failing_mirrored_check_at_base_is_red_and_names_the_check() {
    let health = classify_base_health(
        &[
            check("cargo clippy", Some("failure")),
            check("cargo test", Some("success")),
        ],
        BASE,
        &mirrored(),
    );
    match health {
        BaseHealth::Red {
            base_sha,
            failing_check,
        } => {
            assert_eq!(base_sha, BASE, "the operator must be able to verify the claim");
            assert_eq!(failing_check, "cargo clippy");
        }
        other => panic!("expected Red, got {other:?}"),
    }
}

#[test]
fn all_mirrored_checks_passing_at_base_is_green() {
    let health = classify_base_health(
        &[
            check("cargo clippy", Some("success")),
            check("cargo test", Some("success")),
        ],
        BASE,
        &mirrored(),
    );
    assert_eq!(health, BaseHealth::Green);
}

#[test]
fn checks_still_running_at_base_are_not_established_not_green() {
    // Observed live: `GET /commits/{sha}/check-runs` on a just-pushed
    // main returns `"conclusion": null` for every job. Reading that as
    // green would blame the diff on the strength of no evidence.
    let health = classify_base_health(
        &[check("cargo clippy", None), check("cargo test", None)],
        BASE,
        &mirrored(),
    );
    assert_eq!(
        health,
        BaseHealth::NotEstablished,
        "a null conclusion is 'not yet known', never 'passed'",
    );
}

#[test]
fn a_repo_with_no_matching_checks_is_not_established() {
    let health = classify_base_health(
        &[
            check("build", Some("success")),
            check("deploy", Some("success")),
        ],
        BASE,
        &mirrored(),
    );
    assert_eq!(health, BaseHealth::NotEstablished);
}

#[test]
fn no_check_runs_at_all_is_not_established() {
    assert_eq!(
        classify_base_health(&[], BASE, &mirrored()),
        BaseHealth::NotEstablished,
    );
}

#[test]
fn no_mirrored_check_names_is_not_established() {
    // The gate fell back to operator-declared `[gates]` flags, so it is
    // not mirroring CI and there is no job its failure corresponds to.
    let health = classify_base_health(&[check("cargo clippy", Some("failure"))], BASE, &[]);
    assert_eq!(health, BaseHealth::NotEstablished);
}

// ---------------------------------------------------------------------
// The mirrored-checks-only filter
// ---------------------------------------------------------------------

#[test]
fn an_unrelated_failing_check_does_not_make_the_base_red() {
    // AC: "An unrelated failing check on the base — a docs job, a deploy
    // step — must not be read as 'the base is broken' for a clippy
    // failure."
    let health = classify_base_health(
        &[
            check("deploy to staging", Some("failure")),
            check("cargo clippy", Some("success")),
            check("cargo test", Some("success")),
        ],
        BASE,
        &mirrored(),
    );
    assert_eq!(
        health,
        BaseHealth::Green,
        "only checks the gate mirrors may speak to the base's health",
    );
}

#[test]
fn a_sibling_job_the_gate_does_not_run_is_not_treated_as_mirrored() {
    // `cargo test --examples` is a real job on
    // workboard-financial-advice and is NOT what the gate runs. A
    // substring match would let it stand in for `cargo test`, and a
    // false Red suppresses a FinalTestsRed on a diff that really is
    // broken — the dangerous direction.
    let health = classify_base_health(
        &[
            check("cargo test --examples", Some("failure")),
            check("cargo clippy", Some("success")),
            check("cargo test", Some("success")),
        ],
        BASE,
        &mirrored(),
    );
    assert_eq!(health, BaseHealth::Green);
}

#[test]
fn a_workflow_qualified_check_name_still_matches() {
    // GitHub prefixes check names with `workflow / job` in some repo
    // configurations.
    let health = classify_base_health(
        &[check("CI / cargo clippy", Some("failure"))],
        BASE,
        &mirrored(),
    );
    assert!(
        matches!(health, BaseHealth::Red { .. }),
        "expected Red, got {health:?}",
    );
}

#[test]
fn skipped_and_cancelled_checks_establish_nothing() {
    // Neither ran the code, so neither says the base is healthy.
    let health = classify_base_health(
        &[
            check("cargo clippy", Some("skipped")),
            check("cargo test", Some("cancelled")),
        ],
        BASE,
        &mirrored(),
    );
    assert_eq!(health, BaseHealth::NotEstablished);
}

// ---------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------

/// Clippy exiting 101 — the shape all four workboard-financial-advice
/// runs took when `financial-advice-dtos` failed to compile.
fn failing_gate() -> GateOutcome {
    GateOutcome {
        cargo_clippy: Some(CheckResult {
            exit_code: 101,
            output: "error: could not compile `financial-advice-dtos` (lib)".to_string(),
        }),
        cargo_test: None,
    }
}

fn outcomes_with(base_health: BaseHealth, implement_exit: i64) -> PhaseOutcomes {
    PhaseOutcomes {
        implement: ImplementOutcome {
            exit_code: implement_exit,
            ..Default::default()
        },
        post_implement_gate: failing_gate(),
        base_health,
        ..Default::default()
    }
}

#[test]
fn a_gate_failure_on_a_red_base_classifies_as_base_already_red() {
    let reason = policy::classify_exit(&outcomes_with(
        BaseHealth::Red {
            base_sha: BASE.to_string(),
            failing_check: "cargo clippy".to_string(),
        },
        0,
    ));
    assert_eq!(reason, ExitReason::BaseAlreadyRed);
}

#[test]
fn a_gate_failure_on_a_green_base_still_classifies_as_final_tests_red() {
    // The whole point: this must not change how a genuinely broken diff
    // is reported.
    let reason = policy::classify_exit(&outcomes_with(BaseHealth::Green, 0));
    assert_eq!(reason, ExitReason::FinalTestsRed);
}

#[test]
fn a_gate_failure_on_an_unestablished_base_still_classifies_as_final_tests_red() {
    let reason = policy::classify_exit(&outcomes_with(BaseHealth::NotEstablished, 0));
    assert_eq!(
        reason,
        ExitReason::FinalTestsRed,
        "not knowing must classify exactly as it did before issue #196",
    );
}

#[test]
fn the_271_case_a_healthy_agent_against_a_red_base_is_not_blamed() {
    // Issue #271 / PR #637: implement exited 0, the agent did the work,
    // and the run was labelled `agent-failed` on someone else's lint.
    let reason = policy::classify_exit(&outcomes_with(
        BaseHealth::Red {
            base_sha: BASE.to_string(),
            failing_check: "cargo clippy".to_string(),
        },
        0,
    ));
    assert_eq!(reason, ExitReason::BaseAlreadyRed);
}

#[test]
fn an_implement_crash_still_outranks_the_base_verdict() {
    // A run whose engine crashed did not fail *because* the base was
    // red — it never produced a diff for the gate to judge. `Crash` is
    // the honest attribution and keeps its existing precedence.
    let reason = policy::classify_exit(&outcomes_with(
        BaseHealth::Red {
            base_sha: BASE.to_string(),
            failing_check: "cargo clippy".to_string(),
        },
        1,
    ));
    assert_eq!(reason, ExitReason::Crash);
}

#[test]
fn a_passing_gate_is_unaffected_by_base_health() {
    // Defensive: even if a `Red` verdict were somehow present without a
    // gate failure, a clean run must still be a Success.
    let outcomes = PhaseOutcomes {
        base_health: BaseHealth::Red {
            base_sha: BASE.to_string(),
            failing_check: "cargo clippy".to_string(),
        },
        ..Default::default()
    };
    assert_eq!(policy::classify_exit(&outcomes), ExitReason::Success);
}

#[test]
fn an_end_pipeline_gate_failure_on_a_red_base_also_routes_to_base_already_red() {
    let outcomes = PhaseOutcomes {
        end_pipeline_gate: Some(failing_gate()),
        base_health: BaseHealth::Red {
            base_sha: BASE.to_string(),
            failing_check: "cargo test".to_string(),
        },
        ..Default::default()
    };
    assert_eq!(policy::classify_exit(&outcomes), ExitReason::BaseAlreadyRed);
}

// ---------------------------------------------------------------------
// The lookup must not fire on the happy path
// ---------------------------------------------------------------------

#[test]
fn a_passing_gate_never_triggers_the_base_health_lookup() {
    // AC: "A passing gate never triggers the lookup. The happy path is
    // exactly as fast as today." Twelve of the thirteen recorded runs in
    // the window that motivated this issue passed their gates; none of
    // them should spend a request.
    assert!(!policy::should_consult_base_health(
        &GateOutcome::default(),
        None,
    ));
    let passing = GateOutcome {
        cargo_clippy: Some(CheckResult {
            exit_code: 0,
            output: String::new(),
        }),
        cargo_test: Some(CheckResult {
            exit_code: 0,
            output: String::new(),
        }),
    };
    assert!(!policy::should_consult_base_health(&passing, Some(&passing)));
}

#[test]
fn either_failing_gate_triggers_the_base_health_lookup() {
    assert!(
        policy::should_consult_base_health(&failing_gate(), None),
        "a failing post-implement gate poses the question",
    );
    assert!(
        policy::should_consult_base_health(&GateOutcome::default(), Some(&failing_gate())),
        "so does a failing end-pipeline gate, which runs after the fixups",
    );
}

// ---------------------------------------------------------------------
// Which names the lookup asks about.
//
// GitHub names a check-run after the **job**, not the step. Deriving the
// name from the gate command instead — `cargo clippy --locked ...` → the
// key `cargo clippy` — only ever matched on a repo that happens to name
// its jobs after the commands they run. `workboard-financial-advice`
// does, which is why the fixtures above are shaped that way and why the
// heuristic shipped looking correct. Every other configured repo names
// the job `ci`, so nothing matched, `relevant` was empty, and the whole
// feature returned `NotEstablished` on every run with its tests green.
// ---------------------------------------------------------------------

fn gate_commands(
    clippy_check: Option<&str>,
    test_check: Option<&str>,
) -> bellows::workspace::GateCommands {
    use bellows::workflow_parse::Provenance;
    let provenance = |check: Option<&str>| match check {
        Some(_) => Provenance::ParsedFromWorkflow(std::path::PathBuf::from(
            ".github/workflows/ci.yml",
        )),
        None => Provenance::FallbackFromConfig,
    };
    bellows::workspace::GateCommands {
        clippy: "cargo clippy --locked --all-targets".to_string(),
        clippy_source: provenance(clippy_check),
        clippy_env: Vec::new(),
        clippy_check: clippy_check.map(str::to_string),
        test: "cargo test --locked --all-targets".to_string(),
        test_source: provenance(test_check),
        test_env: Vec::new(),
        test_check: test_check.map(str::to_string),
    }
}

#[test]
fn a_single_job_workflow_asks_about_that_job_not_the_commands() {
    // The shape that was silently broken: one job called `ci` running
    // both commands. The lookup must ask GitHub about `ci`.
    let names = bellows::tracker::mirrored_check_names(&gate_commands(Some("ci"), Some("ci")));
    assert_eq!(names, vec!["ci".to_string()], "one job means one check to ask about");
}

#[test]
fn sibling_jobs_are_both_asked_about() {
    let names = bellows::tracker::mirrored_check_names(&gate_commands(
        Some("cargo clippy"),
        Some("cargo test"),
    ));
    assert_eq!(names, vec!["cargo clippy".to_string(), "cargo test".to_string()]);
}

#[test]
fn a_fallback_command_contributes_no_check_name() {
    // Unchanged contract: bellows invented that command's posture, so no
    // check-run on the repo corresponds to it and guessing one would
    // answer a question about a check it never mirrored.
    assert!(bellows::tracker::mirrored_check_names(&gate_commands(None, None)).is_empty());
    assert_eq!(
        bellows::tracker::mirrored_check_names(&gate_commands(None, Some("ci"))),
        vec!["ci".to_string()],
    );
}

#[test]
fn a_single_job_running_both_commands_is_asked_about_once() {
    let names = bellows::tracker::mirrored_check_names(&gate_commands(Some("ci"), Some("ci")));
    assert_eq!(names.len(), 1, "asking GitHub about `ci` twice is noise: {names:?}");
}

#[test]
fn a_matrix_legs_axis_suffix_still_matches_its_job() {
    // A matrix job's check-runs carry their axis values —
    // `ci (ubuntu-latest)`. One leg failing is the job failing.
    let health = classify_base_health(
        &[check("ci (ubuntu-latest)", Some("failure"))],
        BASE,
        &["ci".to_string()],
    );
    assert!(
        matches!(health, BaseHealth::Red { .. }),
        "a failing matrix leg is the job failing: {health:?}",
    );
}

#[test]
fn a_workflow_prefixed_check_name_still_matches_its_job() {
    // GitHub prefixes `workflow / job` on checks required through a
    // ruleset. Unchanged behaviour, re-pinned against the job-name path.
    let health =
        classify_base_health(&[check("CI / ci", Some("success"))], BASE, &["ci".to_string()]);
    assert!(matches!(health, BaseHealth::Green), "{health:?}");
}

#[test]
fn an_unrelated_job_failing_at_base_is_still_not_the_gates_business() {
    // The filter's whole point survives the change: a red deploy job is
    // not evidence that the clippy the gate mirrors was already failing.
    let health = classify_base_health(
        &[
            check("deploy", Some("failure")),
            check("ci", Some("success")),
        ],
        BASE,
        &["ci".to_string()],
    );
    assert!(matches!(health, BaseHealth::Green), "{health:?}");
}
