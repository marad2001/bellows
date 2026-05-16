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
    /// `claude login` (or `codex login`) flow.
    ///
    /// Issue #81 / ADR-0005: `--engine claude` / `--engine codex`
    /// selects which engine's credentials volume the interactive flow
    /// targets. When the flag is omitted the default is the engine of
    /// the first entry of `phases.implement.cli_chain` (model is
    /// ignored for setup-auth/refresh-auth — login is per-subscription,
    /// not per-model).
    SetupAuth {
        /// Engine to seed credentials for. Defaults to the first
        /// chain entry of `phases.implement.cli_chain`.
        #[arg(long, value_name = "NAME")]
        engine: Option<String>,
    },
    /// Manage the per-repo SSH deploy-keys volume (issue #69 /
    /// ADR-0002). Operators populate the volume via `add`, inspect
    /// what's in it via `list`, and clean up via `remove`. Each arm
    /// runs inside a one-shot container with the deploy-keys volume
    /// mounted, so file modes and known_hosts seeding work
    /// consistently regardless of host OS.
    SetupDeployKeys {
        #[command(subcommand)]
        action: SetupDeployKeysAction,
    },
    /// Re-authenticate when your engine's refresh token has expired.
    /// Same flow as `setup-auth` (interactive container, `claude
    /// login` / `codex login`, credentials volume seeded); different
    /// name for the situation.
    RefreshAuth {
        /// Engine to refresh credentials for. Defaults to the first
        /// chain entry of `phases.implement.cli_chain`.
        #[arg(long, value_name = "NAME")]
        engine: Option<String>,
    },
    /// Print a one-line summary of the running orchestrator's state.
    Status,
    /// Abort the in-flight bellows run for a specific issue. Force-removes
    /// the sandbox container, transitions the GitHub issue's label from
    /// `agent-in-progress` to `agent-cancelled`, and posts a short
    /// cancellation comment. The running orchestrator detects the missing
    /// label in finalise, opens whatever workspace state existed at kill
    /// time as a draft PR, and returns to polling.
    ///
    /// Target syntax (issue #35 multi-repo polling):
    ///   `bellows kill <owner>/<name>/<issue>` — explicit form, required
    ///   when multiple `[[repo]]` entries are configured.
    ///   `bellows kill <issue>` — bare form, accepted only when exactly
    ///   one `[[repo]]` is configured. Errors with a clear message
    ///   otherwise.
    Kill {
        /// Either `<owner>/<name>/<issue>` (explicit) or `<issue>`
        /// (bare; single-repo configs only).
        target: String,
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
    /// Inspect or delete Bellows-managed cache volumes. With no flags,
    /// prints a table of every per-repo `bellows-target-*` and the
    /// shared `bellows-cargo-registry` volume — does NOT remove anything
    /// (the default invocation is a dry-run by design, so an unrelated
    /// `bellows prune` never deletes cache data by surprise). `--all`
    /// removes every cache volume (with confirmation; combine with
    /// `--yes` to skip the prompt). `--target <slug>` removes one
    /// per-repo target volume by slug. `--registry` removes the shared
    /// cargo registry volume. The credentials volume is never touched
    /// by any flag combination.
    Prune {
        /// Remove every Bellows-managed cache volume (per-repo target
        /// volumes + the shared cargo registry). Prompts for
        /// confirmation unless `--yes` is also passed.
        #[arg(long, conflicts_with_all = ["target", "registry"])]
        all: bool,
        /// With `--all`, skip the confirmation prompt. Useful for
        /// scripts / CI. Ignored without `--all`.
        #[arg(long)]
        yes: bool,
        /// Remove exactly one per-repo target volume by slug. No
        /// confirmation prompt — the explicit slug IS the confirmation.
        /// Cannot be combined with `--all` or `--registry`.
        #[arg(long, value_name = "SLUG", conflicts_with_all = ["all", "registry"])]
        target: Option<String>,
        /// Remove the shared cargo registry volume
        /// (`bellows-cargo-registry`). No confirmation prompt. Cannot be
        /// combined with `--all` or `--target`.
        #[arg(long, conflicts_with_all = ["target", "all"])]
        registry: bool,
    },
}

#[derive(Subcommand)]
enum SetupDeployKeysAction {
    /// Read a private SSH key from stdin (paste-then-EOF), write it
    /// to the deploy-keys volume at `/<name>` with mode 600, ensure
    /// the volume's `/config` has a Host stanza pointing at
    /// `IdentityFile /home/bellows/.ssh/<name>` with `IdentitiesOnly
    /// yes`, and seed `/known_hosts` via `ssh-keyscan <ssh-host>`.
    /// Idempotent on repeated invocations: re-running `add` for the
    /// same key does not duplicate the Host stanza.
    Add {
        /// Name to give this key inside the volume. Operators
        /// reference this name from `[[repo]] deploy_keys = [...]`.
        name: String,
        /// Host the key authenticates against. Defaults to
        /// `github.com`; override for self-hosted GitHub Enterprise
        /// or other git servers.
        #[arg(long, default_value = "github.com")]
        ssh_host: String,
    },
    /// Print every key filename present in the deploy-keys volume
    /// and the Host stanzas in the volume's `/config`.
    List,
    /// Remove a key from the deploy-keys volume — both the key file
    /// at `/<name>` and the matching Host stanza in `/config`.
    /// Removing a non-existent key is not an error.
    Remove {
        /// Name of the key to remove (the same name passed to `add`).
        name: String,
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
        Command::SetupAuth { engine } => setup_auth(&config_path, engine.as_deref()).await,
        Command::RefreshAuth { engine } => setup_auth(&config_path, engine.as_deref()).await,
        Command::SetupDeployKeys { action } => setup_deploy_keys_cmd(&config_path, action).await,
        Command::Status => status_cmd().await,
        Command::Kill { target } => kill_cmd(&config_path, &target).await,
        Command::Triage { issue, dry_run } => triage_cmd(&config_path, issue, dry_run).await,
        Command::Prune {
            all,
            yes,
            target,
            registry,
        } => prune_cmd(all, yes, target, registry).await,
    }
}

/// Startup validation entry point for `bellows run` and
/// `bellows triage` (issue #69 / ADR-0002 AC9). Maps the Config's
/// `[[repo]]` blocks into the borrow-friendly `DeployKeyRepo` shape
/// the sandbox-side validator consumes, then dispatches. The map step
/// lives in main.rs (not in config or sandbox) so neither module has
/// to know about the other's types.
async fn validate_deploy_keys_at_startup(config: &Config) -> Result<()> {
    let repos: Vec<sandbox::DeployKeyRepo> = config
        .repos
        .iter()
        .map(|r| sandbox::DeployKeyRepo {
            url: r.url.clone(),
            deploy_keys: r.deploy_keys.clone(),
        })
        .collect();
    sandbox::validate_deploy_keys(&repos, &config.auth.ssh_keys_volume)
        .await
        .map_err(|e| anyhow!("{e}"))
}

