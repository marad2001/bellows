use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

use bellows::auth::{Auth, CLAUDE_HOME_IN_CONTAINER};
use bellows::config::{AuthMethod, Config};
use bellows::runner::{self, RunOutcome};
use bellows::sandbox;
use bellows::status::{self, KillPrecheck, OutcomeTransition, StatusContext};
use bellows::tracker;
use bellows::triage::{
    self, render_dry_run_report, render_triage_input, render_triage_kickoff, TriageVerdict, Verdict,
    VerdictState, TRIAGE_INPUT_FILE, TRIAGE_VERDICT_FILE,
};
use bellows::workspace;

#[derive(Parser)]
#[command(name = "bellows", about = "AFK Claude Code orchestrator for Rust repos")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to orchestrator.toml. Defaults to ./orchestrator.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the polling loop in the foreground.
    Run,
    /// First-time auth setup — run once per new install to seed the
    /// credentials volume with an OAuth session via an interactive
    /// `claude login` flow.
    SetupAuth,
    /// Re-authenticate when your Claude Code refresh token has expired.
    /// Same flow as `setup-auth` (interactive container, `claude login`,
    /// credentials volume seeded); different name for the situation.
    RefreshAuth,
    /// Print a one-line summary of the running orchestrator's state.
    Status,
    /// Abort the in-flight bellows run for a specific issue. Force-removes
    /// the sandbox container, transitions the GitHub issue's label from
    /// `agent-in-progress` to `agent-cancelled`, and posts a short
    /// cancellation comment. The running orchestrator detects the missing
    /// label in finalise, opens whatever workspace state existed at kill
    /// time as a draft PR, and returns to polling.
    Kill {
        /// GitHub issue number to cancel.
        issue: u64,
    },
    /// Run the triage skill against one issue or walk the whole
    /// `needs-triage` backlog. With an issue number, triages just that
    /// one (slice T1 / issue #21). With no issue number, walks every
    /// open `needs-triage` issue serially, oldest-first (slice T2 /
    /// issue #22). Per-issue failures are isolated — the backlog drain
    /// keeps going and tallies failures in the end-of-run summary.
    Triage {
        /// GitHub issue number to triage. Omit to drain the whole
        /// open `needs-triage` backlog (oldest-first).
        issue: Option<u64>,
        /// Skip the apply step on every per-issue invocation: print
        /// each verdict and the summary, but make no mutations.
        #[arg(long)]
        dry_run: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(|| PathBuf::from("orchestrator.toml"));

    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run(&config_path).await,
        // `refresh-auth` is a sibling subcommand to `setup-auth` for
        // operator readability — they describe two different
        // situations (first-time install vs token expired) but share
        // the same underlying flow (interactive `claude login` against
        // the credentials volume).
        Command::SetupAuth | Command::RefreshAuth => setup_auth(&config_path).await,
        Command::Status => status_cmd().await,
        Command::Kill { issue } => kill_cmd(&config_path, issue).await,
        Command::Triage { issue, dry_run } => triage_cmd(&config_path, issue, dry_run).await,
    }
}

async fn triage_cmd(config_path: &PathBuf, issue: Option<u64>, dry_run: bool) -> Result<()> {
    let config_text = std::fs::read_to_string(config_path)
        .with_context(|| format!("read config at {}", config_path.display()))?;
    let config = Config::from_str(&config_text)
        .with_context(|| format!("parse config at {}", config_path.display()))?;

    let pat = std::env::var(&config.github.pat_env_var).map_err(|_| {
        anyhow!(
            "env var {} (configured in [github].pat_env_var) is not set",
            config.github.pat_env_var
        )
    })?;
    let client = octocrab::OctocrabBuilder::new()
        .personal_token(pat)
        .build()
        .context("build octocrab client")?;
    let (owner, repo) = parse_owner_repo(&config.repo.url)?;

    match issue {
        Some(n) => triage_one_cmd(&client, &owner, &repo, &config, n, dry_run).await,
        None => triage_backlog_cmd(&client, &owner, &repo, &config, dry_run).await,
    }
}

async fn triage_one_cmd(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    config: &Config,
    issue: u64,
    dry_run: bool,
) -> Result<()> {
    // Targeted form (slice T1 / issue #21). Dispatches the triage
    // skill against a single issue. The backlog-drain form (T2)
    // shares this entry point so the per-issue isolation + verdict-
    // tally contract is the same in both modes.
    match call_triage_one(client, owner, repo, config, issue, dry_run).await {
        Ok(v) => {
            println!("bellows triage: issue #{} -> {}", issue, v);
            Ok(())
        }
        Err(e) => Err(anyhow!(
            "bellows triage: issue #{} failed: {}",
            issue,
            e
        )),
    }
}

