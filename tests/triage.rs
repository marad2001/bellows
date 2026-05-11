//! Slice T2 (#22): `bellows triage` backlog drain — verdict tally,
//! summary formatting, and the serial iteration loop's contract.
//!
//! The per-issue triage path (T1 / issue #21) is injected as an
//! async closure so the drain logic can be tested without spawning
//! containers. The "calls T1's per-issue triage path serially with
//! workspace state flowing between issues" property is implicit in
//! the serial-await loop — the test pins the visit order, the
//! per-issue isolation, and the dry-run flag propagation.
//!
//! Slice T1 (#21): per-issue triage path — verdict JSON schema,
//! parsing, and validation. The JSON is what the in-container claude
//! agent writes to `/workspace/.bellows-triage-verdict.json`; bellows
//! reads + validates it, then either prints (dry-run) or applies via
//! the tracker. Tests pin the schema's conditional-field rules — a
//! malformed or under-specified verdict surfaces an explicit error
//! rather than partially applying.

use std::sync::{Arc, Mutex};

use bellows::tracker::Issue;
use bellows::triage::{
    drain_backlog, BacklogSummary, TriageVerdict, Verdict, VerdictCategory, VerdictParseError,
    VerdictState,
};

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
    assert_eq!(Verdict::Wontfix.label(), "wontfix");
}

#[test]
fn backlog_summary_starts_empty() {
    let s = BacklogSummary::default();
    assert_eq!(s.total(), 0);
    assert_eq!(s.ready_for_agent, 0);
    assert_eq!(s.needs_info, 0);
    assert_eq!(s.wontfix, 0);
    assert_eq!(s.ready_for_human, 0);
    assert_eq!(s.failed, 0);
}