async fn triage_cmd(config_path: &PathBuf, issue: Option<u64>, dry_run: bool) -> Result<()> {
    let config_text = std::fs::read_to_string(config_path)
        .with_context(|| format!("read config at {}", config_path.display()))?;
    let config = Config::from_str(&config_text)
        .with_context(|| format!("parse config at {}", config_path.display()))?;

    // Issue #69 (ADR-0002) AC9: refuse to start when any [[repo]]
    // deploy_keys references a key name that's not present in the
    // configured ssh_keys_volume. Doing this here — before `bellows
    // triage` claims any work — keeps the failure mode operator-
    // legible rather than surfacing as a confusing cargo-fetch crash
    // inside a container minutes later. No-op when no [[repo]] opts in.
    validate_deploy_keys_at_startup(&config)
        .await
        .context("validate deploy keys")?;

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
    // Issue #35 multi-repo polling deliberately keeps `bellows triage`
    // scoped to a single repo per invocation — the triage skill
    // dispatches against one issue at a time and does not yet have a
    // `<repo>/<issue>` argument shape. The first configured repo is
    // used; operators driving triage against a non-first repo should
    // re-order `[[repo]]` entries or run with a single-repo
    // `orchestrator.toml`. A multi-repo-aware triage CLI is a future
    // enhancement (not in this slice's scope).
    let primary_repo = config
        .repos
        .first()
        .expect("config.repos non-empty by FromStr invariant");
    let (owner, repo) = parse_owner_repo(&primary_repo.url)?;
    if config.repos.len() > 1 {
        eprintln!(
            "bellows triage: multiple repos configured; triaging against {}/{} only (first [[repo]] entry)",
            owner, repo,
        );
    }

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
    let primary_repo = config
        .repos
        .first()
        .expect("config.repos non-empty by FromStr invariant");
    let workspace = workspace::prepare(&primary_repo.url, &branch_name)
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
            engine: bellows::config::Engine::Claude,
            model: None,
            credentials_volume_name: config.auth.claude.credentials_volume.clone(),
        },
    };
    let repo_slug = bellows::repo_slug(&primary_repo.url);
    let repo_label = format!("{}/{}", owner, repo);
    let deadline = Some(Duration::from_secs(config.agent.wall_clock_minutes.get() * 60));

    let mut log_writer = std::io::stderr();
    let agent_run = sandbox::run_agent(
        &workspace,
        &auth,
        issue,
        &repo_label,
        &repo_slug,
        &config.auth.ssh_keys_volume,
        &primary_repo.deploy_keys,
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

/// Dispatch for `bellows prune` (issue #13 / slice 11). Reads no
/// orchestrator config: prune is a one-shot CLI that talks to Docker
/// only. Behaviour matrix:
///
///   • no flags         — list every cache volume, no removal (dry-run).
///   • --all (+--yes)   — remove every cache volume; prompt unless --yes.
///   • --target <slug>  — remove one per-repo target volume.
///   • --registry       — remove the shared cargo registry volume.
///
/// Flag combinations that would be ambiguous (`--target` with `--all`
/// or `--registry`, or `--all` with `--registry`) are rejected at the
/// clap layer; this function only sees the four legal shapes above.
async fn prune_cmd(
    all: bool,
    yes: bool,
    target: Option<String>,
    registry: bool,
) -> Result<()> {
    let docker = bollard::Docker::connect_with_local_defaults()
        .context("connect to local docker daemon")?;

    if let Some(slug) = target {
        let name = bellows::target_volume_name_from_slug(&slug);
        return remove_one_volume(&docker, &name, "target").await;
    }
    if registry {
        return remove_one_volume(
            &docker,
            sandbox::CARGO_REGISTRY_VOLUME_NAME,
            "registry",
        )
        .await;
    }

    let volumes = sandbox::list_cache_volumes(&docker)
        .await
        .context("list bellows-managed cache volumes")?;

    if !all {
        // Dry-run / inspect mode: print the table and exit. The brief
        // pins this as "an unrelated `bellows prune` invocation should
        // never delete cache data by surprise" — the table is the only
        // side effect.
        print!("{}", format_volume_table(&volumes));
        Ok(())
    } else {
        prune_all(&docker, volumes, yes).await
    }
}

/// Removes the shared cargo registry OR a per-repo target volume by
/// name. Shared between the `--registry` and `--target <slug>` paths
/// so the not-found / in-use error mapping reads the same in both.
async fn remove_one_volume(
    docker: &bollard::Docker,
    name: &str,
    label: &str,
) -> Result<()> {
    match sandbox::remove_cache_volume(docker, name).await {
        Ok(()) => {
            println!("bellows prune: removed {} volume `{}`", label, name);
            Ok(())
        }
        Err(sandbox::SandboxError::VolumeNotFound { .. }) => {
            eprintln!("bellows prune: {} volume `{}` does not exist", label, name);
            std::process::exit(1);
        }
        Err(e) => Err(anyhow!("remove {} volume `{}`: {}", label, name, e)),
    }
}

/// `--all` body: optionally prompt, then remove every cache volume the
/// discovery returned. Confirmation reads a single line from stdin —
/// any line starting with `y`/`Y` proceeds; everything else aborts.
async fn prune_all(
    docker: &bollard::Docker,
    volumes: Vec<sandbox::CacheVolume>,
    yes: bool,
) -> Result<()> {
    if volumes.is_empty() {
        println!("bellows prune: no Bellows-managed cache volumes to remove");
        return Ok(());
    }

    println!("bellows prune: the following cache volumes will be removed:");
    print!("{}", format_volume_table(&volumes));

    if !yes && !confirm_destructive_action()? {
        println!("bellows prune: aborted; no volumes removed");
        return Ok(());
    }

    let mut failures = 0u32;
    for v in &volumes {
        match sandbox::remove_cache_volume(docker, &v.name).await {
            Ok(()) => println!("bellows prune: removed `{}`", v.name),
            Err(e) => {
                eprintln!("bellows prune: failed to remove `{}`: {}", v.name, e);
                failures += 1;
            }
        }
    }
    if failures > 0 {
        anyhow::bail!(
            "bellows prune: {failures} of {} volumes failed to remove",
            volumes.len(),
        );
    }
    Ok(())
}

/// Read one line from stdin and accept it as confirmation if it starts
/// with `y` (case-insensitive). Anything else (including a bare RET) is
/// "no". Pulled out so the prune-all body stays readable; the prompt
/// text is intentionally explicit about what's about to happen.
fn confirm_destructive_action() -> Result<bool> {
    use std::io::{BufRead, Write as _};
    print!("Remove every Bellows-managed cache volume listed above? [y/N]: ");
    std::io::stdout()
        .flush()
        .context("flush stdout before confirmation prompt")?;
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("read confirmation from stdin")?;
    Ok(line.trim_start().chars().next().is_some_and(|c| c == 'y' || c == 'Y'))
}

/// Render a list of cache volumes as a stdout table. Always emits a
/// header line so an empty list still tells the operator that bellows
/// looked and found nothing — easier to read than a silent exit.
///
/// Columns: NAME (full volume name), KIND (`target` / `cargo-registry`),
/// REPO-SLUG (empty for the shared registry), SIZE (only when Docker
/// returned UsageData — usually absent on the `/volumes` list endpoint).
fn format_volume_table(volumes: &[sandbox::CacheVolume]) -> String {
    let mut rows: Vec<[String; 4]> = Vec::with_capacity(volumes.len() + 1);
    rows.push([
        "NAME".to_string(),
        "KIND".to_string(),
        "REPO-SLUG".to_string(),
        "SIZE".to_string(),
    ]);
    for v in volumes {
        let (kind, slug) = match &v.kind {
            sandbox::CacheVolumeKind::Target { repo_slug } => {
                ("target".to_string(), repo_slug.clone())
            }
            sandbox::CacheVolumeKind::CargoRegistry => {
                ("cargo-registry".to_string(), "-".to_string())
            }
        };
        let size = match v.size_bytes {
            Some(b) => format!("{}", b),
            None => "-".to_string(),
        };
        rows.push([v.name.clone(), kind, slug, size]);
    }

    let mut widths = [0usize; 4];
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            if cell.len() > widths[i] {
                widths[i] = cell.len();
            }
        }
    }

    let mut out = String::new();
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            out.push_str(cell);
            if i + 1 < row.len() {
                for _ in cell.len()..widths[i] {
                    out.push(' ');
                }
            }
        }
        out.push('\n');
    }
    if volumes.is_empty() {
        out.push_str("(no Bellows-managed cache volumes found)\n");
    }
    out
}

/// Resolved `bellows kill` target — both the repo slug ("owner/name")
/// and the issue number, after the optional `<repo>/` prefix has been
/// parsed and reconciled against `config.repos`.
#[derive(Debug)]
struct ResolvedKillTarget {
    /// `owner/name`. Used both for the GitHub-side label transition AND
    /// for the `bellows-repo` container-filter label so the kill never
    /// matches a different repo's container with the same issue number.
    repo_label: String,
    /// The fully-qualified repo URL from the matching `[[repo]]` entry.
    /// Needed for `parse_owner_repo` (which the runner shares with this
    /// path) and matches whatever URL form the operator wrote.
    repo_url: String,
    /// The issue number to cancel.
    issue: u64,
}

/// Parse the `bellows kill <target>` argument and resolve it against
/// the configured repos. Pure function (no IO) so each branch is
/// directly unit-testable.
///
/// Accepted shapes (issue #35):
///   `<owner>/<name>/<issue>` — explicit form. The `<owner>/<name>`
///     slug must match exactly one configured `[[repo]]` entry's URL
///     (via `parse_owner_repo`); otherwise the kill refuses so a
///     typo doesn't silently target nothing.
///   `<issue>` — bare form. Accepted only when exactly one repo is
///     configured; rejected with a clear message in multi-repo
///     configs so the operator gets an explicit prompt rather than a
///     silent default.
fn resolve_kill_target(
    target: &str,
    repos: &[bellows::config::RepoConfig],
) -> Result<ResolvedKillTarget> {
    let (qualifier, issue_str) = match target.rsplit_once('/') {
        Some((q, rest)) => (Some(q), rest),
        None => (None, target),
    };
    let issue: u64 = issue_str.parse().map_err(|_| {
        anyhow!(
            "bellows kill: invalid issue number `{}` (expected `<owner>/<name>/<issue>` or bare `<issue>`)",
            issue_str,
        )
    })?;

    if let Some(qualifier) = qualifier {
        // `<owner>/<name>/<issue>` shape — match against configured repos.
        // Skip entries whose URL fails to parse so a single malformed
        // `[[repo]]` does not pre-empt the match for unrelated entries.
        // Matches the shape of the `configured` list built in the
        // not-found branch below, which already tolerates parse failures
        // via `filter_map`.
        let mut found: Option<(String, String)> = None;
        for r in repos {
            if let Ok((owner, repo)) = parse_owner_repo(&r.url) {
                let slug = format!("{}/{}", owner, repo);
                if slug == qualifier {
                    found = Some((slug, r.url.clone()));
                    break;
                }
            }
        }
        let (repo_label, repo_url) = found.ok_or_else(|| {
            let configured: Vec<String> = repos
                .iter()
                .filter_map(|r| {
                    parse_owner_repo(&r.url)
                        .ok()
                        .map(|(o, n)| format!("{}/{}", o, n))
                })
                .collect();
            anyhow!(
                "bellows kill: `{}` does not match any configured [[repo]] entry. Configured: {:?}",
                qualifier,
                configured,
            )
        })?;
        Ok(ResolvedKillTarget {
            repo_label,
            repo_url,
            issue,
        })
    } else {
        // Bare `<issue>` shape — accepted only when there's exactly one
        // configured repo. With multiple repos the bare form is
        // ambiguous so we refuse explicitly.
        if repos.len() != 1 {
            anyhow::bail!(
                "bellows kill: bare `<issue>` form is only valid with a single configured [[repo]]; \
                 {} are configured. Re-run as `bellows kill <owner>/<name>/{}`.",
                repos.len(),
                issue,
            );
        }
        let r = &repos[0];
        let (owner, repo) = parse_owner_repo(&r.url)?;
        Ok(ResolvedKillTarget {
            repo_label: format!("{}/{}", owner, repo),
            repo_url: r.url.clone(),
            issue,
        })
    }
}

