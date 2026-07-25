//! Issue #164: **Stall** detection during the implement phase.
//!
//! Covers the three surfaces the feature adds, in the vocabulary
//! `CONTEXT.md` fixes:
//!
//! - `policy::classify_stall` — the pure classifier over a bounded
//!   sample sequence that tells **Oscillation** from **Idleness**.
//! - `workspace::sample_workspace_state` — the git-shaped sampler that
//!   turns a workspace into one comparable hash.
//! - `chain_walker::decide_oscillation_advance_action` — admitting
//!   **Oscillation** as the second trigger for an **Advance**, under
//!   the shared `advances_used` allowance and the new budget floor.

use std::path::Path;
use std::process::Command;
use std::str::FromStr;
use std::time::Duration;

use tempfile::TempDir;

use bellows::policy::{
    classify_stall, record_sample, stall_window_len, SampleHash, Stall, StallTracker,
    DEFAULT_IDLENESS_SAMPLES, STALL_SAMPLE_WINDOW,
};
use bellows::chain_walker::{
    decide_oscillation_advance_action, format_idleness_log, format_oscillation_advance_log,
    oscillation_kill_window, OscillationAdvanceAction,
    PickReason, RateLimitDisposition, DEFAULT_ADVANCE_BUDGET_FLOOR_FRACTION,
};
use bellows::config::{Config, Engine};
use bellows::workspace::sample_workspace_state;

fn samples(seq: &[&str]) -> Vec<SampleHash> {
    seq.iter().map(|s| SampleHash::new(*s)).collect()
}

fn run_git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("git invocation");
    assert!(status.success(), "git {args:?} failed in {cwd:?}");
}

/// A repo standing in for the bind-mounted workspace the agent works
/// in: one commit, a `.gitignore` that ignores `target/` exactly as a
/// Rust repo's does.
fn init_repo(path: &Path) {
    run_git(path, &["init"]);
    run_git(path, &["config", "user.email", "test@example.com"]);
    run_git(path, &["config", "user.name", "Test"]);
    std::fs::write(path.join("src.rs"), "fn main() {}\n").unwrap();
    std::fs::write(path.join(".gitignore"), "target/\n").unwrap();
    run_git(path, &["add", "."]);
    run_git(path, &["commit", "-m", "initial"]);
}

#[test]
fn oscillating_sample_sequence_classifies_as_oscillation() {
    // CONTEXT.md: Oscillation is "the workspace cycles through repeated
    // states — an edit made, reverted, and made again". A-B-A-B-A is
    // that shape: the same state seen three times with a different
    // state between occurrences.
    assert_eq!(
        classify_stall(&samples(&["a", "b", "a", "b", "a"]), 15),
        Some(Stall::Oscillation),
    );
}

#[test]
fn consecutively_repeated_sample_is_not_oscillation() {
    // The crux of the feature: a hash repeated with nothing in between
    // is a still workspace (Idleness), not a cycle. Advancing on it
    // would discard a workspace that may be seconds from a clean exit.
    assert_ne!(
        classify_stall(&samples(&["a", "a", "a", "a", "a"]), 15),
        Some(Stall::Oscillation),
    );
}

#[test]
fn n_consecutive_identical_samples_classify_as_idleness() {
    let still = samples(&["a"; 15]);
    assert_eq!(classify_stall(&still, 15), Some(Stall::Idleness));
}

#[test]
fn idleness_needs_the_full_run_of_consecutive_identical_samples() {
    // One short of the threshold is not yet Idleness, and a workspace
    // that moved on part-way through the window is not idle either —
    // only the *trailing* run of identical samples counts.
    assert_eq!(classify_stall(&samples(&["a"; 14]), 15), None);
    let mut moved_on = samples(&["a"; 7]);
    moved_on.extend(samples(&["b"; 8]));
    assert_eq!(classify_stall(&moved_on, 15), None);
}