async fn triage_backlog_cmd(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    config: &Config,
    dry_run: bool,
) -> Result<()> {
    // Backlog drain (slice T2 / issue #22). Lists every open
    // `needs-triage` issue oldest-first, then iterates serially
    // through T1's per-issue triage path, tallying verdicts and
    // failures into a single end-of-run summary.
    let needs_triage_label = "needs-triage";
    let issues = tracker::list_needs_triage_issues(client, owner, repo, needs_triage_label)
        .await
        .context("list open needs-triage issues")?;

    if issues.is_empty() {
        println!(
            "bellows triage: no open `{}` issues to drain",
            needs_triage_label,
        );
        return Ok(());
    }

    println!(
        "bellows triage: draining {} open `{}` issue(s){}",
        issues.len(),
        needs_triage_label,
        if dry_run { " (dry-run)" } else { "" },
    );

    let summary = triage::drain_backlog(issues, dry_run, |n, dr| async move {
        println!("bellows triage: processing issue #{}", n);
        match call_triage_one(client, owner, repo, config, n, dr).await {
            Ok(v) => {
                println!("bellows triage: issue #{} -> {}", n, v);
                Ok(v)
            }
            Err(e) => {
                // The brief calls out logging failures explicitly:
                // "failures are logged and tallied in the summary."
                eprintln!("bellows triage: issue #{} failed: {}", n, e);
                Err(e)
            }
        }
    })
    .await;

    print!("{}", summary);
    Ok(())
}

/// Map a TriageVerdict's state onto the backlog-drain summary bucket
/// (`triage::Verdict`). The full verdict is applied internally by
/// `call_triage_one`; the drain just needs to tally outcomes by role
/// for the end-of-run summary.
fn verdict_to_summary_bucket(v: &TriageVerdict) -> Verdict {
    match v.state {
        VerdictState::NeedsInfo => Verdict::NeedsInfo,
        VerdictState::ReadyForAgent => Verdict::ReadyForAgent,
        VerdictState::ReadyForHuman => Verdict::ReadyForHuman,
        VerdictState::Wontfix => Verdict::Wontfix,
    }
}

