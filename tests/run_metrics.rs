//! Issue #168: the append-only per-run metrics record (`runs.jsonl`).
//!
//! Two surfaces are covered here:
//!
//!  - `policy::build_run_metrics` — the pure constructor that turns the
//!    data already in hand at finalisation (issue/PR identity, exit
//!    classification, merger verdict, and the per-phase timeline the
//!    pipeline recorded as it ran) into one serialisable record. Pure so
//!    it is testable without Docker or GitHub, matching how
//!    `render_kickoff` and `classify_exit` are already testable.
//!  - `runner::append_run_metrics` — the best-effort append. Nothing in
//!    the run gates on it, so a failure must log and continue rather
//!    than propagate.

use std::io::Cursor;

use bellows::config::{ChainEntry, Engine, RuntimeLabelsConfig};
use bellows::policy::{
    build_run_metrics, gate_exit_code, CheckResult, ExitReason, GateOutcome, MergerVerdict,
    PhaseTimeline, RunMetrics, RunMetricsInput, RUN_METRICS_SCHEMA_VERSION,
};
use bellows::runner::effective_terminal_outcome;

fn ts(rfc3339: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .expect("test timestamp should be valid RFC 3339")
        .with_timezone(&chrono::Utc)
}

fn entry(engine: Engine, model: Option<&str>) -> ChainEntry {
    ChainEntry {
        engine,
        model: model.map(str::to_string),
    }
}

/// The happy path: implement on claude, a cargo gate, then review on a
/// different engine. Mirrors the record shape in the issue #168 brief.
fn successful_multi_phase_timeline() -> PhaseTimeline {
    let mut timeline = PhaseTimeline::new();
    timeline.record_engine_phase(
        "implement",
        Some(&entry(Engine::Claude, Some("opus-4-7"))),
        1401,
        0,
    );
    timeline.record_gate_phase("post_implement_gate", 122, 0);
    timeline.record_engine_phase(
        "review",
        Some(&entry(Engine::Codex, Some("gpt-5.5"))),
        380,
        0,
    );
    timeline
}

fn successful_run_metrics() -> RunMetrics {
    build_run_metrics(RunMetricsInput {
        issue: 168,
        repo: "marad2001/bellows",
        pr: 200,
        started_at: ts("2026-07-25T22:04:11Z"),
        finished_at: ts("2026-07-25T22:41:02Z"),
        exit_reason: &ExitReason::Success,
        merger_verdict: Some(MergerVerdict::Merge),
        draft: false,
        outcome_label: "agent-done",
        phases: &successful_multi_phase_timeline(),
    })
}

#[test]
fn run_metrics_serialises_to_a_single_json_line_that_round_trips() {
    // AC1: the file is JSON *lines* — one record per line — so the
    // serialised form must carry exactly one newline, at the very end,
    // and must survive a round-trip through a reader (#167).
    let metrics = successful_run_metrics();
    let line = metrics
        .to_jsonl_line()
        .expect("a plain record should serialise");

    assert_eq!(
        line.matches('\n').count(),
        1,
        "record must occupy exactly one line: {line}",
    );
    assert!(
        line.ends_with('\n'),
        "the single newline must terminate the line: {line}",
    );

    let parsed: RunMetrics =
        serde_json::from_str(line.trim_end()).expect("the line should round-trip");
    assert_eq!(parsed, metrics);
}

#[test]
fn run_metrics_carries_the_brief_contract_fields() {
    // The brief names these field names as the contract; a rename is a
    // breaking change for any reader built on #167.
    let json: serde_json::Value =
        serde_json::to_value(successful_run_metrics()).expect("record should serialise");

    assert_eq!(json["schema"], serde_json::json!(RUN_METRICS_SCHEMA_VERSION));
    assert_eq!(json["schema"], serde_json::json!(1));
    assert_eq!(json["issue"], serde_json::json!(168));
    assert_eq!(json["repo"], serde_json::json!("marad2001/bellows"));
    assert_eq!(json["pr"], serde_json::json!(200));
    assert_eq!(json["started_at"], serde_json::json!("2026-07-25T22:04:11Z"));
    assert_eq!(
        json["finished_at"],
        serde_json::json!("2026-07-25T22:41:02Z"),
    );
    // 22:04:11 → 22:41:02 is 36m51s.
    assert_eq!(json["wall_clock_seconds"], serde_json::json!(2211));
    assert_eq!(json["draft"], serde_json::json!(false));
    assert_eq!(json["outcome_label"], serde_json::json!("agent-done"));
}