#[test]
fn backlog_summary_tallies_each_verdict_into_its_own_bucket() {
    let mut s = BacklogSummary::default();
    s.record_verdict(Verdict::ReadyForAgent);
    s.record_verdict(Verdict::ReadyForAgent);
    s.record_verdict(Verdict::NeedsInfo);
    s.record_verdict(Verdict::Wontfix);
    s.record_verdict(Verdict::ReadyForHuman);

    assert_eq!(s.ready_for_agent, 2);
    assert_eq!(s.needs_info, 1);
    assert_eq!(s.wontfix, 1);
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
    s.record_verdict(Verdict::Wontfix);
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
        rendered.contains("wontfix"),
        "summary must name wontfix: {rendered}"
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

// ----------------------------------------------------------------------
// Slice T1 (#21): vendored triage prompt + bundle/dry-run renderers.
// The triage container reads `/workspace/.bellows-triage-input.md`
// (the IssueBundle rendered as markdown) and the kickoff prompt
// (which instructs it to write a structured JSON verdict to
// `/workspace/.bellows-triage-verdict.json`). Vendored so the manual
// `/triage` skill at `~/.claude/skills/triage/` can stay
// gh-CLI-oriented while bellows's containerised flow stays
// verdict-file-oriented.
// ----------------------------------------------------------------------

#[test]
fn triage_prompt_template_documents_the_verdict_file_path_and_input_file_path() {
    use bellows::triage::{TRIAGE_INPUT_FILE, TRIAGE_PROMPT, TRIAGE_VERDICT_FILE};
    assert!(
        TRIAGE_PROMPT.contains(TRIAGE_INPUT_FILE),
        "prompt must tell the agent where to read the bundle from: {TRIAGE_PROMPT}"
    );
    assert!(
        TRIAGE_PROMPT.contains(TRIAGE_VERDICT_FILE),
        "prompt must tell the agent where to write the verdict to: {TRIAGE_PROMPT}"
    );
}

#[test]
fn triage_prompt_template_forbids_gh_cli_calls_and_directs_verdict_to_a_file() {
    // Bellows-specific contract: the triage container has NO GitHub
    // credentials, so the prompt must NOT instruct the agent to call
    // gh to label/comment/close. The prompt must direct the agent
    // toward producing a verdict FILE instead, which bellows then
    // applies on the host. The manual /triage skill is the
    // gh-CLI-oriented one; this vendored template is the verdict-
    // file-oriented sibling.
    use bellows::triage::TRIAGE_PROMPT;
    let lower = TRIAGE_PROMPT.to_lowercase();
    assert!(
        !lower.contains("gh issue edit") && !lower.contains("gh issue close"),
        "triage prompt must NOT instruct gh-CLI calls (container has no PAT): {TRIAGE_PROMPT}"
    );
    assert!(
        lower.contains("verdict"),
        "triage prompt must direct the agent toward the verdict-file output: {TRIAGE_PROMPT}"
    );
}

#[test]
fn triage_prompt_describes_each_of_the_four_verdict_states() {
    use bellows::triage::TRIAGE_PROMPT;
    for label in [
        "needs-info",
        "ready-for-agent",
        "ready-for-human",
        "wontfix",
    ] {
        assert!(
            TRIAGE_PROMPT.contains(label),
            "triage prompt must mention state `{label}` so the agent picks it: {TRIAGE_PROMPT}"
        );
    }
}

#[test]
fn triage_prompt_describes_the_wontfix_enhancement_out_of_scope_path() {
    use bellows::triage::TRIAGE_PROMPT;
    // The agent must know that wontfix + enhancement requires the
    // out_of_scope_filename/content fields so the precedent lands on
    // master; otherwise the apply step rejects the verdict.
    let lower = TRIAGE_PROMPT.to_lowercase();
    assert!(
        lower.contains("out_of_scope") || lower.contains("out-of-scope"),
        "triage prompt must describe the wontfix-enhancement out-of-scope payload: \
         {TRIAGE_PROMPT}"
    );
}

#[test]
fn render_triage_input_includes_issue_number_title_body_labels_and_comments() {
    use bellows::tracker::IssueBundle;
    use bellows::triage::render_triage_input;

    let bundle = IssueBundle {
        number: 42,
        title: "Crash on empty input".to_string(),
        body: Some("Repro: pass \"\" and panic.".to_string()),
        labels: vec!["needs-triage".to_string(), "bug".to_string()],
        comments: vec![
            "What version?".to_string(),
            "0.4.2.".to_string(),
        ],
    };
    let rendered = render_triage_input(&bundle);

    assert!(
        rendered.contains("#42"),
        "rendered input must include the issue number: {rendered}"
    );
    assert!(
        rendered.contains("Crash on empty input"),
        "rendered input must include the issue title: {rendered}"
    );
    assert!(
        rendered.contains("Repro: pass"),
        "rendered input must include the issue body: {rendered}"
    );
    assert!(
        rendered.contains("needs-triage"),
        "rendered input must surface current labels: {rendered}"
    );
    assert!(
        rendered.contains("What version?"),
        "rendered input must include the first comment: {rendered}"
    );
    assert!(
        rendered.contains("0.4.2."),
        "rendered input must include later comments: {rendered}"
    );
}

#[test]
fn render_triage_input_preserves_comment_order_so_re_triage_sees_conversation_flow() {
    use bellows::tracker::IssueBundle;
    use bellows::triage::render_triage_input;

    let bundle = IssueBundle {
        number: 7,
        title: "needs-info follow-up".to_string(),
        body: None,
        labels: vec!["needs-info".to_string()],
        comments: vec![
            "First: what version are you on?".to_string(),
            "Reporter: 0.4.2.".to_string(),
            "Maintainer follow-up: thanks, reproducing now.".to_string(),
        ],
    };
    let rendered = render_triage_input(&bundle);

    let first = rendered.find("First: what version").expect("first comment must appear");
    let reporter = rendered.find("Reporter: 0.4.2.").expect("reporter reply must appear");
    let follow_up = rendered.find("Maintainer follow-up").expect("follow-up must appear");
    assert!(
        first < reporter && reporter < follow_up,
        "comments must appear in chronological order so the agent reads question-then-answer: {rendered}"
    );
}

#[test]
fn render_triage_input_handles_missing_body_without_panicking() {
    use bellows::tracker::IssueBundle;
    use bellows::triage::render_triage_input;

    let bundle = IssueBundle {
        number: 99,
        title: "Empty issue".to_string(),
        body: None,
        labels: vec![],
        comments: vec![],
    };
    let rendered = render_triage_input(&bundle);
    assert!(rendered.contains("#99"));
    assert!(rendered.contains("Empty issue"));
}

#[test]
fn render_dry_run_report_surfaces_state_comment_preview_and_brief_preview() {
    // Brief acceptance criterion: dry-run mode prints the verdict to
    // stdout in human-readable form (state, comment preview, brief
    // preview, file-write preview); no GitHub or git mutations
    // performed. The renderer is the source of truth for that
    // human-readable form.
    let v = TriageVerdict::parse(
        "{\
            \"category\": \"bug\",\
            \"state\": \"ready-for-agent\",\
            \"reasoning\": \"clear repro\",\
            \"comment_body\": \"Moving to ready-for-agent.\",\
            \"agent_brief\": \"## Agent Brief\\n\\nFix the foo bug.\"\
        }",
    )
    .expect("valid verdict");

    let rendered = bellows::triage::render_dry_run_report(&v);
    assert!(
        rendered.contains("ready-for-agent"),
        "dry-run output must surface the state: {rendered}"
    );
    assert!(
        rendered.contains("Moving to ready-for-agent."),
        "dry-run output must preview comment_body: {rendered}"
    );
    assert!(
        rendered.contains("Agent Brief"),
        "dry-run output must preview the brief: {rendered}"
    );
}

#[test]
fn render_dry_run_report_surfaces_out_of_scope_file_write_preview_for_wontfix_enhancement() {
    let v = TriageVerdict::parse(
        "{\
            \"category\": \"enhancement\",\
            \"state\": \"wontfix\",\
            \"reasoning\": \"out of scope\",\
            \"comment_body\": \"Closing.\",\
            \"close_issue\": true,\
            \"out_of_scope_filename\": \"auto-rerun.md\",\
            \"out_of_scope_content\": \"# Auto-rerun out of scope\\n\"\
        }",
    )
    .expect("valid verdict");
    let rendered = bellows::triage::render_dry_run_report(&v);
    assert!(
        rendered.contains(".out-of-scope/auto-rerun.md"),
        "dry-run output must preview the file path that WOULD be written: {rendered}"
    );
    assert!(
        rendered.contains("Auto-rerun out of scope"),
        "dry-run output must preview the content that WOULD be written: {rendered}"
    );
}

// ----------------------------------------------------------------------
// Slice T1 (#21): TriageVerdict — schema, parsing, conditional
// validation per state. The JSON the in-container claude agent writes
// to `/workspace/.bellows-triage-verdict.json`. Bellows reads + validates
// before applying; an under-specified verdict surfaces a typed error
// rather than partially applying GitHub-side mutations.
// ----------------------------------------------------------------------

fn ready_for_agent_json() -> &'static str {
    "{
        \"category\": \"bug\",
        \"state\": \"ready-for-agent\",
        \"reasoning\": \"clear repro and a minimal fix\",
        \"comment_body\": \"Moving to ready-for-agent.\",
        \"agent_brief\": \"## Agent Brief\\n\\nFix the foo bug.\"
    }"
}

