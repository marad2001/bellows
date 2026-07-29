//! Issue #197: `runs.jsonl` must record the runs that never reached a
//! PR, not just the ones that did.
//!
//! The file's own doc comment says every terminal outcome gets a line
//! "because the failure distribution is the point of the file". It was
//! appended only after `finalise`, which is reached only once a PR
//! exists — so in the 2026-07-25 → 2026-07-28 window it held 13 lines,
//! 12 of them `Success`, while the log for the same window showed 46
//! claims against 31 finalised runs. Seven sandbox aborts, one git push
//! failure and nine refusals to claim were invisible.

use bellows::policy::{
    build_abort_metrics, build_run_metrics, AbortCause, AbortMetricsInput, ExitReason,
    PhaseTimeline, RunMetricsInput, RUN_METRICS_SCHEMA_VERSION,
};

fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(s)
        .expect("fixture timestamp")
        .with_timezone(&chrono::Utc)
}

fn abort(cause: AbortCause, phases: &[bellows::policy::PhaseMetrics]) -> serde_json::Value {
    let record = build_abort_metrics(AbortMetricsInput {
        issue: 672,
        repo: "marad2001/workboard-financial-advice",
        started_at: at("2026-07-28T17:14:00Z"),
        finished_at: at("2026-07-28T17:14:12Z"),
        cause,
        phases,
    });
    serde_json::to_value(record).expect("record should serialise")
}

// ---------------------------------------------------------------------
// The record shape
// ---------------------------------------------------------------------

#[test]
fn an_aborted_run_records_a_line_with_a_null_pr() {
    let json = abort(AbortCause::Sandbox, &[]);
    assert_eq!(json["pr"], serde_json::Value::Null, "{json}");
    assert_eq!(json["issue"], serde_json::json!(672));
    assert_eq!(
        json["repo"],
        serde_json::json!("marad2001/workboard-financial-advice"),
    );
}

#[test]
fn the_schema_is_bumped_so_a_reader_can_tell_the_shapes_apart() {
    assert_eq!(RUN_METRICS_SCHEMA_VERSION, 2);
    assert_eq!(abort(AbortCause::Sandbox, &[])["schema"], serde_json::json!(2));
}

#[test]
fn an_aborted_run_has_no_exit_reason_and_a_finalised_run_has_no_abort_cause() {
    // Exactly one of the two is set. The invariant holds by
    // construction — each builder sets one and nulls the other — and
    // this pins it so a future third constructor cannot quietly break
    // the contract a reader will rely on to bucket the file.
    let aborted = abort(AbortCause::Sandbox, &[]);
    assert_eq!(aborted["exit_reason"], serde_json::Value::Null, "{aborted}");
    assert_eq!(aborted["abort_cause"], serde_json::json!("Sandbox"));

    let timeline = PhaseTimeline::new();
    let finalised = serde_json::to_value(build_run_metrics(RunMetricsInput {
        issue: 168,
        repo: "marad2001/bellows",
        pr: 200,
        started_at: at("2026-07-25T22:04:11Z"),
        finished_at: at("2026-07-25T22:41:02Z"),
        exit_reason: &ExitReason::Success,
        merger_verdict: None,
        draft: false,
        outcome_label: "agent-done",
        phases: &timeline,
    }))
    .expect("record should serialise");
    assert_eq!(finalised["abort_cause"], serde_json::Value::Null);
    assert_eq!(finalised["exit_reason"], serde_json::json!("Success"));
    assert_eq!(finalised["pr"], serde_json::json!(200));
}

#[test]
fn an_aborted_run_has_no_draft_flag_and_no_outcome_label() {
    // Issue #193 hands the issue back to the pickup label rather than
    // leaving a terminal one on it, so there is no outcome to record.
    let json = abort(AbortCause::Sandbox, &[]);
    assert_eq!(json["draft"], serde_json::Value::Null, "{json}");
    assert_eq!(json["outcome_label"], serde_json::Value::Null, "{json}");
}

#[test]
fn the_elapsed_seconds_separate_an_early_abort_from_a_late_one() {
    // The discriminator the issue asks for: a run that died twenty
    // minutes in must not look like one that died on its first call.
    let quick = abort(AbortCause::Sandbox, &[]);
    assert_eq!(quick["wall_clock_seconds"], serde_json::json!(12));

    let slow = serde_json::to_value(build_abort_metrics(AbortMetricsInput {
        issue: 675,
        repo: "marad2001/workboard-financial-advice",
        started_at: at("2026-07-28T17:14:00Z"),
        finished_at: at("2026-07-28T17:34:00Z"),
        cause: AbortCause::Sandbox,
        phases: &[],
    }))
    .expect("serialise");
    assert_eq!(slow["wall_clock_seconds"], serde_json::json!(1200));
}

