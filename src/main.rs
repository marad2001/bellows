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
    /// One-time interactive `claude login` against the credentials volume.
    SetupAuth,
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
        Command::SetupAuth => setup_auth(&config_path).await,
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

    loop {
        let outcome = runner::run_once(&client, &config, &mut log_file).await;
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
            Err(e) => log(
                &mut log_file,
                &format!("bellows: error: {}", format_error_chain(&e)),
            ),
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