fn needs_info_json() -> &'static str {
    "{
        \"category\": \"bug\",
        \"state\": \"needs-info\",
        \"reasoning\": \"no repro steps\",
        \"comment_body\": \"Could you share repro steps?\"
    }"
}

fn ready_for_human_json() -> &'static str {
    "{
        \"category\": \"enhancement\",
        \"state\": \"ready-for-human\",
        \"reasoning\": \"requires architectural decisions an agent shouldn't make\",
        \"comment_body\": \"Routing this to a human implementer.\",
        \"human_brief\": \"## Human Brief\\n\\nDecide on the schema migration approach.\"
    }"
}

fn wontfix_bug_json() -> &'static str {
    "{
        \"category\": \"bug\",
        \"state\": \"wontfix\",
        \"reasoning\": \"not reproducible and reporter unresponsive\",
        \"comment_body\": \"Closing as wontfix.\",
        \"close_issue\": true
    }"
}

fn wontfix_enhancement_json() -> &'static str {
    "{
        \"category\": \"enhancement\",
        \"state\": \"wontfix\",
        \"reasoning\": \"out of scope per CONTEXT.md\",
        \"comment_body\": \"Closing as out-of-scope; see the linked file for the precedent.\",
        \"close_issue\": true,
        \"out_of_scope_filename\": \"auto-rerun.md\",
        \"out_of_scope_content\": \"# Auto-rerun out of scope\\n\\nReason: ...\\n\"
    }"
}