/// Per-issue triage entry point. Slice T1 (#21):
///   1. Fetch the issue body + comments + labels via octocrab on the host.
///   2. Clone the repo into a temp workspace; write the bundle into
///      `.bellows-triage-input.md` and the kickoff into
///      `.bellows-kickoff.md`.
///   3. Launch a sandbox container that runs claude against the kickoff
///      — same image as the implement / review / nit phases; container
///      has NO GitHub credentials and so cannot post or label directly.
///   4. Read + validate `.bellows-triage-verdict.json` produced by the
///      agent. A missing or malformed verdict halts here with the
///      issue untouched (no partial label swap).
///   5. In `--dry-run`: print the verdict, no mutations. Otherwise:
///      for `wontfix-enhancement`, commit the `.out-of-scope/<filename>`
///      precedent directly to master; then call `apply_verdict` to
///      post comments, transition labels, and (for wontfix) close the
///      issue.
///
/// Halt-on-failure: any of fetch / workspace / container / parse /
/// apply error returns `Err(_)`. The backlog drain (T2) records the
/// failure in its `failed` tally and continues with the next issue;
/// the targeted form returns the error to the operator.
async fn call_triage_one(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    config: &Config,
    issue: u64,
    dry_run: bool,
) -> Result<Verdict, String> {
    let bundle = tracker::fetch_issue_with_comments(client, owner, repo, issue)
        .await
        .map_err(|e| format!("fetch issue #{issue}: {e}"))?;

    // Per-invocation throwaway branch name so multiple triage runs
    // against the same repo don't collide on a shared local branch.
    let branch_name = format!("bellows-triage-tmp/{issue}");
    let workspace = workspace::prepare(&config.repo.url, &branch_name)
        .await
        .map_err(|e| format!("prepare workspace: {e}"))?;

    let input_path = workspace.path().join(TRIAGE_INPUT_FILE);
    tokio::fs::write(&input_path, render_triage_input(&bundle))
        .await
        .map_err(|e| format!("write triage input file: {e}"))?;
    let kickoff_path = workspace.path().join(".bellows-kickoff.md");
    tokio::fs::write(&kickoff_path, render_triage_kickoff())
        .await
        .map_err(|e| format!("write triage kickoff: {e}"))?;

    let auth = match config.auth.method {
        AuthMethod::Subscription => Auth::Subscription {
            credentials_volume_name: config.auth.credentials_volume.clone(),
        },
    };
    let repo_slug = bellows::repo_slug(&config.repo.url);
    let deadline = Some(Duration::from_secs(config.agent.wall_clock_minutes.get() * 60));

    let mut log_writer = std::io::stderr();
    let agent_run = sandbox::run_agent(
        &workspace,
        &auth,
        issue,
        &repo_slug,
        &mut log_writer,
        deadline,
    )
    .await
    .map_err(|e| format!("sandbox::run_agent: {e}"))?;
    if agent_run.exit_code != 0 {
        return Err(format!(
            "triage agent exited non-zero (exit {}); stderr tail:\n{}",
            agent_run.exit_code, agent_run.stderr_tail,
        ));
    }

    let verdict_path = workspace.path().join(TRIAGE_VERDICT_FILE);
    let verdict_text = tokio::fs::read_to_string(&verdict_path).await.map_err(|e| {
        format!(
            "read verdict file at {}: {e} (the agent must produce this file)",
            verdict_path.display(),
        )
    })?;
    let verdict = TriageVerdict::parse(&verdict_text)
        .map_err(|e| format!("validate verdict JSON: {e}"))?;

    if dry_run {
        println!("{}", render_dry_run_report(&verdict));
        return Ok(verdict_to_summary_bucket(&verdict));
    }

    // wontfix-enhancement workspace-side mutation: commit the precedent
    // file directly to master BEFORE the closing comment lands, so the
    // comment can reference a file that actually exists on master.
    if verdict.is_wontfix_enhancement() {
        let filename = verdict
            .out_of_scope_filename
            .as_deref()
            .ok_or_else(|| {
                "wontfix-enhancement verdict missing out_of_scope_filename (validator bug)"
                    .to_string()
            })?;
        let content = verdict
            .out_of_scope_content
            .as_deref()
            .ok_or_else(|| {
                "wontfix-enhancement verdict missing out_of_scope_content (validator bug)"
                    .to_string()
            })?;
        let rel_path = format!(".out-of-scope/{filename}");
        let commit_msg = format!(
            "bellows triage: record `{filename}` as out-of-scope (issue #{issue})"
        );
        workspace::commit_to_branch(
            &workspace,
            workspace.default_branch(),
            &commit_msg,
            &[(rel_path, content.to_string())],
        )
        .await
        .map_err(|e| format!("commit_to_branch: {e}"))?;
    }

    tracker::apply_verdict(client, owner, repo, issue, &verdict)
        .await
        .map_err(|e| format!("apply_verdict: {e}"))?;

    Ok(verdict_to_summary_bucket(&verdict))
}

