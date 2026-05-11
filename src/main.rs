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
            credentials_volume_name: config.auth.credentials_volume.clone(),
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
        let mut found: Option<(String, String)> = None;
        for r in repos {
            let (owner, repo) = runner::parse_owner_repo(&r.url).map_err(|e| anyhow!("{e}"))?;
            let slug = format!("{}/{}", owner, repo);
            if slug == qualifier {
                found = Some((slug, r.url.clone()));
                break;
            }
        }
        let (repo_label, repo_url) = found.ok_or_else(|| {
            let configured: Vec<String> = repos
                .iter()
                .filter_map(|r| {
                    runner::parse_owner_repo(&r.url)
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
        let (owner, repo) = runner::parse_owner_repo(&r.url).map_err(|e| anyhow!("{e}"))?;
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
}