#[test]
fn phases_that_completed_before_the_abort_are_recorded() {
    let mut timeline = PhaseTimeline::new();
    timeline.record_gate_phase("post_implement_gate", 13, 0);
    let json = abort(AbortCause::Sandbox, timeline.entries());
    assert_eq!(json["phases"].as_array().map(Vec::len), Some(1), "{json}");
    assert_eq!(json["phases"][0]["phase"], serde_json::json!("post_implement_gate"));
}

#[test]
fn the_record_is_exactly_one_line() {
    let record = build_abort_metrics(AbortMetricsInput {
        issue: 672,
        repo: "marad2001/workboard-financial-advice",
        started_at: at("2026-07-28T17:14:00Z"),
        finished_at: at("2026-07-28T17:14:12Z"),
        cause: AbortCause::Sandbox,
        phases: &[],
    });
    let line = record.to_jsonl_line().expect("serialise");
    assert_eq!(line.matches('\n').count(), 1, "{line:?}");
    assert!(line.ends_with('\n'));
}

// ---------------------------------------------------------------------
// Bucketing
// ---------------------------------------------------------------------

#[test]
fn the_three_operator_responses_are_distinguishable() {
    // A daemon dropping connections, a repo that will not clone, and an
    // issue with no brief need three different actions, so they must not
    // collapse into one bucket.
    assert_eq!(
        AbortCause::from_error_shape("sandbox:docker: error reading a body from connection"),
        AbortCause::Sandbox,
    );
    assert_eq!(
        AbortCause::from_error_shape("workspace:git push failed"),
        AbortCause::Workspace,
    );
    assert_eq!(
        AbortCause::from_error_shape("missing_agent_brief:681"),
        AbortCause::Unclaimable,
    );
}

#[test]
fn every_run_error_shape_maps_to_a_bucket() {
    // These prefixes are exactly `RunError::shape()`'s vocabulary.
    for (shape, expected) in [
        ("octocrab:HTTP 502", AbortCause::GitHub),
        ("io:permission denied", AbortCause::Io),
        ("invalid_repo_url:not-a-url", AbortCause::Unclaimable),
        ("ambiguous_engine_labels:42", AbortCause::Unclaimable),
    ] {
        assert_eq!(AbortCause::from_error_shape(shape), expected, "{shape}");
    }
}

#[test]
fn an_unrecognised_shape_does_not_panic() {
    // A metrics record must never be the thing that fails a run, so an
    // unmapped shape degrades to a bucket rather than exploding.
    assert_eq!(AbortCause::from_error_shape(""), AbortCause::Io);
    assert_eq!(
        AbortCause::from_error_shape("something_new_in_a_later_release:x"),
        AbortCause::Io,
    );
}

// ---------------------------------------------------------------------
// Backwards compatibility
// ---------------------------------------------------------------------

#[test]
fn a_schema_1_line_still_deserialises_under_schema_2() {
    // AC: "Lines written by the current implementation remain parseable.
    // Nothing rewrites or migrates the existing file." This is a real
    // line from the operator's runs.jsonl, written before this change.
    let schema_1 = r#"{"schema":1,"issue":68,"repo":"marad2001/workboard-frontend-financial-advice","pr":384,"started_at":"2026-07-25T19:56:56Z","finished_at":"2026-07-25T20:19:09Z","wall_clock_seconds":1332,"exit_reason":"Success","merger_verdict":"MERGE","draft":false,"outcome_label":"agent-done","phases":[{"phase":"implement","engine":"claude","model":"claude-opus-5","seconds":691,"exit_code":0}]}"#;

    let parsed: bellows::policy::RunMetrics =
        serde_json::from_str(schema_1).expect("a schema-1 line must still parse");

    assert_eq!(parsed.schema, 1, "the stored version is preserved as written");
    assert_eq!(parsed.pr, Some(384), "a number deserialises into Some");
    assert_eq!(parsed.exit_reason, Some(ExitReason::Success));
    assert_eq!(parsed.abort_cause, None, "absent field defaults to None");
    assert_eq!(parsed.draft, Some(false));
    assert_eq!(parsed.outcome_label.as_deref(), Some("agent-done"));
    assert_eq!(parsed.phases.len(), 1);
}
