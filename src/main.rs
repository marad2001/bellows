use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

use bellows::auth::CLAUDE_HOME_IN_CONTAINER;
use bellows::config::Config;
use bellows::runner::{self, RunOutcome};
use bellows::sandbox;
use bellows::status::{self, StatusContext};

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
    }
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

    loop {
        let outcome = runner::run_once(&client, &config, &mut log_file, status_ctx.as_ref()).await;
        match outcome {
            Ok(RunOutcome::Idle) => log(&mut log_file, "bellows: idle (no ready-for-agent issues)"),
            Ok(RunOutcome::Finalised {
                issue_number,
                pr_number,
                reason,
            }) => log(
                &mut log_file,
                &format!(
                    "bellows: finalised issue #{} -> PR #{} ({:?})",
                    issue_number, pr_number, reason
                ),
            ),
            Ok(RunOutcome::Contended { issue_number }) => log(
                &mut log_file,
                &format!(
                    "bellows: claim contended on issue #{}; will retry next tick",
                    issue_number
                ),
            ),
            Err(e) => {
                log(
                    &mut log_file,
                    &format!("bellows: error: {}", format_error_chain(&e)),
                );
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