async fn kill_cmd(config_path: &PathBuf, issue: u64) -> Result<()> {
    let config_text = std::fs::read_to_string(config_path)
        .with_context(|| format!("read config at {}", config_path.display()))?;
    let config = Config::from_str(&config_text)
        .with_context(|| format!("parse config at {}", config_path.display()))?;

    // Step 1: confirm bellows is running and busy on the requested issue
    // by reading the slice-9 status file. Using is_pid_alive so a stale
    // status file from a crashed prior bellows is treated as idle.
    let status_path = status::default_status_path().context("resolve status file path")?;
    let parsed = status::read(&status_path)
        .await
        .with_context(|| format!("read status file at {}", status_path.display()))?;
    let alive = parsed.as_ref().is_some_and(|s| status::is_pid_alive(s.pid));

    match status::check_status_for_kill(parsed.as_ref(), alive, issue) {
        KillPrecheck::Refuse(msg) => {
            eprintln!("{msg}");
            std::process::exit(1);
        }
        KillPrecheck::Proceed => {}
    }

    // Step 2: locate + force-remove the sandbox container via a
    // server-side label filter. A `None` here means the orchestrator has
    // status=busy but no container is currently up (transient gap between
    // phases) — still proceed to the GitHub-side transition; the running
    // orchestrator will detect the label flip in its next finalise pass.
    let docker = bollard::Docker::connect_with_local_defaults()
        .context("connect to local docker daemon")?;
    let container_ids = sandbox::find_containers_for_issue(&docker, issue)
        .await
        .context("find containers for issue")?;
    if container_ids.is_empty() {
        println!(
            "bellows: no live sandbox container found for issue #{} (orchestrator likely between phases)",
            issue,
        );
    } else {
        // PR #33 review finding #1: a single `bellows-issue-number=<N>`
        // label can match multiple containers — the live one plus a
        // stopped corpse whose lifecycle force-remove failed. Remove
        // every match so the kill is honest; reporting "removed N
        // container(s)" tells the operator what happened.
        for id in &container_ids {
            sandbox::kill_container(&docker, id)
                .await
                .with_context(|| format!("force-remove sandbox container {id}"))?;
            println!(
                "bellows: removed container {} for issue #{}",
                &id[..id.len().min(12)],
                issue,
            );
        }
        if container_ids.len() > 1 {
            println!(
                "bellows: removed {} containers in total for issue #{} (a prior phase's lifecycle-end force-remove likely failed; both the corpse and the live container shared the label)",
                container_ids.len(),
                issue,
            );
        }
    }

    // Step 3: transition the GitHub issue's label and post the
    // cancellation comment. The running orchestrator's finalise will
    // detect the missing in_progress label and short-circuit cleanly.
    let pat = std::env::var(&config.github.pat_env_var).map_err(|_| {
        anyhow!(
            "env var {} (configured in [github].pat_env_var) is not set",
            config.github.pat_env_var
        )
    })?;
    let client = octocrab::OctocrabBuilder::new()
        .personal_token(pat)
        .build()
        .context("build octocrab client")?;
    let (owner, repo) = parse_owner_repo(&config.repo.url)?;
    tracker::transition_to_cancelled(
        &client,
        &owner,
        &repo,
        issue,
        &config.runtime_labels.agent_in_progress,
        &config.runtime_labels.agent_cancelled,
    )
    .await
    .context("transition issue label to agent-cancelled")?;

    println!(
        "bellows: kill signal sent; agent-cancelled label applied to issue #{}",
        issue,
    );
    Ok(())
}

/// Parse a GitHub repo URL like `https://github.com/owner/repo` (with or
/// Thin wrapper that delegates to `runner::parse_owner_repo` and adapts
/// the error from `RunError` into `anyhow::Error`. Single source of
/// truth lives in runner.rs (where the parser's tests are); main.rs
/// just consumes it. PR #33 review finding #3 fix — the previous
/// per-crate copy would have drifted the moment either was updated.
fn parse_owner_repo(url: &str) -> Result<(String, String)> {
    runner::parse_owner_repo(url).map_err(|e| anyhow!("{e}"))
}

async fn status_cmd() -> Result<()> {
    let path = status::default_status_path().context("resolve status file path")?;
    let parsed = status::read(&path).await;
    match parsed {
        Ok(opt) => {
            let alive = opt.as_ref().is_some_and(|s| status::is_pid_alive(s.pid));
            println!("{}", status::summarise(opt.as_ref(), alive));
            Ok(())
        }
        Err(e) => {
            eprintln!(
                "bellows: status file at {} is malformed: {}",
                path.display(),
                e,
            );
            std::process::exit(1);
        }
    }
}

async fn setup_auth(config_path: &PathBuf) -> Result<()> {
    let config_text = std::fs::read_to_string(config_path)
        .with_context(|| format!("read config at {}", config_path.display()))?;
    let config = Config::from_str(&config_text)
        .with_context(|| format!("parse config at {}", config_path.display()))?;

    let image_tag = sandbox::ensure_policy_image()
        .await
        .context("build/check policy image")?;

    let volume = &config.auth.credentials_volume;
    println!(
        "bellows: launching interactive Claude Code in a container to seed `{}` with OAuth credentials.",
        volume
    );
    println!("bellows: inside the container, type `/login` to start the OAuth flow.");
    println!("bellows: when login completes, type `/exit` to close Claude Code. The container will exit and the volume retains the credentials.");

    let status = tokio::process::Command::new("docker")
        .args([
            "run",
            "-it",
            "--rm",
            "--volume",
            &format!("{volume}:{}", CLAUDE_HOME_IN_CONTAINER),
            "--entrypoint",
            "claude",
            &image_tag,
        ])
        .status()
        .await
        .context("spawn `docker run -it`")?;

    if !status.success() {
        anyhow::bail!("docker run exited with {}", status);
    }

    println!("bellows: setup-auth complete; credentials volume `{}` is seeded.", volume);
    Ok(())
}