#[test]
fn a_successful_multi_phase_run_records_only_the_phases_that_ran_in_order() {
    // AC2: execution order is the array order, and a gate phase carries
    // explicit nulls for engine/model rather than omitting the keys —
    // a reader can then treat "key absent" as a schema problem.
    let json: serde_json::Value =
        serde_json::to_value(successful_run_metrics()).expect("record should serialise");
    let phases = json["phases"]
        .as_array()
        .expect("phases must serialise as an array");

    assert_eq!(phases.len(), 3, "only the three phases that ran: {phases:?}");
    assert_eq!(
        phases
            .iter()
            .map(|p| p["phase"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["implement", "post_implement_gate", "review"],
    );

    assert_eq!(phases[0]["engine"], serde_json::json!("claude"));
    assert_eq!(phases[0]["model"], serde_json::json!("opus-4-7"));
    assert_eq!(phases[0]["seconds"], serde_json::json!(1401));
    assert_eq!(phases[0]["exit_code"], serde_json::json!(0));

    assert!(
        phases[1].get("engine").is_some(),
        "gate phases keep the engine key present: {:?}",
        phases[1],
    );
    assert_eq!(phases[1]["engine"], serde_json::Value::Null);
    assert!(
        phases[1].get("model").is_some(),
        "gate phases keep the model key present: {:?}",
        phases[1],
    );
    assert_eq!(phases[1]["model"], serde_json::Value::Null);
    assert_eq!(phases[1]["seconds"], serde_json::json!(122));

    assert_eq!(phases[2]["engine"], serde_json::json!("codex"));
    assert_eq!(phases[2]["model"], serde_json::json!("gpt-5.5"));
}

#[test]
fn an_engine_phase_with_no_model_pin_serialises_a_null_model() {
    // `model: None` in a chain entry means "the CLI's default model —
    // bellows omits the `-m` flag". The record says null, not "".
    let mut timeline = PhaseTimeline::new();
    timeline.record_engine_phase("implement", Some(&entry(Engine::Opencode, None)), 12, 0);
    let json = serde_json::to_value(build_run_metrics(RunMetricsInput {
        issue: 1,
        repo: "o/r",
        pr: 2,
        started_at: ts("2026-07-25T22:04:11Z"),
        finished_at: ts("2026-07-25T22:04:23Z"),
        exit_reason: &ExitReason::Success,
        merger_verdict: None,
        draft: false,
        outcome_label: "agent-done",
        phases: &timeline,
    }))
    .expect("record should serialise");

    assert_eq!(json["phases"][0]["engine"], serde_json::json!("opencode"));
    assert_eq!(json["phases"][0]["model"], serde_json::Value::Null);
}

#[test]
fn a_run_that_halted_after_implement_omits_the_phases_that_never_ran() {
    // AC3: implement crashed, so review / security / merger never ran.
    // They are omitted rather than emitted with placeholder values — a
    // reader counting phase frequencies must not see a phantom review.
    let mut timeline = PhaseTimeline::new();
    timeline.record_engine_phase(
        "implement",
        Some(&entry(Engine::Claude, Some("opus-4-7"))),
        41,
        1,
    );
    let json = serde_json::to_value(build_run_metrics(RunMetricsInput {
        issue: 42,
        repo: "marad2001/bellows",
        pr: 77,
        started_at: ts("2026-07-25T22:04:11Z"),
        finished_at: ts("2026-07-25T22:04:52Z"),
        exit_reason: &ExitReason::Crash,
        merger_verdict: None,
        draft: true,
        outcome_label: "agent-failed",
        phases: &timeline,
    }))
    .expect("record should serialise");

    let phases = json["phases"].as_array().expect("phases is an array");
    assert_eq!(phases.len(), 1, "only implement ran: {phases:?}");
    assert_eq!(phases[0]["phase"], serde_json::json!("implement"));
    assert_eq!(phases[0]["exit_code"], serde_json::json!(1));
    assert_eq!(json["exit_reason"], serde_json::json!("Crash"));
    assert_eq!(json["draft"], serde_json::json!(true));
    assert_eq!(json["outcome_label"], serde_json::json!("agent-failed"));
}

#[test]
fn a_phase_no_engine_ever_served_is_omitted_from_the_record() {
    // The all-chain-entries-cooling path reaches a phase but never
    // dispatches a container: no engine served it, so per AC3 the phase
    // is omitted rather than recorded with a placeholder engine.
    let mut timeline = PhaseTimeline::new();
    timeline.record_engine_phase("implement", None, 0, 0);

    assert!(
        timeline.entries().is_empty(),
        "a phase no engine served must not be recorded: {:?}",
        timeline.entries(),
    );
}

#[test]
fn exit_reason_serialises_the_variant_name_verbatim() {
    // AC4: three-plus variants, so an operator grepping `runs.jsonl`
    // for a failure distribution matches on the same strings the
    // `ExitReason` enum uses.
    for (reason, expected) in [
        (ExitReason::Success, "Success"),
        (ExitReason::Crash, "Crash"),
        (ExitReason::RateLimited, "RateLimited"),
        (ExitReason::WallClockExceeded, "WallClockExceeded"),
        (ExitReason::AgentSelfReportedFailure, "AgentSelfReportedFailure"),
        (ExitReason::FinalTestsRed, "FinalTestsRed"),
        (ExitReason::AuthError, "AuthError"),
        (ExitReason::Cancelled, "Cancelled"),
    ] {
        let json = serde_json::to_value(build_run_metrics(RunMetricsInput {
            issue: 1,
            repo: "o/r",
            pr: 2,
            started_at: ts("2026-07-25T22:04:11Z"),
            finished_at: ts("2026-07-25T22:04:12Z"),
            exit_reason: &reason,
            merger_verdict: None,
            draft: false,
            outcome_label: "agent-done",
            phases: &PhaseTimeline::new(),
        }))
        .expect("record should serialise");
        assert_eq!(json["exit_reason"], serde_json::json!(expected));
    }
}

#[test]
fn merger_verdict_serialises_the_parsed_token_or_null() {
    // AC4: the existing `Option<MergerVerdict>` maps straight onto the
    // field — the three tokens verbatim, and `null` when the merger
    // produced nothing parseable (or never ran).
    for (verdict, expected) in [
        (Some(MergerVerdict::Merge), serde_json::json!("MERGE")),
        (Some(MergerVerdict::HoldNoted), serde_json::json!("HOLD-NOTED")),
        (Some(MergerVerdict::HoldDraft), serde_json::json!("HOLD-DRAFT")),
        (None, serde_json::Value::Null),
    ] {
        let json = serde_json::to_value(build_run_metrics(RunMetricsInput {
            issue: 1,
            repo: "o/r",
            pr: 2,
            started_at: ts("2026-07-25T22:04:11Z"),
            finished_at: ts("2026-07-25T22:04:12Z"),
            exit_reason: &ExitReason::Success,
            merger_verdict: verdict,
            draft: false,
            outcome_label: "agent-done",
            phases: &PhaseTimeline::new(),
        }))
        .expect("record should serialise");
        assert_eq!(json["merger_verdict"], expected);
        assert!(
            json.get("merger_verdict").is_some(),
            "the key stays present even for the null verdict: {json}",
        );
    }
}

#[test]
fn the_serialised_verdict_token_cannot_drift_from_as_token() {
    // `MergerVerdict::as_token` renders the run-log line and the
    // `## Merge verdict` comment; the serde renames render the record.
    // They must stay the same vocabulary, so pin them to each other.
    for verdict in [
        MergerVerdict::Merge,
        MergerVerdict::HoldNoted,
        MergerVerdict::HoldDraft,
    ] {
        assert_eq!(
            serde_json::to_value(verdict).expect("verdict should serialise"),
            serde_json::json!(verdict.as_token()),
        );
    }
}

#[test]
fn wall_clock_seconds_never_goes_negative_on_a_clock_step() {
    // A wall-clock that steps backwards mid-run (NTP correction) must
    // not produce a negative duration in a machine-read field.
    let json = serde_json::to_value(build_run_metrics(RunMetricsInput {
        issue: 1,
        repo: "o/r",
        pr: 2,
        started_at: ts("2026-07-25T22:04:11Z"),
        finished_at: ts("2026-07-25T22:04:01Z"),
        exit_reason: &ExitReason::Success,
        merger_verdict: None,
        draft: false,
        outcome_label: "agent-done",
        phases: &PhaseTimeline::new(),
    }))
    .expect("record should serialise");
    assert_eq!(json["wall_clock_seconds"], serde_json::json!(0));
}

#[test]
fn a_gate_phases_exit_code_is_the_check_that_actually_failed() {
    // The gate rows in the record carry one exit code, but a gate runs
    // two checks. Clippy runs first and short-circuits the gate, so its
    // non-zero code is the one that explains the failure; a green clippy
    // defers to test; an all-green gate is 0.
    let check = |exit_code: i64| CheckResult {
        exit_code,
        output: String::new(),
    };

    assert_eq!(gate_exit_code(&GateOutcome::default()), 0);
    assert_eq!(
        gate_exit_code(&GateOutcome {
            cargo_clippy: Some(check(0)),
            cargo_test: Some(check(0)),
        }),
        0,
    );
    assert_eq!(
        gate_exit_code(&GateOutcome {
            cargo_clippy: Some(check(101)),
            // Clippy failing short-circuits the gate, so test is `None`.
            cargo_test: None,
        }),
        101,
    );
    assert_eq!(
        gate_exit_code(&GateOutcome {
            cargo_clippy: Some(check(0)),
            cargo_test: Some(check(101)),
        }),
        101,
    );
    // Both failing is not a shape the gate produces (clippy failing
    // short-circuits), but if it ever did, the first failure wins.
    assert_eq!(
        gate_exit_code(&GateOutcome {
            cargo_clippy: Some(check(4)),
            cargo_test: Some(check(101)),
        }),
        4,
    );
}

// ---------------------------------------------------------------------
// The append helper
// ---------------------------------------------------------------------

#[test]
fn appending_twice_leaves_two_parseable_lines_and_disturbs_neither() {
    // AC5: the file is append-only and never rewritten, so a second
    // run's record must land after the first with the first byte-for-
    // byte intact.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let path = dir.path().join("runs.jsonl");
    let mut log = Cursor::new(Vec::new());

    let first = successful_run_metrics();
    bellows::runner::append_run_metrics(&path, &first, &mut log);
    let after_first = std::fs::read_to_string(&path).expect("file should exist after first append");

    let mut timeline = PhaseTimeline::new();
    timeline.record_engine_phase("implement", Some(&entry(Engine::Codex, None)), 9, 1);
    let second = build_run_metrics(RunMetricsInput {
        issue: 169,
        repo: "marad2001/bellows",
        pr: 201,
        started_at: ts("2026-07-25T23:00:00Z"),
        finished_at: ts("2026-07-25T23:00:09Z"),
        exit_reason: &ExitReason::Crash,
        merger_verdict: None,
        draft: true,
        outcome_label: "agent-failed",
        phases: &timeline,
    });
    bellows::runner::append_run_metrics(&path, &second, &mut log);

    let body = std::fs::read_to_string(&path).expect("file should exist after second append");
    assert!(
        body.starts_with(&after_first),
        "the first record must be untouched by the second append: {body}",
    );

    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 2, "one line per run: {body}");
    let parsed: Vec<RunMetrics> = lines
        .iter()
        .map(|l| serde_json::from_str(l).expect("every line should parse"))
        .collect();
    assert_eq!(parsed, vec![first, second]);
}

#[test]
fn a_failing_append_logs_a_warning_and_does_not_panic_or_return_an_error() {
    // AC6: the append is best-effort. A path pointing at a directory
    // can never be opened for append, which stands in for the
    // permissions / full-disk cases. The helper returns `()` — there is
    // no error channel for a caller to accidentally gate on — and the
    // operator learns about it from the log.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let mut log = Cursor::new(Vec::new());

    bellows::runner::append_run_metrics(dir.path(), &successful_run_metrics(), &mut log);

    let logged = String::from_utf8(log.into_inner()).expect("log should be utf-8");
    assert!(
        logged.contains("bellows:") && logged.to_lowercase().contains("metrics"),
        "the failure must be operator-visible on the run log: {logged}",
    );
    assert!(
        logged.contains(&dir.path().display().to_string()),
        "the warning should name the path that failed: {logged}",
    );
}

#[test]
fn a_successful_append_stays_silent_on_the_run_log() {
    // The happy path must not add noise to a log an operator tails.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let mut log = Cursor::new(Vec::new());
    bellows::runner::append_run_metrics(
        &dir.path().join("runs.jsonl"),
        &successful_run_metrics(),
        &mut log,
    );
    assert_eq!(
        String::from_utf8(log.into_inner()).expect("log should be utf-8"),
        "",
    );
}

// ---------------------------------------------------------------------
// Late cancellation — the record must follow finalise, not the pipeline
//
// `run_once` computes its exit reason before opening the PR, but
// `tracker::finalise`'s GET is the last word on cancellation: an
// operator can `bellows kill <N>` in the window between the pre-PR check
// and finalisation. That run returns `RunOutcome::Cancelled`, so the
// `runs.jsonl` line has to agree — otherwise the file permanently
// reports `Success` / `agent-done` for a cancelled run and the failure
// distribution the file exists for is wrong.
// ---------------------------------------------------------------------

#[test]
fn a_cancellation_seen_only_by_finalise_overrides_the_pipeline_classification() {
    let labels = RuntimeLabelsConfig::default();
    let (reason, outcome_label) =
        effective_terminal_outcome(&ExitReason::Success, true, &labels);

    assert_eq!(reason, ExitReason::Cancelled);
    assert_eq!(outcome_label, labels.agent_cancelled);
}

#[test]
fn without_a_late_cancellation_the_pipeline_classification_passes_through() {
    let labels = RuntimeLabelsConfig::default();

    for (pipeline_reason, expected_label) in [
        (ExitReason::Success, &labels.agent_done),
        (ExitReason::Crash, &labels.agent_failed),
        (ExitReason::RateLimited, &labels.agent_rate_limited),
        // A cancellation the *pre-PR* check already caught stays
        // cancelled even though finalise's own flag is false (the label
        // PATCH ran normally in that case).
        (ExitReason::Cancelled, &labels.agent_cancelled),
    ] {
        let (reason, outcome_label) =
            effective_terminal_outcome(&pipeline_reason, false, &labels);
        assert_eq!(reason, pipeline_reason);
        assert_eq!(&outcome_label.to_string(), expected_label);
    }
}

#[test]
fn a_late_cancelled_run_emits_a_cancelled_record_with_the_landed_label() {
    // End-to-end over the two surfaces `run_once` composes after
    // `finalise` returns: derive the authoritative outcome, build the
    // record from it, append it. The emitted line must say `Cancelled`
    // and carry the label the issue actually landed on, while `draft`
    // still reports the PR state the run really opened (the PR predates
    // finalise, so a late cancellation cannot retroactively draft it).
    let labels = RuntimeLabelsConfig::default();
    let (reason, outcome_label) =
        effective_terminal_outcome(&ExitReason::Success, true, &labels);

    let dir = tempfile::TempDir::new().expect("temp dir");
    let path = dir.path().join("runs.jsonl");
    let mut log = Cursor::new(Vec::new());
    bellows::runner::append_run_metrics(
        &path,
        &build_run_metrics(RunMetricsInput {
            issue: 168,
            repo: "marad2001/bellows",
            pr: 200,
            started_at: ts("2026-07-25T22:04:11Z"),
            finished_at: ts("2026-07-25T22:41:02Z"),
            exit_reason: &reason,
            merger_verdict: Some(MergerVerdict::Merge),
            draft: false,
            outcome_label,
            phases: &successful_multi_phase_timeline(),
        }),
        &mut log,
    );

    let body = std::fs::read_to_string(&path).expect("the record should have been written");
    let json: serde_json::Value =
        serde_json::from_str(body.trim_end()).expect("the line should parse");
    assert_eq!(json["exit_reason"], serde_json::json!("Cancelled"));
    assert_eq!(json["outcome_label"], serde_json::json!("agent-cancelled"));
    assert_eq!(json["draft"], serde_json::json!(false));
}

#[test]
fn run_once_appends_the_record_after_finalise_has_returned() {
    // Ordering guard. The whole point of deriving the record from
    // `finalise`'s outcome is that the append happens *after* the call
    // that produces it — an append moved back above `tracker::finalise`
    // would still compile and every assertion above would still pass,
    // but the run would be back to recording the stale pre-finalise
    // classification. `run_once` needs Docker and GitHub to reach this
    // code, so the call order is asserted at the source level.
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/runner.rs"))
        .expect("runner.rs should be readable");
    let finalise_at = src
        .find("tracker::finalise(")
        .expect("run_once should call tracker::finalise");
    // Indented, so this is the call inside `run_once` and not the
    // top-level `pub fn append_run_metrics` definition.
    let append_at = src
        .find("    append_run_metrics(")
        .expect("run_once should call append_run_metrics");

    assert!(
        append_at > finalise_at,
        "the metrics append must follow tracker::finalise so the record \
         can carry the authoritative cancellation outcome",
    );
}
