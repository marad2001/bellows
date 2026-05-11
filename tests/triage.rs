//! Slice T2 (#22): `bellows triage` backlog drain — verdict tally,
//! summary formatting, and the serial iteration loop's contract.
//!
//! The per-issue triage path (T1 / issue #21) is injected as an
//! async closure so the drain logic can be tested without spawning
//! containers. The "calls T1's per-issue triage path serially with
//! workspace state flowing between issues" property is implicit in
//! the serial-await loop — the test pins the visit order, the
//! per-issue isolation, and the dry-run flag propagation.

use std::sync::{Arc, Mutex};

use bellows::tracker::Issue;
use bellows::triage::{drain_backlog, BacklogSummary, Verdict};

fn issue(number: u64, title: &str) -> Issue {
    Issue {
        number,
        title: title.to_string(),
        labels: vec![],
    }
}

#[test]
fn verdict_label_renders_canonical_strings_from_triage_labels_doc() {
    // The four canonical roles in docs/agents/triage-labels.md must
    // map 1:1 onto Verdict's `label()` output — the apply step and the
    // summary printer both depend on this contract.
    assert_eq!(Verdict::ReadyForAgent.label(), "ready-for-agent");
    assert_eq!(Verdict::NeedsInfo.label(), "needs-info");
    assert_eq!(Verdict::ReadyForHuman.label(), "ready-for-human");
    assert_eq!(Verdict::WontfixEnhancement.label(), "wontfix-enhancement");
}

#[test]
fn backlog_summary_starts_empty() {
    let s = BacklogSummary::default();
    assert_eq!(s.total(), 0);
    assert_eq!(s.ready_for_agent, 0);
    assert_eq!(s.needs_info, 0);
    assert_eq!(s.wontfix_enhancement, 0);
    assert_eq!(s.ready_for_human, 0);
    assert_eq!(s.failed, 0);
}

#[test]
fn backlog_summary_tallies_each_verdict_into_its_own_bucket() {
    let mut s = BacklogSummary::default();
    s.record_verdict(Verdict::ReadyForAgent);
    s.record_verdict(Verdict::ReadyForAgent);
    s.record_verdict(Verdict::NeedsInfo);
    s.record_verdict(Verdict::WontfixEnhancement);
    s.record_verdict(Verdict::ReadyForHuman);

    assert_eq!(s.ready_for_agent, 2);
    assert_eq!(s.needs_info, 1);
    assert_eq!(s.wontfix_enhancement, 1);
    assert_eq!(s.ready_for_human, 1);
    assert_eq!(s.failed, 0);
    assert_eq!(s.total(), 5);
}

#[test]
fn backlog_summary_failures_tally_separately_from_verdicts() {
    // Brief acceptance: failures get their own count alongside the
    // verdict breakdown; they MUST NOT silently roll into any verdict
    // bucket (an operator scanning the summary needs to know whether
    // any per-issue triage call crashed).
    let mut s = BacklogSummary::default();
    s.record_verdict(Verdict::ReadyForAgent);
    s.record_failure();
    s.record_failure();

    assert_eq!(s.ready_for_agent, 1);
    assert_eq!(s.failed, 2);
    assert_eq!(s.total(), 3);
}

#[test]
fn backlog_summary_display_shows_total_and_per_verdict_counts() {
    let mut s = BacklogSummary::default();
    s.record_verdict(Verdict::ReadyForAgent);
    s.record_verdict(Verdict::ReadyForAgent);
    s.record_verdict(Verdict::ReadyForAgent);
    s.record_verdict(Verdict::NeedsInfo);
    s.record_verdict(Verdict::NeedsInfo);
    s.record_verdict(Verdict::WontfixEnhancement);
    s.record_verdict(Verdict::ReadyForHuman);

    let rendered = format!("{}", s);

    assert!(
        rendered.contains("7"),
        "summary must show total: {rendered}"
    );
    assert!(
        rendered.contains("ready-for-agent"),
        "summary must name ready-for-agent: {rendered}"
    );
    assert!(
        rendered.contains("needs-info"),
        "summary must name needs-info: {rendered}"
    );
    assert!(
        rendered.contains("wontfix-enhancement"),
        "summary must name wontfix-enhancement: {rendered}"
    );
    assert!(
        rendered.contains("ready-for-human"),
        "summary must name ready-for-human: {rendered}"
    );
}

#[test]
fn backlog_summary_display_shows_failed_line_when_any_failures() {
    // Brief acceptance: "including a `failed` count if any" — when
    // failures occurred, the operator must see them in the summary.
    let mut s = BacklogSummary::default();
    s.record_verdict(Verdict::ReadyForAgent);
    s.record_failure();

    let rendered = format!("{}", s);
    assert!(
        rendered.contains("failed"),
        "summary must call out failures when present: {rendered}"
    );
    assert!(
        rendered.contains('1'),
        "failed count of 1 must appear: {rendered}"
    );
}

#[test]
fn backlog_summary_display_omits_failed_line_when_no_failures() {
    // Happy-path symmetry: a clean run shouldn't dangle a `0 failed`
    // line because that's noise. The criterion phrases the failed
    // count as "including ... if any" — only when present.
    let mut s = BacklogSummary::default();
    s.record_verdict(Verdict::ReadyForAgent);
    s.record_verdict(Verdict::NeedsInfo);

    let rendered = format!("{}", s);
    assert!(
        !rendered.contains("failed"),
        "clean run summary must not mention failures: {rendered}"
    );
}