async fn run(config_path: &PathBuf) -> Result<()> {
    let config_text = std::fs::read_to_string(config_path)
        .with_context(|| format!("read config at {}", config_path.display()))?;
    let config = Config::from_str(&config_text)
        .with_context(|| format!("parse config at {}", config_path.display()))?;

    let pat = std::env::var(&config.github.pat_env_var).map_err(|_| {
        anyhow!(
            "env var {} (configured in [github].pat_env_var) is not set",
            config.github.pat_env_var
        )
    })?;

    let client = octocrab::OctocrabBuilder::new()
        .personal_token(pat)
        .build()
        .context("build octocrab client")?;

    let interval = Duration::from_secs(config.polling.interval_seconds);
    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.logging.path)
        .with_context(|| format!("open log file at {}", config.logging.path.display()))?;

    log(
        &mut log_file,
        &format!(
            "bellows: polling {} every {}s, log file: {}",
            config.repo.url,
            interval.as_secs(),
            config.logging.path.display(),
        ),
    );

    // Slice 7: clean up any orphan containers from a prior bellows
    // process that didn't shut down cleanly. Best-effort — a flaky
    // Docker daemon shouldn't prevent bellows from running. Note that
    // GitHub issues stuck at agent-in-progress from the killed run
    // are NOT auto-reclaimed; the operator re-labels manually.
    //
    // Per-orphan lines are routed through `log()` so the operator
    // running bellows interactively sees *which* container was cleaned
    // up, not just the summary count.
    match bollard::Docker::connect_with_local_defaults() {
        Ok(docker) => {
            match sandbox::cleanup_orphan_containers(&docker, &mut log_file).await {
                Ok(lines) if lines.is_empty() => log(
                    &mut log_file,
                    "bellows: no orphan containers from prior runs",
                ),
                Ok(lines) => {
                    for line in &lines {
                        log(&mut log_file, line);
                    }
                    log(
                        &mut log_file,
                        &format!(
                            "bellows: cleaned up {} orphan containers from prior runs (any GitHub issues stuck at agent-in-progress need manual re-labelling to retry)",
                            lines.len(),
                        ),
                    );
                }
                Err(e) => log(
                    &mut log_file,
                    &format!(
                        "bellows: orphan-cleanup failed (continuing anyway): {}",
                        format_error_chain(&e),
                    ),
                ),
            }
        }
        Err(e) => log(
            &mut log_file,
            &format!(
                "bellows: could not connect to Docker for orphan cleanup (continuing anyway): {}",
                format_error_chain(&e),
            ),
        ),
    }

    // Slice 9: write an initial idle status so `bellows status` in
    // another terminal can answer "is bellows running?" before the
    // first issue is claimed. The status file lives at
    // `dirs::cache_dir()/bellows/status.json`. Best-effort: a failure
    // to resolve the path or write the initial file is logged but
    // does not prevent bellows from running — the operator still has
    // the log file to fall back on.
    let status_ctx = match status::default_status_path() {
        Ok(p) => {
            let ctx = StatusContext::new(p);
            if let Err(e) = ctx.write_idle().await {
                log(
                    &mut log_file,
                    &format!(
                        "bellows: could not write initial status file at {} (continuing): {}",
                        ctx.path.display(),
                        format_error_chain(&e),
                    ),
                );
            }
            Some(ctx)
        }
        Err(e) => {
            log(
                &mut log_file,
                &format!(
                    "bellows: could not resolve status file path (continuing without it): {}",
                    format_error_chain(&e),
                ),
            );
            None
        }
    };

    // Polling-loop transition tracker (issues #42 and #50). Without
    // dedup, a 30s tick floods the log with identical "idle (no
    // ready-for-agent issues)" lines between events and identical
    // "blocked by PR #N" / `MissingAgentBrief(N)` lines while an
    // ongoing condition persists. `OutcomeTransition` collapses each
    // ongoing-state run to a single line emitted on transition into
    // that state; per-event outcomes (Finalised/Contended/Cancelled)
    // bypass dedup and always log.
    let mut transition = OutcomeTransition::new();

    loop {
        let outcome = runner::run_once(&client, &config, &mut log_file, status_ctx.as_ref()).await;
        match outcome {
            Ok(RunOutcome::Blocked { pr_numbers }) => {
                if let Some(line) = transition.observe_blocked(&pr_numbers) {
                    log(&mut log_file, &line);
                }
            }
            Ok(RunOutcome::Idle) => {
                for line in transition.observe_idle() {
                    log(&mut log_file, &line);
                }
            }
            Ok(RunOutcome::Finalised {
                issue_number,
                pr_number,
                reason,
            }) => {
                if let Some(line) = transition.observe_event() {
                    log(&mut log_file, &line);
                }
                log(
                    &mut log_file,
                    &format!(
                        "bellows: finalised issue #{} -> PR #{} ({:?})",
                        issue_number, pr_number, reason
                    ),
                );
            }
            Ok(RunOutcome::Contended { issue_number }) => {
                if let Some(line) = transition.observe_event() {
                    log(&mut log_file, &line);
                }
                log(
                    &mut log_file,
                    &format!(
                        "bellows: claim contended on issue #{}; will retry next tick",
                        issue_number
                    ),
                );
            }
            Ok(RunOutcome::Cancelled {
                issue_number,
                pr_number,
            }) => {
                if let Some(line) = transition.observe_event() {
                    log(&mut log_file, &line);
                }
                log(
                    &mut log_file,
                    &format!(
                        "bellows: cancelled by operator mid-run — issue #{} -> draft PR #{} (agent-cancelled label applied externally)",
                        issue_number, pr_number,
                    ),
                );
            }
            Err(e) => {
                let shape = e.shape();
                let line = format!("bellows: error: {}", format_error_chain(&e));
                for line in transition.observe_error(&shape, line) {
                    log(&mut log_file, &line);
                }
                // Finding #1 (review of PR #26): if run_once returned Err
                // between write_busy and write_idle (any of ~24 `?`
                // propagations — workspace ops, sandbox calls, tokio::fs,
                // octocrab), the status file is still pinned to the prior
                // CurrentRun. PID-liveness can't save us because the
                // polling-loop process is still alive. Reset to idle here
                // so `bellows status` doesn't lie until the next claim.
                if let Some(ctx) = status_ctx.as_ref()
                    && let Err(e) = ctx.write_idle().await
                {
                    log(
                        &mut log_file,
                        &format!(
                            "bellows: warning: status idle write failed after error: {}",
                            format_error_chain(&e),
                        ),
                    );
                }
            }
        }
        tokio::time::sleep(interval).await;
    }
}