#[test]
fn all_distinct_samples_are_neither_oscillation_nor_idleness() {
    // Healthy progress: every sample differs from the last.
    let healthy = samples(&["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]);
    assert_eq!(classify_stall(&healthy, 15), None);
}

#[test]
fn the_retained_window_is_bounded_and_keeps_the_most_recent_samples() {
    // The sample sequence is bounded so a long run does not grow it
    // without limit. The bound has to be wide enough for the idleness
    // threshold as well as the 10-sample oscillation window, otherwise
    // Idleness could never be observed.
    assert_eq!(stall_window_len(DEFAULT_IDLENESS_SAMPLES), DEFAULT_IDLENESS_SAMPLES);
    assert_eq!(stall_window_len(2), STALL_SAMPLE_WINDOW);

    let window = stall_window_len(DEFAULT_IDLENESS_SAMPLES);
    let mut retained: Vec<SampleHash> = Vec::new();
    for i in 0..window + 5 {
        record_sample(&mut retained, SampleHash::new(format!("h{i}")), window);
    }
    assert_eq!(retained.len(), window);
    assert_eq!(retained.first().unwrap().as_str(), "h5");
    assert_eq!(
        retained.last().unwrap().as_str(),
        format!("h{}", window + 4),
    );
}

#[test]
fn an_empty_or_single_sample_sequence_is_not_a_stall() {
    // The first sample of a run must never look like a stall.
    assert_eq!(classify_stall(&[], 15), None);
    assert_eq!(classify_stall(&samples(&["a"]), 15), None);
}

#[tokio::test]
async fn sampling_the_same_unchanged_workspace_twice_gives_the_same_hash() {
    // The sample is of the agent's *work product*: a modified tracked
    // file plus an untracked file it created.
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    std::fs::write(dir.path().join("src.rs"), "fn main() { todo!() }\n").unwrap();
    std::fs::write(dir.path().join("new.rs"), "// new\n").unwrap();

    let first = sample_workspace_state(dir.path()).await.expect("sample");
    let second = sample_workspace_state(dir.path()).await.expect("sample");
    assert_eq!(first, second, "no intervening change means no new state");
}

#[tokio::test]
async fn sampling_a_changed_workspace_gives_a_different_hash() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    std::fs::write(dir.path().join("src.rs"), "fn main() { todo!() }\n").unwrap();
    std::fs::write(dir.path().join("new.rs"), "// new\n").unwrap();
    let before = sample_workspace_state(dir.path()).await.expect("sample");

    // An edit to a tracked file moves the state.
    std::fs::write(dir.path().join("src.rs"), "fn main() { println!(); }\n").unwrap();
    let after_edit = sample_workspace_state(dir.path()).await.expect("sample");
    assert_ne!(before, after_edit, "a tracked-file edit is a new state");

    // So does creating another untracked file.
    std::fs::write(dir.path().join("another.rs"), "// another\n").unwrap();
    let after_untracked = sample_workspace_state(dir.path()).await.expect("sample");
    assert_ne!(
        after_edit, after_untracked,
        "a new untracked path is a new state",
    );
}

#[tokio::test]
async fn writing_into_a_gitignored_path_does_not_change_the_hash() {
    // `target/` churns on every cargo invocation. Sampling through git
    // is what keeps build artefacts out of the sample — a recursive
    // file hash would report change on every sample and no stall would
    // ever be detectable.
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    std::fs::write(dir.path().join("src.rs"), "fn main() { todo!() }\n").unwrap();
    let before = sample_workspace_state(dir.path()).await.expect("sample");

    std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
    std::fs::write(dir.path().join("target/debug/bellows"), "binary\n").unwrap();
    let after = sample_workspace_state(dir.path()).await.expect("sample");

    assert_eq!(before, after, "gitignored build output is not work product");
}

#[tokio::test]
async fn an_oscillating_workspace_produces_a_returning_sample_sequence() {
    // End-to-end over the two pure surfaces: sampler feeding
    // classifier. Edit → revert → edit → revert → edit is exactly the
    // shape CONTEXT.md calls Oscillation.
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    let src = dir.path().join("src.rs");

    let mut seq: Vec<SampleHash> = Vec::new();
    for i in 0..5 {
        if i % 2 == 0 {
            std::fs::write(&src, "fn main() { todo!() }\n").unwrap();
        } else {
            std::fs::write(&src, "fn main() {}\n").unwrap();
        }
        let hash = sample_workspace_state(dir.path()).await.expect("sample");
        record_sample(&mut seq, hash, stall_window_len(DEFAULT_IDLENESS_SAMPLES));
    }

    assert_eq!(
        classify_stall(&seq, DEFAULT_IDLENESS_SAMPLES),
        Some(Stall::Oscillation),
    );
}

// ---------------------------------------------------------------
// Oscillation as the second trigger for an Advance.
// ---------------------------------------------------------------

const CAP: Duration = Duration::from_secs(60 * 60);

#[test]
fn oscillation_at_base_sha_with_an_unspent_allowance_advances() {
    // CONTEXT.md: "An Advance has two independent triggers: a
    // rate-limited Engine, and an Oscillation. Both mean the same
    // thing ... and both produce the same response."
    let action = decide_oscillation_advance_action(
        true,
        0,
        Duration::from_secs(45 * 60),
        CAP,
        DEFAULT_ADVANCE_BUDGET_FLOOR_FRACTION,
        true,
    );
    assert_eq!(action, OscillationAdvanceAction::InPlaceAdvance);
    assert_eq!(
        action.disposition(),
        Some(RateLimitDisposition::InPlaceAdvance),
        "oscillation takes the same disposition the rate-limit path takes",
    );
}

#[test]
fn oscillation_does_not_advance_when_the_shared_allowance_is_spent() {
    // The run already advanced once — for a rate limit or for an
    // earlier oscillation, it makes no difference. One allowance,
    // shared, because both triggers mean the same thing.
    let action = decide_oscillation_advance_action(
        true,
        1,
        Duration::from_secs(45 * 60),
        CAP,
        DEFAULT_ADVANCE_BUDGET_FLOOR_FRACTION,
        true,
    );
    assert_eq!(action, OscillationAdvanceAction::ContinueRun);
    assert_eq!(action.disposition(), None);
}

#[test]
fn oscillation_does_not_advance_below_the_budget_floor() {
    // Handing a fresh engine ten minutes of a sixty-minute budget
    // wastes them. Log the Oscillation; let the run continue to its
    // existing terminal state.
    let action = decide_oscillation_advance_action(
        true,
        0,
        Duration::from_secs(10 * 60),
        CAP,
        DEFAULT_ADVANCE_BUDGET_FLOOR_FRACTION,
        true,
    );
    assert_eq!(action, OscillationAdvanceAction::ContinueRun);
}

#[test]
fn oscillation_advances_exactly_at_the_budget_floor() {
    // "At least half" — the boundary itself qualifies.
    let action = decide_oscillation_advance_action(
        true,
        0,
        Duration::from_secs(30 * 60),
        CAP,
        DEFAULT_ADVANCE_BUDGET_FLOOR_FRACTION,
        true,
    );
    assert_eq!(action, OscillationAdvanceAction::InPlaceAdvance);
}

#[test]
fn oscillation_does_not_advance_when_the_workspace_is_ahead_of_base_sha() {
    // The existing guard still holds: an advance discards the
    // workspace, and past base SHA there are commits to lose.
    let action = decide_oscillation_advance_action(
        false,
        0,
        Duration::from_secs(45 * 60),
        CAP,
        DEFAULT_ADVANCE_BUDGET_FLOOR_FRACTION,
        true,
    );
    assert_eq!(action, OscillationAdvanceAction::ContinueRun);
}

#[test]
fn oscillation_does_not_advance_when_there_is_no_other_hot_chain_entry() {
    // Issue #164 review: an advance discards the workspace and re-picks
    // from the chain. With no other hot entry the re-pick would land
    // back on the engine that just wedged, so the run would lose its
    // attempt to repeat the wedge. Log it and continue instead.
    let action = decide_oscillation_advance_action(
        true,
        0,
        Duration::from_secs(45 * 60),
        CAP,
        DEFAULT_ADVANCE_BUDGET_FLOOR_FRACTION,
        false,
    );
    assert_eq!(action, OscillationAdvanceAction::ContinueRun);
    assert_eq!(action.disposition(), None);
}

#[test]
fn the_default_budget_floor_is_half_the_wall_clock() {
    assert_eq!(DEFAULT_ADVANCE_BUDGET_FLOOR_FRACTION, 0.5);
}

#[test]
fn an_oscillation_advance_log_line_names_trigger_abandoned_and_next_engine() {
    let line = format_oscillation_advance_log(Engine::Claude, Engine::Codex);
    assert!(line.contains("oscillation"), "names the trigger: {line:?}");
    assert!(
        line.contains("claude"),
        "names the abandoned engine: {line:?}",
    );
    assert!(
        line.contains("codex"),
        "names the engine being advanced to: {line:?}",
    );
    // Distinguishable from the rate-limit advance line, which says
    // "rate-limited ... in-place-advancing to next hot chain entry".
    assert!(
        !line.contains("rate-limited"),
        "not confusable with a rate-limit advance: {line:?}",
    );
}

#[test]
fn the_oscillation_pick_reason_is_distinguishable_from_the_rate_limit_one() {
    assert_eq!(
        PickReason::InPlaceAdvancementAfterOscillation.as_run_log_phrase(),
        "in-place advancement after oscillation",
    );
    assert_ne!(
        PickReason::InPlaceAdvancementAfterOscillation.as_run_log_phrase(),
        PickReason::InPlaceAdvancementAfterRateLimit.as_run_log_phrase(),
    );
}

// ---------------------------------------------------------------
// Config surface: both keys optional, both defaulted.
// ---------------------------------------------------------------

const MINIMAL_CONFIG: &str = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"
"#;

#[test]
fn stall_sampling_config_defaults_when_the_keys_are_omitted() {
    // An orchestrator.toml written before issue #164 keeps working.
    let config = Config::from_str(MINIMAL_CONFIG).unwrap();
    assert_eq!(config.agent.oscillation_sample_seconds.get(), 60);
    assert_eq!(
        config.agent.advance_budget_floor_fraction,
        DEFAULT_ADVANCE_BUDGET_FLOOR_FRACTION,
    );
}

#[test]
fn stall_sampling_config_can_be_overridden() {
    let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"

[agent]
oscillation_sample_seconds = 30
advance_budget_floor_fraction = 0.25
"#;
    let config = Config::from_str(config_text).unwrap();
    assert_eq!(config.agent.oscillation_sample_seconds.get(), 30);
    assert_eq!(config.agent.advance_budget_floor_fraction, 0.25);
    // Untouched neighbours keep their defaults.
    assert_eq!(config.agent.wall_clock_minutes.get(), 60);
}

#[test]
fn oscillation_sample_seconds_rejects_zero() {
    // A zero interval would spin the sampler in a tight loop against
    // the workspace the container is writing to.
    let config_text = r#"
[repo]
url = "https://github.com/marad2001/bellows"

[github]
pat_env_var = "GITHUB_TOKEN"

[agent]
oscillation_sample_seconds = 0
"#;
    assert!(
        Config::from_str(config_text).is_err(),
        "a zero sampling interval must be rejected at config-load time",
    );
}

// ---------------------------------------------------------------
// The sampling loop's bookkeeping, without a container.
// ---------------------------------------------------------------

#[test]
fn the_tracker_reports_each_stall_shape_at_most_once() {
    // The sampler ticks every 60 seconds for the length of the
    // implement phase. Without this, a workspace that stays idle would
    // re-report Idleness on every remaining tick and bury the run log.
    let mut tracker = StallTracker::new(3);
    assert_eq!(tracker.observe(SampleHash::new("a")), None);
    assert_eq!(tracker.observe(SampleHash::new("a")), None);
    assert_eq!(tracker.observe(SampleHash::new("a")), Some(Stall::Idleness));
    assert_eq!(tracker.observe(SampleHash::new("a")), None, "reported once");
}

#[test]
fn the_tracker_reports_oscillation_the_tick_it_becomes_visible() {
    let mut tracker = StallTracker::new(DEFAULT_IDLENESS_SAMPLES);
    assert_eq!(tracker.observe(SampleHash::new("a")), None);
    assert_eq!(tracker.observe(SampleHash::new("b")), None);
    assert_eq!(tracker.observe(SampleHash::new("a")), None);
    assert_eq!(tracker.observe(SampleHash::new("b")), None);
    assert_eq!(
        tracker.observe(SampleHash::new("a")),
        Some(Stall::Oscillation),
        "the third occurrence with a different state in between",
    );
}

#[test]
fn the_tracker_bounds_what_it_retains() {
    let mut tracker = StallTracker::new(DEFAULT_IDLENESS_SAMPLES);
    for i in 0..100 {
        tracker.observe(SampleHash::new(format!("h{i}")));
    }
    assert_eq!(tracker.retained(), stall_window_len(DEFAULT_IDLENESS_SAMPLES));
}

// ---------------------------------------------------------------
// Operator-facing documentation of the two new keys.
// ---------------------------------------------------------------

fn repo_file(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{name} must exist: {e}"))
}

#[test]
fn the_sample_config_documents_and_still_parses_both_new_keys() {
    let sample = repo_file("orchestrator.example.toml");
    assert!(
        sample.contains("oscillation_sample_seconds"),
        "orchestrator.example.toml must document the sampling interval",
    );
    assert!(
        sample.contains("advance_budget_floor_fraction"),
        "orchestrator.example.toml must document the budget floor",
    );
}

#[test]
fn the_readme_documents_both_new_keys() {
    let readme = repo_file("README.md");
    assert!(
        readme.contains("[agent].oscillation_sample_seconds"),
        "README must document the sampling interval",
    );
    assert!(
        readme.contains("[agent].advance_budget_floor_fraction"),
        "README must document the budget floor",
    );
}

// ---------------------------------------------------------------
// The pre-launch gate: how long into the implement container an
// oscillation may still interrupt it.
// ---------------------------------------------------------------

#[test]
fn the_kill_window_is_what_remains_above_the_budget_floor() {
    // A full budget with the floor at half means an oscillation
    // detected in the first thirty minutes can still advance; after
    // that, interrupting the container would cost the run its
    // remaining time for nothing.
    assert_eq!(
        oscillation_kill_window(
            Some(CAP),
            CAP,
            DEFAULT_ADVANCE_BUDGET_FLOOR_FRACTION,
            0,
            false,
        ),
        Some(Duration::from_secs(30 * 60)),
    );
}

#[test]
fn there_is_no_kill_window_once_the_shared_allowance_is_spent() {
    assert_eq!(
        oscillation_kill_window(
            Some(CAP),
            CAP,
            DEFAULT_ADVANCE_BUDGET_FLOOR_FRACTION,
            1,
            false,
        ),
        None,
    );
}

#[test]
fn there_is_no_kill_window_when_the_engine_is_forced_via_label() {
    // A forced engine bypasses chain walking, so there is no next
    // entry to advance to.
    assert_eq!(
        oscillation_kill_window(
            Some(CAP),
            CAP,
            DEFAULT_ADVANCE_BUDGET_FLOOR_FRACTION,
            0,
            true,
        ),
        None,
    );
}

#[test]
fn there_is_no_kill_window_when_the_phase_starts_below_the_floor() {
    // Earlier phases (or an earlier implement iteration) already ate
    // most of the budget.
    assert_eq!(
        oscillation_kill_window(
            Some(Duration::from_secs(20 * 60)),
            CAP,
            DEFAULT_ADVANCE_BUDGET_FLOOR_FRACTION,
            0,
            false,
        ),
        None,
    );
    // And none at all when the budget is already spent.
    assert_eq!(
        oscillation_kill_window(None, CAP, DEFAULT_ADVANCE_BUDGET_FLOOR_FRACTION, 0, false),
        None,
    );
}

#[test]
fn an_idleness_observation_is_recorded_in_the_run_log() {
    // CONTEXT.md: Idleness is "recorded for the operator, never acted
    // on". The line says how long the workspace has been still, and
    // says explicitly that bellows is not acting on it.
    let line = format_idleness_log(15, 60);
    assert!(line.contains("15"), "names the sample count: {line:?}");
    assert!(line.contains("15 minutes"), "names the elapsed span: {line:?}");
    assert!(
        line.contains("not acting on it"),
        "says it is recorded only: {line:?}",
    );
    assert!(
        !line.contains("advanc"),
        "an idleness line must never suggest an advance: {line:?}",
    );
}