#[tokio::test]
async fn drain_backlog_visits_each_issue_serially_in_input_order() {
    // Brief: "Processes issues serially." The acceptance test pins
    // both the visit count and the order — backlog ordering is a
    // contract because workspace state flows between issues (issue
    // N's apply commit must be visible to issue N+1).
    let issues = vec![issue(3, "oldest"), issue(7, "middle"), issue(9, "newest")];

    let visited = Arc::new(Mutex::new(Vec::<u64>::new()));
    let visited_clone = Arc::clone(&visited);

    let summary = drain_backlog(issues, false, move |n, _dry_run| {
        let v = Arc::clone(&visited_clone);
        async move {
            v.lock().unwrap().push(n);
            Ok(Verdict::ReadyForAgent)
        }
    })
    .await;

    assert_eq!(*visited.lock().unwrap(), vec![3, 7, 9]);
    assert_eq!(summary.total(), 3);
    assert_eq!(summary.ready_for_agent, 3);
}

#[tokio::test]
async fn drain_backlog_propagates_dry_run_flag_to_every_per_issue_call() {
    // Brief: "Propagates `--dry-run` to every per-issue invocation".
    let issues = vec![issue(1, "a"), issue(2, "b"), issue(3, "c")];

    let flags = Arc::new(Mutex::new(Vec::<bool>::new()));
    let flags_clone = Arc::clone(&flags);

    drain_backlog(issues, true, move |_n, dry_run| {
        let f = Arc::clone(&flags_clone);
        async move {
            f.lock().unwrap().push(dry_run);
            Ok(Verdict::ReadyForAgent)
        }
    })
    .await;

    assert_eq!(*flags.lock().unwrap(), vec![true, true, true]);
}

#[tokio::test]
async fn drain_backlog_isolates_per_issue_failures_and_continues_with_next_issue() {
    // Brief acceptance: "Per-issue failures are isolated: a crash or
    // malformed verdict on issue N does NOT prevent issue N+1 from
    // being processed; failures are logged and tallied in the
    // summary." Different from slice X1's halt-on-phase-failure:
    // here, halting would block the whole backlog drain.
    let issues = vec![issue(1, "a"), issue(2, "b"), issue(3, "c")];

    let visited = Arc::new(Mutex::new(Vec::<u64>::new()));
    let visited_clone = Arc::clone(&visited);

    let summary = drain_backlog(issues, false, move |n, _dry_run| {
        let v = Arc::clone(&visited_clone);
        async move {
            v.lock().unwrap().push(n);
            if n == 2 {
                Err("simulated per-issue triage failure".to_string())
            } else {
                Ok(Verdict::ReadyForAgent)
            }
        }
    })
    .await;

    assert_eq!(
        *visited.lock().unwrap(),
        vec![1, 2, 3],
        "issue #3 must still run even though issue #2's triage returned Err"
    );
    assert_eq!(summary.ready_for_agent, 2);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.total(), 3);
}

#[tokio::test]
async fn drain_backlog_with_empty_input_returns_zero_summary() {
    let summary = drain_backlog(Vec::<Issue>::new(), false, |_n, _dry_run| async {
        Ok(Verdict::ReadyForAgent)
    })
    .await;

    assert_eq!(summary.total(), 0);
    assert_eq!(summary.failed, 0);
}

#[tokio::test]
async fn drain_backlog_passes_each_issues_number_to_the_per_issue_path() {
    // The per-issue path receives the issue number (it fetches the
    // body/comments itself, T1's contract). Pinned to prevent a
    // refactor accidentally passing the loop index or some other
    // derived value.
    let issues = vec![issue(101, "x"), issue(202, "y")];

    let seen = Arc::new(Mutex::new(Vec::<u64>::new()));
    let seen_clone = Arc::clone(&seen);

    drain_backlog(issues, false, move |n, _| {
        let s = Arc::clone(&seen_clone);
        async move {
            s.lock().unwrap().push(n);
            Ok(Verdict::ReadyForAgent)
        }
    })
    .await;

    assert_eq!(*seen.lock().unwrap(), vec![101, 202]);
}

#[tokio::test]
async fn drain_backlog_tallies_a_mix_of_verdicts_and_failures_realistically() {
    // The brief's own worked example:
    //   "3 → ready-for-agent, 2 → needs-info, 1 → wontfix-enhancement,
    //    1 → ready-for-human, 1 verdict-failed"
    // The drain must reproduce that breakdown faithfully.
    let issues: Vec<Issue> = (1..=8).map(|n| issue(n, "x")).collect();

    let summary = drain_backlog(issues, false, |n, _| async move {
        match n {
            1..=3 => Ok(Verdict::ReadyForAgent),
            4..=5 => Ok(Verdict::NeedsInfo),
            6 => Ok(Verdict::WontfixEnhancement),
            7 => Ok(Verdict::ReadyForHuman),
            _ => Err("bad verdict".to_string()),
        }
    })
    .await;

    assert_eq!(summary.ready_for_agent, 3);
    assert_eq!(summary.needs_info, 2);
    assert_eq!(summary.wontfix_enhancement, 1);
    assert_eq!(summary.ready_for_human, 1);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.total(), 8);
}
