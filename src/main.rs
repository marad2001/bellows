use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

use bellows::config::Config;
use bellows::runner::{self, RunOutcome};

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
    }
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

    loop {
        match runner::run_once(&client, &config).await {
            Ok(RunOutcome::Idle) => log(&mut log_file, "bellows: idle (no ready-for-agent issues)"),
            Ok(RunOutcome::Finalised {
                issue_number,
                pr_number,
            }) => log(
                &mut log_file,
                &format!(
                    "bellows: finalised issue #{} -> PR #{}",
                    issue_number, pr_number
                ),
            ),
            Ok(RunOutcome::Contended { issue_number }) => log(
                &mut log_file,
                &format!(
                    "bellows: claim contended on issue #{}; will retry next tick",
                    issue_number
                ),
            ),
            Err(e) => log(&mut log_file, &format!("bellows: error: {}", e)),
        }
        tokio::time::sleep(interval).await;
    }
}

fn log(file: &mut File, line: &str) {
    println!("{}", line);
    let _ = writeln!(file, "{}", line);
}