fn format_error_chain(err: &dyn std::error::Error) -> String {
    let mut out = format!("{} (debug: {:?})", err, err);
    let mut source = err.source();
    while let Some(s) = source {
        out.push_str(&format!("\n    caused by: {} (debug: {:?})", s, s));
        source = s.source();
    }
    out
}

fn log(file: &mut File, line: &str) {
    println!("{}", line);
    let _ = writeln!(file, "{}", line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_parses_refresh_auth_subcommand() {
        // The new sibling subcommand must parse — operator typing
        // `bellows refresh-auth` should not be rejected by clap.
        let cli = Cli::try_parse_from(["bellows", "refresh-auth"]);
        assert!(
            cli.is_ok(),
            "bellows refresh-auth must parse: {:?}",
            cli.err(),
        );
    }

    #[test]
    fn cli_still_parses_setup_auth_subcommand() {
        // Adding refresh-auth must not regress the existing setup-auth
        // subcommand.
        let cli = Cli::try_parse_from(["bellows", "setup-auth"]);
        assert!(
            cli.is_ok(),
            "bellows setup-auth must still parse: {:?}",
            cli.err(),
        );
    }

    #[test]
    fn cli_help_lists_both_setup_auth_and_refresh_auth() {
        // `bellows --help` must surface both sibling subcommands so an
        // operator scanning the help knows that refresh-auth exists.
        let help = Cli::command().render_help().to_string();
        assert!(
            help.contains("setup-auth"),
            "top-level --help must list setup-auth: {help}"
        );
        assert!(
            help.contains("refresh-auth"),
            "top-level --help must list refresh-auth: {help}"
        );
    }

    #[test]
    fn cli_parses_triage_without_issue_argument_for_backlog_mode() {
        // Slice T2 (#22): `bellows triage` (no arg) walks every open
        // `needs-triage` issue. Clap must accept the issue argument
        // as optional.
        let cli = Cli::try_parse_from(["bellows", "triage"])
            .expect("bellows triage (no issue arg) must parse for backlog mode");
        match cli.command {
            Some(Command::Triage {
                issue: None,
                dry_run: false,
            }) => {}
            other => panic!(
                "expected Triage with issue=None, dry_run=false; got {:?}",
                match other {
                    Some(Command::Triage { issue, dry_run }) =>
                        format!("Triage {{ issue: {:?}, dry_run: {} }}", issue, dry_run),
                    _ => "non-Triage variant".to_string(),
                }
            ),
        }
    }

    #[test]
    fn cli_parses_triage_with_issue_argument_for_targeted_mode() {
        // Slice T1 (#21) targeted form: `bellows triage <N>` triages
        // just one issue.
        let cli = Cli::try_parse_from(["bellows", "triage", "42"])
            .expect("bellows triage 42 must parse for targeted mode");
        match cli.command {
            Some(Command::Triage {
                issue: Some(42),
                dry_run: false,
            }) => {}
            other => panic!(
                "expected Triage with issue=Some(42), dry_run=false; got {:?}",
                match other {
                    Some(Command::Triage { issue, dry_run }) =>
                        format!("Triage {{ issue: {:?}, dry_run: {} }}", issue, dry_run),
                    _ => "non-Triage variant".to_string(),
                }
            ),
        }
    }

    #[test]
    fn cli_parses_triage_with_dry_run_in_backlog_mode() {
        let cli = Cli::try_parse_from(["bellows", "triage", "--dry-run"])
            .expect("bellows triage --dry-run must parse");
        match cli.command {
            Some(Command::Triage {
                issue: None,
                dry_run: true,
            }) => {}
            _ => panic!("expected Triage with issue=None, dry_run=true"),
        }
    }

    #[test]
    fn cli_parses_triage_with_issue_and_dry_run_in_targeted_mode() {
        let cli = Cli::try_parse_from(["bellows", "triage", "42", "--dry-run"])
            .expect("bellows triage 42 --dry-run must parse");
        match cli.command {
            Some(Command::Triage {
                issue: Some(42),
                dry_run: true,
            }) => {}
            _ => panic!("expected Triage with issue=Some(42), dry_run=true"),
        }
    }

    #[test]
    fn cli_help_lists_the_triage_subcommand() {
        // An operator scanning `bellows --help` must see `triage` so
        // the backlog drain is discoverable without reading the
        // README.
        let help = Cli::command().render_help().to_string();
        assert!(
            help.contains("triage"),
            "top-level --help must list triage: {help}"
        );
    }

    #[test]
    fn cli_triage_help_documents_both_targeted_and_backlog_modes() {
        // The Triage subcommand's help text must communicate that the
        // issue argument is optional — present means targeted, absent
        // means backlog. The two modes are subtle enough that help-text
        // signal beats requiring an operator to consult the README.
        let mut cmd = Cli::command();
        let triage_help = cmd
            .find_subcommand_mut("triage")
            .expect("triage subcommand missing")
            .render_help()
            .to_string();
        let lower = triage_help.to_lowercase();
        assert!(
            lower.contains("backlog") || lower.contains("needs-triage"),
            "triage help must reference the backlog-drain mode: {triage_help}"
        );
        assert!(
            lower.contains("dry-run"),
            "triage help must surface --dry-run: {triage_help}"
        );
    }

    #[test]
    fn cli_help_differentiates_setup_auth_and_refresh_auth_situations() {
        // The two names exist BECAUSE they describe two different
        // operator situations. The help text for each must communicate
        // its situational use case so the operator knows when to use
        // which: setup-auth for first-time install, refresh-auth for
        // an expired token.
        let mut cmd = Cli::command();
        let setup_help = cmd
            .find_subcommand_mut("setup-auth")
            .expect("setup-auth subcommand missing")
            .render_help()
            .to_string();
        assert!(
            setup_help.to_lowercase().contains("first-time"),
            "setup-auth help must mention the first-time-install situation: {setup_help}"
        );

        let mut cmd = Cli::command();
        let refresh_help = cmd
            .find_subcommand_mut("refresh-auth")
            .expect("refresh-auth subcommand missing")
            .render_help()
            .to_string();
        assert!(
            refresh_help.to_lowercase().contains("expired"),
            "refresh-auth help must mention the expired-token situation: {refresh_help}"
        );
    }
}