#[test]
fn parse_verdict_accepts_ready_for_agent_with_required_agent_brief() {
    let v = TriageVerdict::parse(ready_for_agent_json())
        .expect("valid ready-for-agent verdict must parse");
    assert_eq!(v.category, VerdictCategory::Bug);
    assert_eq!(v.state, VerdictState::ReadyForAgent);
    assert_eq!(v.reasoning, "clear repro and a minimal fix");
    assert!(v.agent_brief.as_deref().unwrap().contains("Agent Brief"));
    assert!(v.human_brief.is_none());
    assert!(v.out_of_scope_filename.is_none());
    assert!(v.out_of_scope_content.is_none());
    // close_issue is None or false for non-wontfix states.
    assert!(matches!(v.close_issue, None | Some(false)));
}

#[test]
fn parse_verdict_accepts_needs_info_with_no_conditional_fields() {
    let v = TriageVerdict::parse(needs_info_json())
        .expect("valid needs-info verdict must parse");
    assert_eq!(v.state, VerdictState::NeedsInfo);
    assert!(v.agent_brief.is_none());
    assert!(v.human_brief.is_none());
}

#[test]
fn parse_verdict_accepts_ready_for_human_with_required_human_brief() {
    let v = TriageVerdict::parse(ready_for_human_json())
        .expect("valid ready-for-human verdict must parse");
    assert_eq!(v.state, VerdictState::ReadyForHuman);
    assert_eq!(v.category, VerdictCategory::Enhancement);
    assert!(v.human_brief.as_deref().unwrap().contains("Human Brief"));
    assert!(v.agent_brief.is_none());
}

#[test]
fn parse_verdict_accepts_wontfix_bug_without_out_of_scope_fields() {
    let v = TriageVerdict::parse(wontfix_bug_json())
        .expect("valid wontfix-bug verdict must parse");
    assert_eq!(v.state, VerdictState::Wontfix);
    assert_eq!(v.category, VerdictCategory::Bug);
    assert_eq!(v.close_issue, Some(true));
    // wontfix-bug does NOT carry out-of-scope file payload.
    assert!(v.out_of_scope_filename.is_none());
    assert!(v.out_of_scope_content.is_none());
}

#[test]
fn parse_verdict_accepts_wontfix_enhancement_with_out_of_scope_file_payload() {
    // The wontfix-enhancement form is keyed on (state=wontfix,
    // category=enhancement) and carries an .out-of-scope/<filename>.md
    // payload that bellows commits directly to master after the GitHub-
    // side mutations land. The schema requires the filename + content
    // pair for this combination.
    let v = TriageVerdict::parse(wontfix_enhancement_json())
        .expect("valid wontfix-enhancement verdict must parse");
    assert_eq!(v.state, VerdictState::Wontfix);
    assert_eq!(v.category, VerdictCategory::Enhancement);
    assert_eq!(v.out_of_scope_filename.as_deref(), Some("auto-rerun.md"));
    assert!(v
        .out_of_scope_content
        .as_deref()
        .unwrap()
        .contains("Auto-rerun out of scope"));
    assert_eq!(v.close_issue, Some(true));
}