async fn kill_cmd(config_path: &PathBuf, target: &str) -> Result<()> {
    let config_text = std::fs::read_to_string(config_path)
        .with_context(|| format!("read config at {}", config_path.display()))?;
    let config = Config::from_str(&config_text)
        .with_context(|| format!("parse config at {}", config_path.display()))?;

    let resolved = resolve_kill_target(target, &config.repos)?;
    let issue = resolved.issue;

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
    // server-side label filter. The filter is scoped to BOTH the repo
    // slug and the issue number (issue #35) so a kill against repo A's
    // `#42` never accidentally takes out repo B's `#42`. A `None` here
    // means the orchestrator has status=busy but no container is
    // currently up (transient gap between phases) — still proceed to
    // the GitHub-side transition; the running orchestrator will detect
    // the label flip in its next finalise pass.
    let docker = bollard::Docker::connect_with_local_defaults()
        .context("connect to local docker daemon")?;
    let container_ids =
        sandbox::find_containers_for_issue(&docker, &resolved.repo_label, issue)
            .await
            .context("find containers for issue")?;
    if container_ids.is_empty() {
        println!(
            "bellows: no live sandbox container found for {}#{} (orchestrator likely between phases)",
            resolved.repo_label, issue,
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
                "bellows: removed container {} for {}#{}",
                &id[..id.len().min(12)],
                resolved.repo_label,
                issue,
            );
        }
        if container_ids.len() > 1 {
            println!(
                "bellows: removed {} containers in total for {}#{} (a prior phase's lifecycle-end force-remove likely failed; both the corpse and the live container shared the label)",
                container_ids.len(),
                resolved.repo_label,
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
    let (owner, repo) = parse_owner_repo(&resolved.repo_url)?;
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
        "bellows: kill signal sent; agent-cancelled label applied to {}#{}",
        resolved.repo_label, issue,
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

/// Validate a deploy-key `name` at CLI-parse time. The name becomes a
/// filename in the deploy-keys volume and is interpolated into shell
/// scripts (single-quoted in `docker run -c "..."`), so reject anything
/// that could break out of either context. The allowed set is
/// `[A-Za-z0-9._-]+`, which covers every realistic crate/repo handle
/// without leaving room for `'`, `$`, `/`, `\n`, or other shell-special
/// or path-traversal characters.
///
/// Additionally rejects the reserved metadata filenames `config` and
/// `known_hosts` (the volume's SSH config and known-hosts files) and the
/// path-resolution literals `.` and `..` — a name like `config` would
/// pass the character-class check but pipe the operator's private key
/// through `cat > /sshvol/config`, clobbering the volume's SSH config;
/// `known_hosts` would do the same and then `chmod 644` the private key
/// world-readable. `.` and `..` would resolve to `/sshvol` / the parent
/// directory under `cat > /sshvol/<name>`.
fn validate_deploy_key_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("deploy key name must not be empty"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(anyhow!(
            "deploy key name `{name}` contains characters outside `[A-Za-z0-9._-]`; \
             the name becomes a filename in the deploy-keys volume and a shell argument, \
             so only those characters are allowed",
        ));
    }
    if matches!(name, "config" | "known_hosts" | "." | "..") {
        return Err(anyhow!(
            "deploy key name `{name}` is reserved (the deploy-keys volume uses \
             `config` and `known_hosts` for its SSH metadata, and `.`/`..` resolve \
             to directory paths); pick a different name like the crate or repo handle",
        ));
    }
    Ok(())
}

/// Validate an `ssh-host` value at CLI-parse time. The host is
/// interpolated into shell scripts (Host stanza, `ssh-keyscan`,
/// `ssh-keygen -F`), so reject anything outside a conservative
/// hostname character class. `[A-Za-z0-9.-]+` covers `github.com`,
/// `git.example.com`, and any reasonable GHE host; nothing else is
/// needed.
///
/// Also rejects values starting with `-`: while the character class
/// already prevents shell-injection escapes, a leading dash makes
/// `ssh-keyscan -H <host>` and `ssh-keygen -F <host>` parse the value
/// as an option (e.g. `-V` prints version and exits), silently
/// bypassing the known_hosts seeding step and leaving the agent unable
/// to verify the real host key. The `--` separator inside the add
/// script catches the same case as defence-in-depth, but rejecting it
/// up-front gives the operator a clear error message.
fn validate_ssh_host(host: &str) -> Result<()> {
    if host.is_empty() {
        return Err(anyhow!("ssh-host must not be empty"));
    }
    if host.starts_with('-') {
        return Err(anyhow!(
            "ssh-host `{host}` must not start with `-`; \
             a leading dash makes ssh-keyscan / ssh-keygen treat the value as an \
             option flag, silently skipping the host-key seeding step",
        ));
    }
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
    {
        return Err(anyhow!(
            "ssh-host `{host}` contains characters outside `[A-Za-z0-9.-]`; \
             use a plain hostname like `github.com` or `git.example.com`",
        ));
    }
    Ok(())
}

/// Shell script the `add` arm runs inside a one-shot container with
/// the deploy-keys volume mounted at `/sshvol` (issue #69 / ADR-0002).
/// stdin is piped through to the container, so `cat > /sshvol/<name>`
/// captures the operator's paste; the rest of the script chmod's the
/// key, idempotently ensures the Host stanza is in `/sshvol/config`,
/// and idempotently seeds `/sshvol/known_hosts` via `ssh-keyscan`.
///
/// Pure function — the value is the text of the script run via
/// `sh -c "..."`. Tested for shape (paths, chmod 600, Host stanza
/// fields, idempotence guard, host-key seeding) so the contract can
/// evolve without silently dropping a required step.
fn build_deploy_keys_add_script(name: &str, ssh_host: &str) -> String {
    let identity = format!("/home/bellows/.ssh/{name}");
    // Notes on idempotence:
    //   * the Host-stanza guard greps for the unique IdentityFile
    //     line, which is plaintext in `/sshvol/config`;
    //   * the known_hosts guard uses `ssh-keygen -F` rather than
    //     plain `grep`, because `ssh-keyscan -H` hashes hostnames in
    //     its output (`|1|<b64>|<b64> ssh-ed25519 ...`) so a plain
    //     `grep -F 'github.com '` would never match and every re-run
    //     would append another triplet. `ssh-keygen -F` is the
    //     canonical search and resolves hashed entries.
    // The Host-stanza body uses `\\n` (escaped) because the outer Rust
    // format includes the literal backslash-n that `printf` then
    // expands to a real newline at shell runtime.
    //
    // The seeding block is built separately so the github.com arm can
    // verify ssh-keyscan output against the pinned fingerprint table
    // before appending — defeating a network-position attacker who
    // substitutes a host key during the first `bellows setup-deploy-keys
    // add` invocation.
    let seeding = build_known_hosts_seeding_block(ssh_host);
    // Final `chown -R 1000:1000 /sshvol`: the one-shot container that
    // runs this script runs as `--user 0` (root) so it can write to a
    // fresh-volume mount point that Docker created `root:root`. Without
    // a follow-up chown, the produced files stay root-owned; when the
    // agent / cargo-checks gate container later mounts this volume at
    // `/home/bellows/.ssh/` read-only and runs as bellows (uid 1000),
    // bellows cannot read the `root:root` mode-600 key — SSH silently
    // fails with "Permission denied (publickey)" and cargo reports the
    // dep as unfetchable. The chown is run at the end so it covers both
    // the key file and the `config` / `known_hosts` files (regardless of
    // which conditional branches the script took). Idempotent — `chown
    // -R` on a directory that already has the correct ownership is a
    // no-op. Issue #73.
    format!(
        "set -e\n\
         umask 077\n\
         cat > /sshvol/{name}\n\
         chmod 600 /sshvol/{name}\n\
         touch /sshvol/config\n\
         chmod 600 /sshvol/config\n\
         if ! grep -F -q 'IdentityFile {identity}' /sshvol/config; then\n\
             printf 'Host {ssh_host}\\n    IdentityFile {identity}\\n    IdentitiesOnly yes\\n\\n' >> /sshvol/config\n\
         fi\n\
         touch /sshvol/known_hosts\n\
         chmod 644 /sshvol/known_hosts\n\
         if ! ssh-keygen -F {ssh_host} -f /sshvol/known_hosts >/dev/null 2>&1; then\n\
         {seeding}\
         fi\n\
         chown -R 1000:1000 /sshvol\n",
        name = name,
        identity = identity,
        ssh_host = ssh_host,
        seeding = seeding,
    )
}

/// Pinned SHA256 host-key fingerprints for well-known SSH hosts. The
/// add script verifies `ssh-keyscan` output against this table before
/// trusting it, so a network-position attacker who substitutes a host
/// key at first contact cannot poison `/sshvol/known_hosts`.
///
/// Source: <https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/githubs-ssh-key-fingerprints>
/// (ed25519, ECDSA, RSA — the three host key types github.com publishes).
fn pinned_host_key_fingerprints(ssh_host: &str) -> Option<&'static [&'static str]> {
    match ssh_host {
        "github.com" => Some(&[
            "SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU",
            "SHA256:p2QAMXNIC1TJYWeIOttrVc98/R1BUFWu3/LiyKgUfQM",
            "SHA256:uNiVztksCsDhcc0u9e8BujQXVUpKZIDTMczCvj3tD2s",
        ]),
        _ => None,
    }
}

/// Shell fragment that seeds `/sshvol/known_hosts` for `ssh_host`. The
/// fragment is inlined into the `add` script's `if ! ssh-keygen -F ...`
/// branch so it only runs on first seeding for a given host.
///
/// For pinned hosts (currently `github.com`), the fragment runs
/// `ssh-keyscan` to a temp file, computes the SHA256 fingerprints, and
/// aborts unless every returned fingerprint matches a value in the
/// pinned table — closing the trust-on-first-use gap. For unpinned
/// hosts the fragment still runs `ssh-keyscan` but prints the
/// fingerprints to stderr so the operator can verify them out-of-band
/// before relying on the host.
///
/// `--` separates options from the positional host so a `-V`-style
/// host name (caught up-front by `validate_ssh_host` but kept here as
/// defence-in-depth) cannot be parsed as a flag by `ssh-keyscan`.
fn build_known_hosts_seeding_block(ssh_host: &str) -> String {
    // Lines have no leading whitespace — shell does not care about
    // indentation, and Rust's `\<newline>` continuation eats leading
    // whitespace on the next source line anyway.
    let common_preamble = format!(
        "KEYSCAN_TMP=$(mktemp)\n\
         ssh-keyscan -H -- {ssh_host} > \"$KEYSCAN_TMP\" 2>/dev/null\n\
         if [ ! -s \"$KEYSCAN_TMP\" ]; then\n\
         rm -f \"$KEYSCAN_TMP\"\n\
         echo 'bellows: ssh-keyscan returned no host keys for {ssh_host}' >&2\n\
         exit 1\n\
         fi\n",
        ssh_host = ssh_host,
    );
    let body = match pinned_host_key_fingerprints(ssh_host) {
        Some(expected_fps) => {
            let expected = expected_fps.join("\n");
            format!(
                "EXPECTED_FPS='{expected}'\n\
                 GOT_FPS=$(ssh-keygen -lf \"$KEYSCAN_TMP\" | awk '{{print $2}}')\n\
                 for fp in $GOT_FPS; do\n\
                 if ! printf '%s\\n' \"$EXPECTED_FPS\" | grep -F -q -x -- \"$fp\"; then\n\
                 echo \"bellows: SECURITY: ssh-keyscan returned fingerprint $fp which is NOT in the pinned list for {ssh_host}\" >&2\n\
                 echo 'bellows: expected one of:' >&2\n\
                 printf '%s\\n' \"$EXPECTED_FPS\" >&2\n\
                 echo 'bellows: refusing to trust this host key (possible MitM); aborting' >&2\n\
                 rm -f \"$KEYSCAN_TMP\"\n\
                 exit 1\n\
                 fi\n\
                 done\n",
                expected = expected,
                ssh_host = ssh_host,
            )
        }
        None => format!(
            "echo 'bellows: {ssh_host} is not in the pinned-fingerprint table; trusting on first use.' >&2\n\
             echo 'bellows: fingerprints being added to /sshvol/known_hosts:' >&2\n\
             ssh-keygen -lf \"$KEYSCAN_TMP\" >&2\n\
             echo 'bellows: verify these against an out-of-band source before depending on this host.' >&2\n",
            ssh_host = ssh_host,
        ),
    };
    format!(
        "{common_preamble}\
         {body}\
         cat \"$KEYSCAN_TMP\" >> /sshvol/known_hosts\n\
         rm -f \"$KEYSCAN_TMP\"\n",
        common_preamble = common_preamble,
        body = body,
    )
}

/// Shell script the `list` arm runs inside the one-shot container.
/// Prints filenames in `/sshvol/` (one per line) under a `keys:`
/// header, then the `config` file under a `config:` header so the
/// operator can read every Host stanza at a glance.
fn build_deploy_keys_list_script() -> String {
    "set -e\n\
     echo 'keys:'\n\
     ls -1A /sshvol 2>/dev/null | grep -v -x -e config -e known_hosts || true\n\
     echo ''\n\
     echo 'config:'\n\
     cat /sshvol/config 2>/dev/null || echo '(no /sshvol/config — run `bellows setup-deploy-keys add <name>` to create it)'\n"
        .to_string()
}

/// Shell script the `remove` arm runs inside the one-shot container.
/// Removes the key file at `/sshvol/<name>` (rm -f so a missing key
/// is not an error) AND the matching Host stanza from `/sshvol/config`
/// (located by its `IdentityFile /home/bellows/.ssh/<name>` line; the
/// awk filter spans from the preceding `Host` line through the next
/// blank line so the block is removed cleanly).
fn build_deploy_keys_remove_script(name: &str) -> String {
    let identity = format!("/home/bellows/.ssh/{name}");
    // Clear `block` when the matching IdentityFile arms `in_block`:
    // otherwise the deferred Host header survives in `block` past the
    // body, and if the target stanza is the file's last entry the END
    // rule resurrects the orphaned `Host ...` line. Clearing on entry
    // makes the END rule's `if (block != "")` correctly skip the
    // doomed header in every case.
    format!(
        "set -e\n\
         rm -f /sshvol/{name}\n\
         if [ -f /sshvol/config ]; then\n\
             awk -v identity='{identity}' 'BEGIN {{ in_block=0 }}\n\
                 /^Host / {{ block=$0; in_block=0; next }}\n\
                 $0 ~ \"IdentityFile \" identity {{ in_block=1; block=\"\"; next }}\n\
                 in_block && /^$/ {{ in_block=0; next }}\n\
                 in_block {{ next }}\n\
                 /^$/ {{ if (block != \"\") {{ print block }} block=\"\"; print; next }}\n\
                 {{ if (block != \"\") {{ print block; block=\"\" }} print }}\n\
                 END {{ if (block != \"\") print block }}\n\
             ' /sshvol/config > /sshvol/config.new\n\
             mv /sshvol/config.new /sshvol/config\n\
             chmod 600 /sshvol/config\n\
             chown 1000:1000 /sshvol/config\n\
         fi\n",
        name = name,
        identity = identity,
    )
}

/// Dispatch for `bellows setup-deploy-keys add | list | remove`
/// (issue #69 / ADR-0002 ACs 3, 4, 5). Each arm builds the
/// corresponding shell script and runs it inside a one-shot
/// policy-image container with the deploy-keys volume mounted at
/// `/sshvol`. `add` is run with `-i` so stdin is piped through to the
/// container; `list` and `remove` don't read stdin.
async fn setup_deploy_keys_cmd(
    config_path: &PathBuf,
    action: SetupDeployKeysAction,
) -> Result<()> {
    let config_text = std::fs::read_to_string(config_path)
        .with_context(|| format!("read config at {}", config_path.display()))?;
    let config = Config::from_str(&config_text)
        .with_context(|| format!("parse config at {}", config_path.display()))?;
    let volume = &config.auth.ssh_keys_volume;

    let image_tag = sandbox::ensure_policy_image()
        .await
        .context("build/check policy image")?;

    match action {
        SetupDeployKeysAction::Add { name, ssh_host } => {
            validate_deploy_key_name(&name)?;
            validate_ssh_host(&ssh_host)?;
            println!(
                "bellows: importing deploy key `{name}` into volume `{volume}` (host: {ssh_host})."
            );
            println!("bellows: paste the PRIVATE half of the key, then press Ctrl-D (EOF).");
            let script = build_deploy_keys_add_script(&name, &ssh_host);
            let status = tokio::process::Command::new("docker")
                .args([
                    "run",
                    "-i",
                    "--rm",
                    "--user",
                    "0",
                    "--volume",
                    &format!("{volume}:/sshvol"),
                    "--entrypoint",
                    "sh",
                    &image_tag,
                    "-c",
                    &script,
                ])
                .status()
                .await
                .context("spawn `docker run -i` for setup-deploy-keys add")?;
            if !status.success() {
                anyhow::bail!("docker run (add) exited with {}", status);
            }
            println!("bellows: deploy key `{name}` added.");
        }
        SetupDeployKeysAction::List => {
            let script = build_deploy_keys_list_script();
            let status = tokio::process::Command::new("docker")
                .args([
                    "run",
                    "--rm",
                    "--user",
                    "0",
                    "--volume",
                    &format!("{volume}:/sshvol:ro"),
                    "--entrypoint",
                    "sh",
                    &image_tag,
                    "-c",
                    &script,
                ])
                .status()
                .await
                .context("spawn `docker run` for setup-deploy-keys list")?;
            if !status.success() {
                anyhow::bail!("docker run (list) exited with {}", status);
            }
        }
        SetupDeployKeysAction::Remove { name } => {
            validate_deploy_key_name(&name)?;
            let script = build_deploy_keys_remove_script(&name);
            let status = tokio::process::Command::new("docker")
                .args([
                    "run",
                    "--rm",
                    "--user",
                    "0",
                    "--volume",
                    &format!("{volume}:/sshvol"),
                    "--entrypoint",
                    "sh",
                    &image_tag,
                    "-c",
                    &script,
                ])
                .status()
                .await
                .context("spawn `docker run` for setup-deploy-keys remove")?;
            if !status.success() {
                anyhow::bail!("docker run (remove) exited with {}", status);
            }
            println!("bellows: deploy key `{name}` removed (no-op if it was already absent).");
        }
    }
    Ok(())
}

/// Docker argv for the engine-specific interactive login flow driven
/// by `setup_auth` (issue #100). Pure helper so the inline tests can
/// pin the argv shape per engine.
///
/// Claude path is unchanged from before issue #100: docker runs the
/// claude entrypoint with no positional args. The operator types
/// `/login` inside the interactive claude TUI to start the OAuth
/// flow — that slash command is Claude Code's idiom for triggering
/// the browser-OAuth handoff.
///
/// Codex path runs `codex login --device-auth` directly. Codex has no
/// `/login` slash command (the binary only ships `/org-setup`), so
/// dropping into the interactive TUI gave the operator no way to
/// drive OAuth — the original bug. The `login --device-auth`
/// subcommand makes codex print a device-authorisation URL + short
/// code, wait, and write `auth.json` into the mounted credentials
/// volume once the operator approves the device. Treated as the
/// container command by docker since the args appear after the image
/// tag.
fn setup_auth_docker_args(
    engine: bellows::config::Engine,
    volume: &str,
    home_in_container: &str,
    image_tag: &str,
) -> Vec<String> {
    // `--entrypoint <engine>` bypasses the bellows entrypoint script
    // that normally `chown`s the cache mount points and `runuser`s to
    // the bellows user. Without `--user` + an explicit `HOME` the
    // container runs as root with `HOME=/root`, so:
    //   - codex writes `auth.json` to `/root/.codex` (ephemeral; lost
    //     on container exit — the original #100 symptom);
    //   - if claude ever has to write fresh credentials it would land
    //     in `/root/.claude` for the same reason (currently masked by
    //     Claude Code v2.1's auto-detect logic, but latent).
    // Pin both flags so login files land in the mounted volume AND
    // are owned by `bellows:bellows` (uid 1000 — baked by the
    // Dockerfile's `useradd --uid 1000 bellows`). That ownership is
    // what subsequent agent containers — which also run as bellows
    // uid via runuser — need in order to READ the credentials.
    let mut args: Vec<String> = vec![
        "run".to_string(),
        "-it".to_string(),
        "--rm".to_string(),
        "--user".to_string(),
        "1000:1000".to_string(),
        "-e".to_string(),
        "HOME=/home/bellows".to_string(),
        "--volume".to_string(),
        format!("{volume}:{home_in_container}"),
        "--entrypoint".to_string(),
        engine.as_name().to_string(),
        image_tag.to_string(),
    ];
    match engine {
        bellows::config::Engine::Claude => {}
        bellows::config::Engine::Codex => {
            args.push("login".to_string());
            args.push("--device-auth".to_string());
        }
    }
    args
}

/// Pre-flight text printed before `docker run` so the operator knows
/// how to drive the engine's OAuth flow (issue #100). Engine-specific
/// because the two CLIs use different idioms — claude has a `/login`
/// slash command inside its TUI, codex has no slash commands and
/// drives OAuth via the `login --device-auth` subcommand the
/// container is now launched with.
fn setup_auth_preflight_instructions(
    engine: bellows::config::Engine,
    volume: &str,
) -> String {
    let engine_name = engine.as_name();
    let mut out = String::new();
    out.push_str(&format!(
        "bellows: launching interactive {engine_name} in a container to seed `{volume}` with OAuth credentials.\n"
    ));
    match engine {
        bellows::config::Engine::Claude => {
            out.push_str(
                "bellows: inside the container, type `/login` to start the OAuth flow.\n",
            );
            out.push_str(&format!(
                "bellows: when login completes, type `/exit` to close {engine_name}. The container will exit and the volume retains the credentials.\n"
            ));
        }
        bellows::config::Engine::Codex => {
            out.push_str(
                "bellows: codex will print a device-authorisation URL and a short code. Open the URL in any browser where you're signed in to your codex account, paste the code, and approve the device.\n",
            );
            out.push_str(
                "bellows: codex writes auth.json into the credentials volume once the device is approved, then exits on its own — the container exits and the volume retains the credentials.\n",
            );
        }
    }
    out
}

async fn setup_auth(config_path: &PathBuf, engine_flag: Option<&str>) -> Result<()> {
    let config_text = std::fs::read_to_string(config_path)
        .with_context(|| format!("read config at {}", config_path.display()))?;
    let config = Config::from_str(&config_text)
        .with_context(|| format!("parse config at {}", config_path.display()))?;

    // Issue #81 / ADR-0005: `--engine claude` / `--engine codex`.
    // Default = the engine of the first chain entry of
    // `phases.implement.cli_chain`. Model pins on chain entries are
    // ignored for setup-auth/refresh-auth (login is per-subscription,
    // not per-model).
    let engine = match engine_flag {
        Some(name) => bellows::config::Engine::from_name(name).ok_or_else(|| {
            anyhow!(
                "unknown --engine `{name}` (expected `claude` or `codex`)",
            )
        })?,
        None => config.phases.implement.first_entry().engine,
    };

    let image_tag = sandbox::ensure_policy_image()
        .await
        .context("build/check policy image")?;

    let engine_auth = config.auth.for_engine(engine);
    let volume = &engine_auth.credentials_volume;
    let engine_name = engine.as_name();
    let home_in_container = match engine {
        bellows::config::Engine::Claude => CLAUDE_HOME_IN_CONTAINER,
        bellows::config::Engine::Codex => bellows::auth::CODEX_HOME_IN_CONTAINER,
    };

    // Engine-specific pre-flight text + docker argv (issue #100). The
    // helpers are pure so the inline tests can pin the per-engine
    // shape; the runtime path just prints + spawns docker.
    print!("{}", setup_auth_preflight_instructions(engine, volume));
    let docker_args = setup_auth_docker_args(engine, volume, home_in_container, &image_tag);
    let status = tokio::process::Command::new("docker")
        .args(&docker_args)
        .status()
        .await
        .context("spawn `docker run -it`")?;

    if !status.success() {
        anyhow::bail!("docker run exited with {}", status);
    }

    println!(
        "bellows: setup-auth complete; {engine_name} credentials volume `{}` is seeded.",
        volume,
    );
    Ok(())
}

async fn run(config_path: &PathBuf) -> Result<()> {
    let config_text = std::fs::read_to_string(config_path)
        .with_context(|| format!("read config at {}", config_path.display()))?;
    let config = Config::from_str(&config_text)
        .with_context(|| format!("parse config at {}", config_path.display()))?;

    // Issue #69 (ADR-0002) AC9: refuse to start when any [[repo]]
    // deploy_keys references a key name that's not present in the
    // configured ssh_keys_volume. Fail-fast here is much friendlier
    // than letting the agent crash mid-cargo-fetch later. No-op when
    // no [[repo]] opts in.
    validate_deploy_keys_at_startup(&config)
        .await
        .context("validate deploy keys")?;

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

    let repo_urls = config
        .repos
        .iter()
        .map(|r| r.url.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    log(
        &mut log_file,
        &format!(
            "bellows: polling {} repo(s) [{}] every {}s, log file: {}",
            config.repos.len(),
            repo_urls,
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
            Ok(RunOutcome::Blocked { reason }) => {
                if let Some(line) = transition.observe_blocked(&reason) {
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
    fn cli_parses_setup_deploy_keys_add_with_default_ssh_host() {
        // Issue #69 (ADR-0002) AC3 acceptance: `bellows
        // setup-deploy-keys add <name>` parses with the ssh-host
        // defaulting to `github.com`. The name is positional; the
        // host is an optional `--ssh-host <host>` flag.
        let cli = Cli::try_parse_from(["bellows", "setup-deploy-keys", "add", "workboard-core"])
            .expect("bellows setup-deploy-keys add <name> must parse");
        match cli.command {
            Some(Command::SetupDeployKeys {
                action: SetupDeployKeysAction::Add { name, ssh_host },
            }) => {
                assert_eq!(name, "workboard-core");
                assert_eq!(ssh_host, "github.com", "default ssh-host must be github.com");
            }
            other => panic!("expected SetupDeployKeys::Add, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn cli_parses_setup_deploy_keys_add_with_explicit_ssh_host() {
        // Acceptance: operators with self-hosted git servers can
        // override the ssh-host via `--ssh-host`.
        let cli = Cli::try_parse_from([
            "bellows",
            "setup-deploy-keys",
            "add",
            "my-key",
            "--ssh-host",
            "git.example.com",
        ])
        .expect("bellows setup-deploy-keys add --ssh-host must parse");
        match cli.command {
            Some(Command::SetupDeployKeys {
                action: SetupDeployKeysAction::Add { name, ssh_host },
            }) => {
                assert_eq!(name, "my-key");
                assert_eq!(ssh_host, "git.example.com");
            }
            _ => panic!("expected SetupDeployKeys::Add with custom ssh_host"),
        }
    }

    #[test]
    fn cli_parses_setup_deploy_keys_list_subcommand() {
        // Acceptance AC4: `bellows setup-deploy-keys list` must parse.
        let cli = Cli::try_parse_from(["bellows", "setup-deploy-keys", "list"])
            .expect("bellows setup-deploy-keys list must parse");
        match cli.command {
            Some(Command::SetupDeployKeys {
                action: SetupDeployKeysAction::List,
            }) => {}
            _ => panic!("expected SetupDeployKeys::List"),
        }
    }

    #[test]
    fn cli_parses_setup_deploy_keys_remove_subcommand() {
        // Acceptance AC5: `bellows setup-deploy-keys remove <name>`
        // must parse, name is positional.
        let cli = Cli::try_parse_from(["bellows", "setup-deploy-keys", "remove", "workboard-core"])
            .expect("bellows setup-deploy-keys remove <name> must parse");
        match cli.command {
            Some(Command::SetupDeployKeys {
                action: SetupDeployKeysAction::Remove { name },
            }) => assert_eq!(name, "workboard-core"),
            _ => panic!("expected SetupDeployKeys::Remove"),
        }
    }

    #[test]
    fn cli_help_lists_setup_deploy_keys_subcommand() {
        // The new subcommand is discoverable from `bellows --help`.
        let help = Cli::command().render_help().to_string();
        assert!(
            help.contains("setup-deploy-keys"),
            "top-level --help must list setup-deploy-keys: {help}"
        );
    }

    #[test]
    fn validate_deploy_key_name_accepts_realistic_handles() {
        for name in ["workboard-core", "my_key", "v2.0", "abc123", "a"] {
            assert!(
                validate_deploy_key_name(name).is_ok(),
                "expected `{name}` to validate",
            );
        }
    }

    #[test]
    fn validate_deploy_key_name_rejects_shell_special_and_path_chars() {
        // Each of these would break the shell scripts that interpolate
        // the name into single-quoted `docker run -c "..."` text, or
        // would escape the /sshvol/ prefix.
        for name in [
            "",
            "foo'bar",
            "foo$IFS",
            "foo/bar",
            "../etc/passwd",
            "foo bar",
            "foo\nbar",
            "foo;rm",
        ] {
            assert!(
                validate_deploy_key_name(name).is_err(),
                "expected `{name}` to be rejected",
            );
        }
    }

    #[test]
    fn validate_deploy_key_name_rejects_reserved_metadata_filenames() {
        // `config` and `known_hosts` are the volume's SSH metadata files;
        // accepting either as a deploy-key name would route the operator's
        // private-key paste into the metadata file (overwriting config or
        // leaving the key world-readable under chmod 644 of known_hosts).
        // `.` and `..` resolve to directory paths under `cat > /sshvol/<name>`.
        for name in ["config", "known_hosts", ".", ".."] {
            assert!(
                validate_deploy_key_name(name).is_err(),
                "expected reserved name `{name}` to be rejected",
            );
        }
    }

    #[test]
    fn validate_ssh_host_accepts_realistic_hosts() {
        for host in ["github.com", "git.example.com", "ghe.internal", "host-1.io"] {
            assert!(
                validate_ssh_host(host).is_ok(),
                "expected `{host}` to validate",
            );
        }
    }

    #[test]
    fn validate_ssh_host_rejects_shell_special_chars() {
        for host in ["", "foo'bar", "foo$x", "foo bar", "foo;rm", "foo/bar", "foo_bar"] {
            assert!(
                validate_ssh_host(host).is_err(),
                "expected `{host}` to be rejected",
            );
        }
    }

    #[test]
    fn validate_ssh_host_rejects_leading_dash() {
        // Values starting with `-` pass the character-class check but
        // get parsed as option flags by ssh-keyscan / ssh-keygen,
        // silently bypassing the known_hosts seeding step. The
        // validator must reject them up-front (with `--` as a
        // defence-in-depth shield inside the script).
        for host in ["-V", "--help", "-G", "-foo.com"] {
            assert!(
                validate_ssh_host(host).is_err(),
                "expected leading-dash host `{host}` to be rejected",
            );
        }
    }

    #[test]
    fn build_deploy_keys_add_script_uses_double_dash_before_keyscan_host() {
        // Defence-in-depth: even though validate_ssh_host rejects
        // leading-dash hosts, `ssh-keyscan -H -- <host>` halts option
        // parsing so a future relaxation of the character class cannot
        // re-open the flag-injection gap.
        let script = build_deploy_keys_add_script("my-key", "github.com");
        assert!(
            script.contains("ssh-keyscan -H -- github.com"),
            "add script must pass `--` before the ssh-keyscan host: {script}",
        );
    }

    #[test]
    fn build_deploy_keys_add_script_pins_github_fingerprints() {
        // Closes the trust-on-first-use gap for github.com: the add
        // script must verify ssh-keyscan output against GitHub's
        // published SHA256 host-key fingerprints before appending to
        // /sshvol/known_hosts. Source:
        // https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/githubs-ssh-key-fingerprints
        let script = build_deploy_keys_add_script("my-key", "github.com");
        for expected in [
            "SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU",
            "SHA256:p2QAMXNIC1TJYWeIOttrVc98/R1BUFWu3/LiyKgUfQM",
            "SHA256:uNiVztksCsDhcc0u9e8BujQXVUpKZIDTMczCvj3tD2s",
        ] {
            assert!(
                script.contains(expected),
                "github.com add script must pin fingerprint `{expected}`: {script}",
            );
        }
        // The script must actually compare returned fingerprints
        // against the pinned list, not merely contain the constants.
        assert!(
            script.contains("ssh-keygen -lf"),
            "add script must compute fingerprints with `ssh-keygen -lf`: {script}",
        );
        assert!(
            script.contains("EXPECTED_FPS"),
            "add script must bind the pinned list to EXPECTED_FPS for comparison: {script}",
        );
        assert!(
            script.contains("refusing to trust this host key"),
            "add script must abort (not warn) on fingerprint mismatch: {script}",
        );
    }

    #[test]
    fn build_deploy_keys_add_script_does_not_pin_unknown_hosts() {
        // For hosts not in the pinned-fingerprint table the script
        // still seeds known_hosts (TOFU, matching the OpenSSH default)
        // but must print the fingerprints to stderr so the operator
        // can verify them out-of-band.
        let script = build_deploy_keys_add_script("my-key", "ghe.internal");
        assert!(
            !script.contains("SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU"),
            "unpinned host must not carry github.com's pinned fingerprints: {script}",
        );
        assert!(
            script.contains("ssh-keygen -lf"),
            "unpinned host script must still print fingerprints via `ssh-keygen -lf`: {script}",
        );
        assert!(
            script.contains("trusting on first use"),
            "unpinned host script must surface its TOFU posture to the operator: {script}",
        );
    }

    #[test]
    fn build_deploy_keys_add_script_writes_to_volume_with_mode_600() {
        // Acceptance AC3 pinning: the add script must (a) read stdin
        // and write to /sshvol/<name>, (b) chmod 600 the result, and
        // (c) idempotently ensure the Host stanza is in /sshvol/config
        // pointing at IdentityFile /home/bellows/.ssh/<name> with
        // IdentitiesOnly yes, (d) seed known_hosts via ssh-keyscan.
        let script = build_deploy_keys_add_script("workboard-core", "github.com");
        // (a) writes to /sshvol/<name>
        assert!(
            script.contains("/sshvol/workboard-core"),
            "add script must write to /sshvol/<name>: {script}",
        );
        // (b) chmod 600
        assert!(
            script.contains("chmod 600"),
            "add script must chmod 600 the key file: {script}",
        );
        // (c) Host stanza pointing at the in-container path
        assert!(
            script.contains("Host github.com"),
            "add script must add Host stanza: {script}",
        );
        assert!(
            script.contains("IdentityFile /home/bellows/.ssh/workboard-core"),
            "Host stanza must reference the in-container identity path: {script}",
        );
        assert!(
            script.contains("IdentitiesOnly yes"),
            "Host stanza must include IdentitiesOnly yes: {script}",
        );
        // (d) ssh-keyscan seeds known_hosts
        assert!(
            script.contains("ssh-keyscan"),
            "add script must seed known_hosts via ssh-keyscan: {script}",
        );
        assert!(
            script.contains("github.com"),
            "ssh-keyscan must target the configured host: {script}",
        );
    }

    #[test]
    fn build_deploy_keys_add_script_is_idempotent_on_repeated_invocations() {
        // Acceptance AC3: "Idempotent on subsequent adds." Pin the
        // contract: the script must guard BOTH the Host-stanza append
        // AND the known_hosts seeding so running add twice for the
        // same key doesn't duplicate either.
        let script = build_deploy_keys_add_script("my-key", "github.com");

        // Host-stanza guard: the IdentityFile path is unique to this
        // key, so greping for it before appending is the canonical
        // shape.
        assert!(
            script.contains("grep -F -q 'IdentityFile /home/bellows/.ssh/my-key'"),
            "Host-stanza append must be guarded by a grep for the unique IdentityFile line: {script}",
        );

        // known_hosts guard: ssh-keyscan -H hashes hostnames in its
        // output (`|1|<b64>|<b64> ssh-ed25519 ...`) so a plain
        // `grep -F 'github.com '` against the file will never match
        // and every re-run will append another triplet. The guard
        // must use `ssh-keygen -F` (which resolves hashed entries) or
        // drop `-H` so plaintext matching works.
        let uses_hashed_keyscan = script.contains("ssh-keyscan -H");
        let uses_keygen_search = script.contains("ssh-keygen -F github.com");
        let uses_broken_plain_grep_guard = script
            .contains("grep -F -q 'github.com ' /sshvol/known_hosts");
        assert!(
            !uses_broken_plain_grep_guard,
            "known_hosts guard `grep -F 'github.com '` cannot match hashed entries written by `ssh-keyscan -H` — \
             every re-run will re-append. Use `ssh-keygen -F` instead, or drop `-H` from ssh-keyscan: {script}",
        );
        assert!(
            uses_keygen_search || !uses_hashed_keyscan,
            "known_hosts guard must locate entries written by `ssh-keyscan -H` (hashed) — \
             use `ssh-keygen -F <host>` to search, or drop `-H` so plaintext matching works: {script}",
        );
    }

    #[test]
    fn build_deploy_keys_add_script_chowns_volume_to_bellows_uid() {
        // Issue #73: the setup-deploy-keys add container runs as
        // `--user 0` (root), so files it writes to /sshvol stay
        // root-owned. When the volume is later mounted into an agent
        // / cargo-checks gate container at /home/bellows/.ssh/
        // read-only and that container drops to bellows (uid 1000),
        // bellows cannot read the root:root mode-600 key — SSH fails
        // with "Permission denied (publickey)" and cargo can't fetch
        // the dep. The script must chown the volume to 1000:1000
        // (bellows's uid, baked by the policy image's `useradd
        // --uid 1000 bellows`) before exiting. Run at the end so it
        // covers every file regardless of which conditional branches
        // the script took.
        let script = build_deploy_keys_add_script("workboard-core", "github.com");
        assert!(
            script.contains("chown -R 1000:1000 /sshvol"),
            "add script must chown the volume to bellows uid 1000 so the agent container \
             can read the keys (issue #73): {script}",
        );
    }

    #[test]
    fn build_deploy_keys_remove_script_removes_key_and_host_stanza() {
        // Acceptance AC5: remove must delete /sshvol/<name> AND the
        // matching Host stanza from /sshvol/config. Idempotent on
        // missing key.
        let script = build_deploy_keys_remove_script("workboard-core");
        assert!(
            script.contains("/sshvol/workboard-core"),
            "remove script must target the key file: {script}",
        );
        assert!(
            script.contains("rm -f") || script.contains("rm --force"),
            "remove must use -f so missing key is not an error: {script}",
        );
        // The config edit needs to know the IdentityFile path so it
        // can locate the matching Host stanza.
        assert!(
            script.contains("/home/bellows/.ssh/workboard-core"),
            "remove script must reference the in-container identity path to locate the Host stanza: {script}",
        );
    }

    #[test]
    fn build_deploy_keys_remove_script_clears_deferred_host_header_on_match() {
        // Regression: if the awk filter sets `in_block=1` without
        // clearing `block`, the END rule resurrects the matching
        // stanza's Host header when the file does not end with a
        // trailing blank line — leaving an orphan `Host github.com`
        // with no IdentityFile. Pin the contract: the IdentityFile-
        // match rule must clear `block`.
        let script = build_deploy_keys_remove_script("workboard-core");
        assert!(
            script.contains("in_block=1; block=\"\""),
            "remove script must clear `block` when entering in_block so the \
             END rule does not resurrect an orphan Host header: {script}",
        );
    }

    #[test]
    fn build_deploy_keys_remove_script_chowns_config_to_bellows_uid() {
        // Issue #73: `mv config.new config` inside the awk-rewrite
        // block recreates /sshvol/config as root:root (the script runs
        // as root). Subsequent agent containers can't read it under
        // bellows uid 1000. The script must chown the rewritten config
        // back to 1000:1000 before exiting, matching the add script's
        // post-write chown.
        let script = build_deploy_keys_remove_script("workboard-core");
        assert!(
            script.contains("chown 1000:1000 /sshvol/config")
                || script.contains("chown -R 1000:1000 /sshvol"),
            "remove script must chown /sshvol/config back to bellows uid 1000 after \
             the awk-rewrite recreates it as root:root (issue #73): {script}",
        );
    }

    #[test]
    fn build_deploy_keys_list_script_emits_filenames_and_host_stanzas() {
        // Acceptance AC4: list prints key filenames + Host stanzas
        // from /sshvol/config. Operator who's lost track of which
        // keys live in the volume runs this; the output is the
        // tracking shape.
        let script = build_deploy_keys_list_script();
        assert!(
            script.contains("ls") || script.contains("find"),
            "list must enumerate files in /sshvol: {script}",
        );
        assert!(
            script.contains("/sshvol"),
            "list must operate inside /sshvol: {script}",
        );
        // The config has the Host stanzas; surfacing it is the whole
        // point of `list` (per the brief).
        assert!(
            script.contains("config"),
            "list must include the config file: {script}",
        );
    }

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
    fn cli_parses_prune_without_flags_for_dry_run_listing() {
        // `bellows prune` (no flags) is the safe default: list every
        // cache volume, remove nothing. The brief calls this out
        // explicitly — an unrelated invocation must not delete data
        // by surprise, so the dry-run path is reachable from the bare
        // subcommand.
        let cli = Cli::try_parse_from(["bellows", "prune"])
            .expect("bellows prune (no flags) must parse for dry-run listing");
        match cli.command {
            Some(Command::Prune {
                all: false,
                yes: false,
                target: None,
                registry: false,
            }) => {}
            _ => panic!("expected Prune with all=false, yes=false, target=None, registry=false"),
        }
    }

    #[test]
    fn cli_parses_prune_with_all_flag() {
        let cli = Cli::try_parse_from(["bellows", "prune", "--all"])
            .expect("bellows prune --all must parse");
        match cli.command {
            Some(Command::Prune {
                all: true,
                yes: false,
                target: None,
                registry: false,
            }) => {}
            _ => panic!("expected Prune with all=true, yes=false"),
        }
    }

    #[test]
    fn cli_parses_prune_with_all_and_yes_for_script_mode() {
        // `--yes` combined with `--all` is the scriptable / CI form:
        // remove everything without prompting. Both flags must parse
        // together.
        let cli = Cli::try_parse_from(["bellows", "prune", "--all", "--yes"])
            .expect("bellows prune --all --yes must parse");
        match cli.command {
            Some(Command::Prune {
                all: true,
                yes: true,
                target: None,
                registry: false,
            }) => {}
            _ => panic!("expected Prune with all=true, yes=true"),
        }
    }

    #[test]
    fn cli_parses_prune_with_target_slug() {
        let cli =
            Cli::try_parse_from(["bellows", "prune", "--target", "marad2001-bellows"])
                .expect("bellows prune --target <slug> must parse");
        match cli.command {
            Some(Command::Prune {
                all: false,
                yes: false,
                target: Some(slug),
                registry: false,
            }) if slug == "marad2001-bellows" => {}
            _ => panic!("expected Prune with target=Some(marad2001-bellows)"),
        }
    }

    #[test]
    fn cli_parses_prune_with_registry_flag() {
        let cli = Cli::try_parse_from(["bellows", "prune", "--registry"])
            .expect("bellows prune --registry must parse");
        match cli.command {
            Some(Command::Prune {
                all: false,
                yes: false,
                target: None,
                registry: true,
            }) => {}
            _ => panic!("expected Prune with registry=true"),
        }
    }

    #[test]
    fn cli_rejects_prune_with_target_and_all_combined() {
        // Brief: mixing `--target` with `--all` is a clap-level usage
        // error so the intent is unambiguous. A single-volume scope and
        // a wipe-everything scope cannot coexist.
        let result =
            Cli::try_parse_from(["bellows", "prune", "--all", "--target", "some-slug"]);
        assert!(
            result.is_err(),
            "bellows prune --all --target <slug> must be a clap usage error",
        );
    }

    #[test]
    fn cli_rejects_prune_with_target_and_registry_combined() {
        // Brief: mixing `--target` with `--registry` is also a usage
        // error — the per-repo target volume and the shared registry
        // volume are different things; the operator must pick one.
        let result = Cli::try_parse_from([
            "bellows",
            "prune",
            "--target",
            "some-slug",
            "--registry",
        ]);
        assert!(
            result.is_err(),
            "bellows prune --target <slug> --registry must be a clap usage error",
        );
    }

    #[test]
    fn cli_rejects_prune_with_all_and_registry_combined() {
        // Mixing `--all` with `--registry` is a clap-level usage error so
        // the wider scope cannot silently swallow the narrower one. The
        // operator who types `bellows prune --all --registry` must get a
        // usage error rather than have `--all` discarded and only the
        // registry volume removed.
        let result =
            Cli::try_parse_from(["bellows", "prune", "--all", "--registry"]);
        assert!(
            result.is_err(),
            "bellows prune --all --registry must be a clap usage error",
        );
    }

    #[test]
    fn cli_help_lists_the_prune_subcommand() {
        // An operator scanning `bellows --help` must see `prune` so the
        // cache-volume tooling is discoverable without reading the
        // README.
        let help = Cli::command().render_help().to_string();
        assert!(
            help.contains("prune"),
            "top-level --help must list prune: {help}"
        );
    }

    #[test]
    fn cli_prune_help_documents_default_dry_run_and_destructive_flags() {
        // The Prune subcommand's help text is the operator's first
        // line of defense against running the wrong shape. The brief
        // pins the no-flag invocation as a dry-run by design and lists
        // the destructive flags; the help must surface both so the
        // safety bias is visible at the CLI.
        let mut cmd = Cli::command();
        let prune_help = cmd
            .find_subcommand_mut("prune")
            .expect("prune subcommand missing")
            .render_help()
            .to_string();
        let lower = prune_help.to_lowercase();
        assert!(
            lower.contains("dry-run") || lower.contains("does not remove") || lower.contains("does not delete"),
            "prune help must communicate that no-flag is a dry-run: {prune_help}"
        );
        for flag in ["--all", "--target", "--registry"] {
            assert!(
                prune_help.contains(flag),
                "prune help must list {flag}: {prune_help}",
            );
        }
    }

    #[test]
    fn format_volume_table_lists_each_volume_with_kind_and_repo_slug() {
        // The default `bellows prune` invocation prints a table of
        // every Bellows-managed cache volume. Pin the columns the
        // brief asks for (name, kind, repo-slug for target volumes)
        // and the inclusion of both kinds in the body.
        let volumes = vec![
            sandbox::CacheVolume {
                name: "bellows-target-marad2001-bellows".to_string(),
                kind: sandbox::CacheVolumeKind::Target {
                    repo_slug: "marad2001-bellows".to_string(),
                },
                size_bytes: None,
            },
            sandbox::CacheVolume {
                name: "bellows-cargo-registry".to_string(),
                kind: sandbox::CacheVolumeKind::CargoRegistry,
                size_bytes: Some(1_048_576),
            },
        ];
        let rendered = format_volume_table(&volumes);
        // Header columns from the brief.
        assert!(rendered.contains("NAME"), "missing NAME column: {rendered}");
        assert!(rendered.contains("KIND"), "missing KIND column: {rendered}");
        assert!(
            rendered.contains("REPO-SLUG"),
            "missing REPO-SLUG column: {rendered}"
        );
        // Body rows include both volume names + kinds + the per-repo
        // target's repo slug.
        assert!(rendered.contains("bellows-target-marad2001-bellows"));
        assert!(rendered.contains("bellows-cargo-registry"));
        assert!(rendered.contains("target"));
        assert!(rendered.contains("cargo-registry"));
        assert!(rendered.contains("marad2001-bellows"));
        // Size column populated when Docker reported usage data.
        assert!(rendered.contains("1048576"));
    }

    #[test]
    fn format_volume_table_handles_empty_list_with_explicit_message() {
        // Better to emit a header + "(no volumes found)" line than a
        // silent exit — the operator needs to know bellows looked.
        let rendered = format_volume_table(&[]);
        assert!(rendered.contains("NAME"));
        assert!(
            rendered.to_lowercase().contains("no")
                && rendered.to_lowercase().contains("found"),
            "empty list rendering must explicitly say nothing was found: {rendered}"
        );
    }

    #[test]
    fn cli_parses_setup_auth_with_engine_claude_flag() {
        // Issue #81 / ADR-0005: `bellows setup-auth --engine claude`
        // selects which engine's credentials volume the interactive
        // flow targets. The flag is optional; the omitted-flag
        // default is the engine of the first chain entry of
        // `phases.implement.cli_chain` (resolved in setup_auth).
        let cli = Cli::try_parse_from(["bellows", "setup-auth", "--engine", "claude"])
            .expect("bellows setup-auth --engine claude must parse");
        match cli.command {
            Some(Command::SetupAuth { engine: Some(e) }) => {
                assert_eq!(e, "claude");
            }
            _ => panic!("expected SetupAuth with engine=Some(claude)"),
        }
    }

    #[test]
    fn cli_parses_setup_auth_with_engine_codex_flag() {
        let cli = Cli::try_parse_from(["bellows", "setup-auth", "--engine", "codex"])
            .expect("bellows setup-auth --engine codex must parse");
        match cli.command {
            Some(Command::SetupAuth { engine: Some(e) }) => {
                assert_eq!(e, "codex");
            }
            _ => panic!("expected SetupAuth with engine=Some(codex)"),
        }
    }

    #[test]
    fn cli_parses_refresh_auth_with_engine_codex_flag() {
        // Same flag exists on refresh-auth (the brief: "bellows
        // setup-auth --engine {claude,codex} and bellows refresh-auth
        // --engine {claude,codex} flags").
        let cli = Cli::try_parse_from(["bellows", "refresh-auth", "--engine", "codex"])
            .expect("bellows refresh-auth --engine codex must parse");
        match cli.command {
            Some(Command::RefreshAuth { engine: Some(e) }) => {
                assert_eq!(e, "codex");
            }
            _ => panic!("expected RefreshAuth with engine=Some(codex)"),
        }
    }

    #[test]
    fn cli_setup_auth_engine_flag_is_optional() {
        // Default-from-first-implement-chain-entry-engine is resolved
        // in setup_auth at config-load time; clap accepts the omitted
        // flag.
        let cli = Cli::try_parse_from(["bellows", "setup-auth"])
            .expect("bellows setup-auth (no flag) must still parse");
        match cli.command {
            Some(Command::SetupAuth { engine: None }) => {}
            _ => panic!("expected SetupAuth with engine=None"),
        }
    }

    // ---- Issue #100: codex setup-auth drives `codex login --device-auth`,
    //                  claude path is unchanged ----
    //
    // The bug fixed here: `bellows setup-auth --engine codex` previously
    // dropped into the codex interactive TUI with no args and printed
    // "type `/login` to start the OAuth flow". Codex has no `/login`
    // slash command (the binary only ships `/org-setup`), so no OAuth
    // ever completed and `auth.json` never landed in the credentials
    // volume — every codex-engine pipeline run then 401'd inside the
    // container. The fix branches the docker argv and pre-flight
    // instructions on engine: claude keeps the interactive-TUI + `/login`
    // flow; codex invokes `codex login --device-auth` directly so codex
    // prints a device URL + code and writes `auth.json` back through the
    // mount when the operator approves the device in their host browser.
    //
    // Both subcommands share the same impl (Command::SetupAuth and
    // Command::RefreshAuth both call setup_auth), so one fix covers
    // setup-auth and refresh-auth in lock-step.

    #[test]
    fn setup_auth_docker_args_for_claude_have_no_positional_args_after_image_tag() {
        // Claude path is unchanged: docker runs the interactive claude
        // TUI with no positional args, then the operator types `/login`
        // inside the TUI. Pin the argv shape so a future regression
        // would flip the test.
        let args = setup_auth_docker_args(
            bellows::config::Engine::Claude,
            "bellows-claude-credentials",
            "/home/bellows/.claude",
            "bellows-policy:abc123",
        );
        // The image tag is the LAST positional arg for claude — no
        // `login` / `--device-auth` etc. after it.
        assert_eq!(
            args.last().map(|s| s.as_str()),
            Some("bellows-policy:abc123"),
            "claude args must end at the image tag with no positional args after it: {args:?}",
        );
        // The engine name must follow `--entrypoint`.
        let entrypoint_idx = args.iter().position(|a| a == "--entrypoint")
            .expect("claude args must include --entrypoint");
        assert_eq!(
            args.get(entrypoint_idx + 1).map(|s| s.as_str()),
            Some("claude"),
            "claude --entrypoint value must be `claude`: {args:?}",
        );
    }

    #[test]
    fn setup_auth_docker_args_for_codex_invoke_login_with_device_auth_after_image_tag() {
        // Codex path: docker runs `codex login --device-auth` (i.e. the
        // `login` subcommand with the `--device-auth` flag) as the
        // container's command, so codex completes a device-auth OAuth
        // flow inside the running container and writes `auth.json` back
        // through the mounted credentials volume.
        let args = setup_auth_docker_args(
            bellows::config::Engine::Codex,
            "bellows-codex-credentials",
            "/home/bellows/.codex",
            "bellows-policy:abc123",
        );
        // The engine name must follow `--entrypoint` (so codex is the
        // entrypoint; docker treats remaining args as the container
        // command).
        let entrypoint_idx = args.iter().position(|a| a == "--entrypoint")
            .expect("codex args must include --entrypoint");
        assert_eq!(
            args.get(entrypoint_idx + 1).map(|s| s.as_str()),
            Some("codex"),
            "codex --entrypoint value must be `codex`: {args:?}",
        );
        // `login` and `--device-auth` must both follow the image tag
        // as positional args (docker treats them as args to the codex
        // entrypoint). Order matters — codex's CLI shape is
        // `codex login --device-auth`.
        let image_tag_idx = args.iter().position(|a| a == "bellows-policy:abc123")
            .expect("codex args must include the image tag");
        assert_eq!(
            args.get(image_tag_idx + 1).map(|s| s.as_str()),
            Some("login"),
            "codex args must place `login` immediately after the image tag: {args:?}",
        );
        assert_eq!(
            args.get(image_tag_idx + 2).map(|s| s.as_str()),
            Some("--device-auth"),
            "codex args must place `--device-auth` immediately after `login`: {args:?}",
        );
    }

    #[test]
    fn setup_auth_docker_args_run_as_bellows_uid_with_home_env_for_both_engines() {
        // Issue #100 follow-up: `--entrypoint <engine>` bypasses the
        // bellows entrypoint script that normally `runuser`s to the
        // bellows user. Without `--user 1000:1000 -e HOME=/home/bellows`
        // the container runs as root with HOME=/root, so the engine
        // writes its credentials file to /root/.<engine> — ephemeral,
        // lost on container exit. Pin both flags for both engines so
        // login files land in the mounted volume AND are owned by
        // `bellows:bellows` (uid 1000 — readable by subsequent agent
        // containers, which also run as bellows uid 1000).
        for engine in [bellows::config::Engine::Claude, bellows::config::Engine::Codex] {
            let args = setup_auth_docker_args(engine, "v", "/h", "img:tag");
            assert!(
                args.windows(2).any(|w| w[0] == "--user" && w[1] == "1000:1000"),
                "{engine:?} args must include `--user 1000:1000` so credentials are bellows-owned: {args:?}",
            );
            assert!(
                args.windows(2).any(|w| w[0] == "-e" && w[1] == "HOME=/home/bellows"),
                "{engine:?} args must include `-e HOME=/home/bellows` so the engine writes to the mounted volume, not /root/.<engine>: {args:?}",
            );
        }
    }

    #[test]
    fn setup_auth_docker_args_mount_credentials_volume_at_engine_home() {
        // Both engines mount their credentials volume at the engine's
        // in-container home. Pin the `--volume <name>:<target>` shape
        // for both engines so the host that ends up with `auth.json`
        // is the host the engine actually reads from.
        for (engine, volume, home) in [
            (bellows::config::Engine::Claude, "v-c", "/home/bellows/.claude"),
            (bellows::config::Engine::Codex,  "v-x", "/home/bellows/.codex"),
        ] {
            let args = setup_auth_docker_args(engine, volume, home, "img:tag");
            assert!(
                args.iter().any(|a| a == &format!("{volume}:{home}")),
                "{engine:?} args must mount `{volume}:{home}`: {args:?}",
            );
        }
    }

    #[test]
    fn setup_auth_preflight_instructions_for_claude_mention_login_slash_command() {
        // Claude path's pre-flight text instructs the operator to type
        // `/login` inside the interactive TUI — that's the Claude Code
        // OAuth idiom. Keep it pinned so the codex-side fix doesn't
        // regress the claude-side messaging.
        let text = setup_auth_preflight_instructions(
            bellows::config::Engine::Claude,
            "bellows-claude-credentials",
        );
        assert!(
            text.contains("/login"),
            "claude pre-flight instructions must mention `/login`: {text}",
        );
    }

    #[test]
    fn setup_auth_preflight_instructions_for_codex_do_not_mention_login_slash_command() {
        // Codex has no `/login` slash command. The pre-flight text MUST
        // NOT tell the operator to type `/login` — that's the exact
        // bug this issue fixes. A future regression that copy-pastes
        // the claude phrasing back into the codex branch flips the
        // test red.
        let text = setup_auth_preflight_instructions(
            bellows::config::Engine::Codex,
            "bellows-codex-credentials",
        );
        assert!(
            !text.contains("/login"),
            "codex pre-flight instructions must NOT mention `/login` (codex has no /login slash command): {text}",
        );
    }

    #[test]
    fn setup_auth_preflight_instructions_for_codex_mention_the_device_auth_flow() {
        // Codex's pre-flight text guides the operator through the
        // device-auth OAuth flow — codex prints a URL + a short code
        // that the operator opens in their host browser and pastes.
        // Pin enough of the wording that an operator scanning the
        // output knows what to do, and that a future regression that
        // strips the guidance fails the test.
        let text = setup_auth_preflight_instructions(
            bellows::config::Engine::Codex,
            "bellows-codex-credentials",
        );
        let lowered = text.to_lowercase();
        assert!(
            lowered.contains("device") && (lowered.contains("url") || lowered.contains("browser")),
            "codex pre-flight instructions must guide the operator through the device-auth flow \
             (mention `device` and a URL / browser step): {text}",
        );
        assert!(
            lowered.contains("code"),
            "codex pre-flight instructions must mention the short code the operator pastes: {text}",
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

    // ---- Issue #35: multi-repo kill target parsing ----

    fn single_repo(url: &str) -> Vec<bellows::config::RepoConfig> {
        // We have no RepoConfig public constructor, so go through
        // Config::from_str to build a normalised vec without
        // duplicating the schema. Same path the real CLI uses.
        let toml = format!(
            "[repo]\nurl = \"{}\"\n[github]\npat_env_var = \"X\"\n",
            url,
        );
        Config::from_str(&toml).unwrap().repos
    }

    fn multi_repo(urls: &[&str]) -> Vec<bellows::config::RepoConfig> {
        let mut toml = String::new();
        for u in urls {
            toml.push_str(&format!("[[repo]]\nurl = \"{}\"\n", u));
        }
        toml.push_str("[github]\npat_env_var = \"X\"\n");
        Config::from_str(&toml).unwrap().repos
    }

    #[test]
    fn cli_parses_kill_with_repo_slash_issue_explicit_form() {
        // Issue #35 acceptance: the explicit `bellows kill
        // <owner>/<name>/<issue>` form must parse through clap. We
        // pin the target string is forwarded verbatim — the
        // host/issue split happens later in resolve_kill_target.
        let cli = Cli::try_parse_from(["bellows", "kill", "marad2001/bellows/42"])
            .expect("bellows kill <owner>/<name>/<issue> must parse");
        match cli.command {
            Some(Command::Kill { target }) => assert_eq!(target, "marad2001/bellows/42"),
            _ => panic!("expected Kill {{ ... }}"),
        }
    }

    #[test]
    fn cli_parses_kill_with_bare_issue_for_back_compat() {
        // Issue #35 backward-compat: `bellows kill 42` must still
        // parse. The runtime check that the bare form is valid only
        // for single-repo configs lives in resolve_kill_target, not
        // in clap.
        let cli = Cli::try_parse_from(["bellows", "kill", "42"])
            .expect("bellows kill <issue> bare form must parse");
        match cli.command {
            Some(Command::Kill { target }) => assert_eq!(target, "42"),
            _ => panic!("expected Kill {{ ... }}"),
        }
    }

    #[test]
    fn resolve_kill_target_accepts_bare_issue_in_single_repo_config() {
        // Back-compat path: `bellows kill 42` with one configured
        // [[repo]] resolves cleanly to that repo's slug + the issue
        // number. Equivalent to the slice-10 behaviour before
        // multi-repo landed.
        let repos = single_repo("https://github.com/marad2001/bellows-test");
        let resolved = resolve_kill_target("42", &repos).unwrap();
        assert_eq!(resolved.issue, 42);
        assert_eq!(resolved.repo_label, "marad2001/bellows-test");
        assert_eq!(
            resolved.repo_url,
            "https://github.com/marad2001/bellows-test",
        );
    }

    #[test]
    fn resolve_kill_target_rejects_bare_issue_with_multiple_repos_configured() {
        // Acceptance: bare form is ambiguous when more than one repo
        // is configured. The kill must refuse with a clear message
        // naming the explicit form so the operator knows what to
        // type instead.
        let repos = multi_repo(&[
            "https://github.com/marad2001/repo-a",
            "https://github.com/marad2001/repo-b",
        ]);
        let err = resolve_kill_target("42", &repos).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("single configured")
                || msg.contains("ambiguous")
                || msg.contains("only valid"),
            "error must explain that bare form is single-repo-only: {msg}",
        );
        assert!(
            msg.contains("<owner>/<name>/42") || msg.contains("/42"),
            "error must suggest the explicit form: {msg}",
        );
    }

    #[test]
    fn resolve_kill_target_accepts_explicit_repo_slash_issue_in_multi_repo_config() {
        let repos = multi_repo(&[
            "https://github.com/marad2001/repo-a",
            "https://github.com/marad2001/repo-b",
        ]);
        let resolved =
            resolve_kill_target("marad2001/repo-b/42", &repos).expect("explicit form must resolve");
        assert_eq!(resolved.issue, 42);
        assert_eq!(resolved.repo_label, "marad2001/repo-b");
        assert_eq!(
            resolved.repo_url,
            "https://github.com/marad2001/repo-b",
        );
    }

    #[test]
    fn resolve_kill_target_tolerates_unrelated_malformed_repo_url_in_config() {
        // A malformed URL in an unrelated `[[repo]]` entry must not
        // pre-empt the explicit-form match for a different entry. The
        // configured-list error path on the not-found branch already
        // tolerates parse failures via `filter_map`; the match loop
        // should match that shape rather than aborting on the first
        // bad URL.
        let repos = multi_repo(&[
            "not-a-url",
            "https://github.com/marad2001/repo-b",
        ]);
        let resolved = resolve_kill_target("marad2001/repo-b/42", &repos)
            .expect("explicit form must still resolve when an unrelated entry has a bad URL");
        assert_eq!(resolved.issue, 42);
        assert_eq!(resolved.repo_label, "marad2001/repo-b");
        assert_eq!(
            resolved.repo_url,
            "https://github.com/marad2001/repo-b",
        );
    }

    #[test]
    fn resolve_kill_target_rejects_explicit_form_when_repo_is_not_configured() {
        // Defence against a typo: `bellows kill some/other/42` against
        // a config that does not list `some/other` must refuse rather
        // than silently doing nothing or guessing at the intended repo.
        let repos = multi_repo(&[
            "https://github.com/marad2001/repo-a",
            "https://github.com/marad2001/repo-b",
        ]);
        let err =
            resolve_kill_target("not-configured/repo-x/42", &repos).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("not-configured/repo-x"),
            "error must echo the rejected qualifier: {msg}",
        );
        assert!(
            msg.contains("Configured"),
            "error must list the configured repos: {msg}",
        );
    }

    #[test]
    fn resolve_kill_target_rejects_non_numeric_issue() {
        let repos = single_repo("https://github.com/marad2001/bellows-test");
        let err = resolve_kill_target("not-a-number", &repos).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not-a-number"), "{msg}");
        assert!(msg.contains("invalid issue number"), "{msg}");
    }

    // ---- Issue #115: triage --repo / --issue filter flags ----

    #[test]
    fn cli_parses_triage_with_repo_flag_for_filtered_drain() {
        // Issue #115 AC1: `--repo <owner/name>` restricts the drain to
        // a single configured `[[repo]]`. Clap must accept the flag as
        // an optional string; the slug-vs-config validation happens
        // later in resolve_triage_filter.
        let cli = Cli::try_parse_from([
            "bellows",
            "triage",
            "--repo",
            "marad2001/bellows",
        ])
        .expect("bellows triage --repo <owner/name> must parse");
        match cli.command {
            Some(Command::Triage {
                issue: None,
                repo: Some(r),
                dry_run: false,
                ..
            }) => {
                assert_eq!(r, "marad2001/bellows");
            }
            _ => panic!("expected Triage with repo=Some(\"marad2001/bellows\")"),
        }
    }
}