#[test]
fn parse_verdict_rejects_invalid_state_value() {
    let bad = "{
        \"category\": \"bug\",
        \"state\": \"wat\",
        \"reasoning\": \"x\",
        \"comment_body\": \"x\"
    }";
    let err = TriageVerdict::parse(bad).expect_err("invalid state must reject");
    assert!(
        matches!(err, VerdictParseError::Json(_)),
        "expected Json error variant for unknown state string, got {err:?}",
    );
}

#[test]
fn parse_verdict_rejects_invalid_category_value() {
    let bad = "{
        \"category\": \"feature\",
        \"state\": \"needs-info\",
        \"reasoning\": \"x\",
        \"comment_body\": \"x\"
    }";
    let err = TriageVerdict::parse(bad).expect_err("invalid category must reject");
    assert!(matches!(err, VerdictParseError::Json(_)));
}

#[test]
fn parse_verdict_rejects_ready_for_agent_without_agent_brief() {
    // Conditional-field validation: ready-for-agent without
    // agent_brief leaves the downstream `tracker::fetch_agent_brief`
    // (which the bellows-run pipeline uses to read the brief out of
    // issue comments) with nothing to find. We refuse rather than
    // post a label with no brief.
    let bad = "{
        \"category\": \"bug\",
        \"state\": \"ready-for-agent\",
        \"reasoning\": \"x\",
        \"comment_body\": \"x\"
    }";
    let err = TriageVerdict::parse(bad).expect_err("missing agent_brief must reject");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("agent_brief"),
        "error must name the missing field: {msg}"
    );
}

#[test]
fn parse_verdict_rejects_ready_for_human_without_human_brief() {
    let bad = "{
        \"category\": \"enhancement\",
        \"state\": \"ready-for-human\",
        \"reasoning\": \"x\",
        \"comment_body\": \"x\"
    }";
    let err = TriageVerdict::parse(bad).expect_err("missing human_brief must reject");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("human_brief"),
        "error must name the missing field: {msg}"
    );
}

#[test]
fn parse_verdict_rejects_wontfix_enhancement_without_out_of_scope_filename() {
    // wontfix-enhancement is the only state/category combination that
    // requires the file payload — and the apply step refuses to skip
    // it (otherwise the closing comment links to a file that doesn't
    // exist on master).
    let bad = "{
        \"category\": \"enhancement\",
        \"state\": \"wontfix\",
        \"reasoning\": \"x\",
        \"comment_body\": \"x\",
        \"close_issue\": true,
        \"out_of_scope_content\": \"body without a filename\"
    }";
    let err = TriageVerdict::parse(bad).expect_err("missing out_of_scope_filename must reject");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("out_of_scope_filename"),
        "error must name the missing field: {msg}"
    );
}

#[test]
fn parse_verdict_rejects_wontfix_enhancement_without_out_of_scope_content() {
    let bad = "{
        \"category\": \"enhancement\",
        \"state\": \"wontfix\",
        \"reasoning\": \"x\",
        \"comment_body\": \"x\",
        \"close_issue\": true,
        \"out_of_scope_filename\": \"stub.md\"
    }";
    let err = TriageVerdict::parse(bad).expect_err("missing out_of_scope_content must reject");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("out_of_scope_content"),
        "error must name the missing field: {msg}"
    );
}

#[test]
fn parse_verdict_rejects_wontfix_without_close_issue_true() {
    // wontfix without close_issue=true is an under-specified verdict:
    // the brief says "For `wontfix` (any category): close the issue."
    // Forcing close_issue=true in the schema means the agent and bellows
    // agree explicitly on the close. A missing/false value triggers
    // an error rather than a label flip with no close.
    let bad = "{
        \"category\": \"bug\",
        \"state\": \"wontfix\",
        \"reasoning\": \"x\",
        \"comment_body\": \"x\",
        \"close_issue\": false
    }";
    let err = TriageVerdict::parse(bad).expect_err("wontfix with close_issue=false must reject");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("close_issue"),
        "error must name the close_issue field: {msg}"
    );
}

#[test]
fn parse_verdict_rejects_non_wontfix_with_out_of_scope_payload() {
    // Defensive: a needs-info verdict that smuggles an out-of-scope
    // file payload makes no semantic sense — we refuse rather than
    // silently ignore the extra fields. This pins the contract:
    // out_of_scope_* fields are valid ONLY for wontfix-enhancement.
    let bad = "{
        \"category\": \"enhancement\",
        \"state\": \"needs-info\",
        \"reasoning\": \"x\",
        \"comment_body\": \"x\",
        \"out_of_scope_filename\": \"stub.md\",
        \"out_of_scope_content\": \"body\"
    }";
    let err = TriageVerdict::parse(bad).expect_err("non-wontfix with out_of_scope must reject");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("out_of_scope")
            || msg.to_lowercase().contains("out-of-scope"),
        "error must name the unexpected field: {msg}"
    );
}

#[test]
fn parse_verdict_rejects_non_wontfix_close_issue_true() {
    // close_issue=true outside of a wontfix verdict would close an
    // issue we explicitly want to keep open (a ready-for-agent issue
    // closed at triage means the agent never runs against it). Refuse.
    let bad = "{
        \"category\": \"bug\",
        \"state\": \"ready-for-agent\",
        \"reasoning\": \"x\",
        \"comment_body\": \"x\",
        \"agent_brief\": \"## Agent Brief\\n\\nDo stuff.\",
        \"close_issue\": true
    }";
    let err = TriageVerdict::parse(bad).expect_err("non-wontfix close_issue=true must reject");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("close_issue"),
        "error must name the close_issue field: {msg}"
    );
}

#[test]
fn parse_verdict_rejects_malformed_json() {
    let err = TriageVerdict::parse("{ not json").expect_err("malformed JSON must reject");
    assert!(matches!(err, VerdictParseError::Json(_)));
}

#[test]
fn verdict_state_maps_to_canonical_triage_label_strings() {
    // The bellows runtime keys label transitions on these exact
    // strings — they must match docs/agents/triage-labels.md.
    assert_eq!(VerdictState::NeedsInfo.label(), "needs-info");
    assert_eq!(VerdictState::ReadyForAgent.label(), "ready-for-agent");
    assert_eq!(VerdictState::ReadyForHuman.label(), "ready-for-human");
    assert_eq!(VerdictState::Wontfix.label(), "wontfix");
}

#[tokio::test]
async fn drain_backlog_tallies_a_mix_of_verdicts_and_failures_realistically() {
    // The brief's own worked example:
    //   "3 → ready-for-agent, 2 → needs-info, 1 → wontfix,
    //    1 → ready-for-human, 1 verdict-failed"
    // The drain must reproduce that breakdown faithfully.
    let issues: Vec<Issue> = (1..=8).map(|n| issue(n, "x")).collect();

    let summary = drain_backlog(issues, false, |n, _| async move {
        match n {
            1..=3 => Ok(Verdict::ReadyForAgent),
            4..=5 => Ok(Verdict::NeedsInfo),
            6 => Ok(Verdict::Wontfix),
            7 => Ok(Verdict::ReadyForHuman),
            _ => Err("bad verdict".to_string()),
        }
    })
    .await;

    assert_eq!(summary.ready_for_agent, 3);
    assert_eq!(summary.needs_info, 2);
    assert_eq!(summary.wontfix, 1);
    assert_eq!(summary.ready_for_human, 1);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.total(), 8);
}
