use std::collections::HashMap;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountType};
use bollard::query_parameters::{
    KillContainerOptions, ListContainersOptionsBuilder, LogsOptionsBuilder,
    RemoveContainerOptionsBuilder,
};
use bollard::Docker;
use sha2::{Digest, Sha256};
use tokio_stream::StreamExt;
use uuid::Uuid;

use crate::auth::Auth;
use crate::policy::{CheckResult, GateOutcome};
use crate::workspace::Workspace;

const POLICY_IMAGE_DIR: &str = "policy-image";

/// Name of the single shared cargo registry volume mounted on every
/// agent container. Holds the cargo registry index plus downloaded
/// crate sources; safe to share across all repos because cargo is
/// invoked one container at a time (concurrency=1 in v1).
pub const CARGO_REGISTRY_VOLUME_NAME: &str = "bellows-cargo-registry";

/// Cargo registry path inside the container. Inherited from the
/// `rust:1.95-slim` base image's `CARGO_HOME=/usr/local/cargo` —
/// `policy-image/Dockerfile` doesn't override that. If the base
/// image ever moves CARGO_HOME, this constant follows the image.
const CARGO_REGISTRY_PATH_IN_CONTAINER: &str = "/usr/local/cargo/registry";

const WORKSPACE_TARGET_PATH_IN_CONTAINER: &str = "/workspace/target";

const VOLUME_KIND_TARGET: &str = "target";
const VOLUME_KIND_CARGO_REGISTRY: &str = "cargo-registry";

/// Root-mode prep entrypoint baked into the policy image. Chowns the
/// cache-volume mount points (Docker creates a fresh named volume's
/// _data dir as root:root; bellows uid 1000 needs to write) and then
/// `exec runuser -u bellows -- "$@"`'s whatever was passed. Used as
/// the first element of the cargo-checks entrypoint override so the
/// chown step still runs when we bypass the default ENTRYPOINT.
const POLICY_PREP_ENTRYPOINT: &str = "/usr/local/bin/entrypoint";
const CARGO_CHECKS_USER_SCRIPT: &str = "/usr/local/bin/run-cargo-checks";

/// How many bytes of agent stdout/stderr to retain for the failure log
/// comment. Streaming to the log_writer is unaffected — this is a tee
/// for the post-run summary, not a cap on what's written.
const OUTPUT_TAIL_CAP_BYTES: usize = 64 * 1024;

/// Outcome of a finished agent run. Carries the container exit code so
/// the runner can pass it to `policy::classify_exit`, a tail of the
/// container's stdout/stderr for embedding in failure log comments,
/// and a flag indicating whether the run was killed by the wall-clock
/// deadline rather than exiting on its own.
#[derive(Debug, Clone)]
pub struct AgentRun {
    pub exit_code: i64,
    pub stderr_tail: String,
    pub killed_by_deadline: bool,
}

/// Bounded byte buffer that retains the most-recent N bytes appended.
/// Used to capture an agent's recent output without holding gigabytes
/// of an unbounded run in memory.
struct OutputTail {
    bytes: Vec<u8>,
    cap: usize,
}

impl OutputTail {
    fn new(cap: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(cap),
            cap,
        }
    }

    fn append(&mut self, more: &[u8]) {
        if more.len() >= self.cap {
            let keep_from = more.len() - self.cap;
            self.bytes.clear();
            self.bytes.extend_from_slice(&more[keep_from..]);
            return;
        }
        let total = self.bytes.len() + more.len();
        if total > self.cap {
            self.bytes.drain(..total - self.cap);
        }
        self.bytes.extend_from_slice(more);
    }

    fn into_string(self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("docker: {0}")]
    Bollard(#[from] bollard::errors::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("docker build failed (status {0})")]
    ImageBuildFailed(std::process::ExitStatus),
    #[error("docker images failed (status {0})")]
    ImageQueryFailed(std::process::ExitStatus),
    #[error("policy-image dir not found at {0}")]
    PolicyImageMissing(String),
    #[error(
        "cargo checks gate produced no results file (container exit {exit_code}); the run-cargo-checks script likely crashed before recording exit codes"
    )]
    CargoChecksScriptCrashed { exit_code: i64 },
}

/// Build (or reuse the cached) policy image and return its tag. Used by
/// both `run_agent` and `bellows setup-auth`.
pub async fn ensure_policy_image() -> Result<String, SandboxError> {
    let hash = compute_dir_content_hash(Path::new(POLICY_IMAGE_DIR))?;
    let image_tag = format!("bellows-policy:{}", &hash[..12]);
    ensure_image_built(&hash, &image_tag).await?;
    Ok(image_tag)
}

/// How the lifecycle helper should retain the container's stdout/stderr.
enum CaptureMode {
    /// Keep at most this many bytes of the most-recent output (used for
    /// the agent run's failure-log tail).
    BoundedTail(usize),
    /// Keep the full output (used for the cargo-test gate so the
    /// failure log comment can show every failing assertion).
    Full,
}

enum Captured {
    Bounded(OutputTail),
    Full(Vec<u8>),
}

impl Captured {
    fn new(mode: CaptureMode) -> Self {
        match mode {
            CaptureMode::BoundedTail(cap) => Captured::Bounded(OutputTail::new(cap)),
            CaptureMode::Full => Captured::Full(Vec::new()),
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        match self {
            Captured::Bounded(tail) => tail.append(bytes),
            Captured::Full(buf) => buf.extend_from_slice(bytes),
        }
    }

    fn into_string(self) -> String {
        match self {
            Captured::Bounded(tail) => tail.into_string(),
            Captured::Full(buf) => String::from_utf8_lossy(&buf).into_owned(),
        }
    }
}

struct ContainerOutcome {
    exit_code: i64,
    captured: String,
    /// True when the lifecycle was terminated by the deadline firing
    /// rather than by the container exiting on its own. The runner uses
    /// this to set `PhaseOutcomes::wall_clock_exceeded` and short-
    /// circuit the rest of the pipeline.
    killed_by_deadline: bool,
}

/// Run a container through its full lifecycle: create, start, stream
/// stdout/stderr to `log_writer` while capturing per `capture_mode`,
/// wait for exit, force-remove. Container is removed even on error.
///
/// Non-zero container exit is returned as `exit_code`, NOT as a sandbox
/// error — the caller (run_agent / run_cargo_checks) and ultimately
/// policy::classify_exit decide what a non-zero exit means.
///
/// `deadline` is the wall-clock budget for THIS container run. When
/// `Some` and the deadline fires before the container exits, the
/// container is killed (SIGKILL) and `killed_by_deadline` is set. When
/// `None`, the container runs to natural completion regardless of
/// elapsed time.
async fn run_container(
    docker: &Docker,
    config: ContainerCreateBody,
    log_writer: &mut dyn Write,
    capture_mode: CaptureMode,
    deadline: Option<Duration>,
) -> Result<ContainerOutcome, SandboxError> {
    let container = docker.create_container(None, config).await?;
    let id = container.id;

    // Once the container exists on the daemon it must be removed even if
    // start/log/wait fail. Run the lifecycle inside an inner async block
    // and force-remove unconditionally afterwards.
    let lifecycle: Result<ContainerOutcome, SandboxError> = async {
        docker.start_container(&id, None).await?;

        // Box the deadline future so we can race it against the log
        // stream in tokio::select! while keeping a single sleep for
        // the whole container lifetime (not re-armed each iteration).
        // When deadline is None, fall back to a never-completing future
        // so the deadline branch effectively never wins.
        let mut deadline_future: Pin<Box<dyn Future<Output = ()> + Send>> = match deadline {
            Some(d) => Box::pin(tokio::time::sleep(d)),
            None => Box::pin(std::future::pending()),
        };

        let log_options = LogsOptionsBuilder::default()
            .follow(true)
            .stdout(true)
            .stderr(true)
            .build();
        let mut log_stream = docker.logs(&id, Some(log_options));
        let mut captured = Captured::new(capture_mode);
        let mut killed_by_deadline = false;

        loop {
            tokio::select! {
                maybe_frame = log_stream.next() => {
                    match maybe_frame {
                        None => break, // log stream ended (container exited)
                        Some(frame) => {
                            let frame = frame?;
                            let bytes = match frame {
                                bollard::container::LogOutput::StdOut { message } => message,
                                bollard::container::LogOutput::StdErr { message } => message,
                                _ => continue,
                            };
                            log_writer.write_all(&bytes)?;
                            log_writer.flush()?;
                            captured.append(&bytes);
                        }
                    }
                }
                _ = &mut deadline_future => {
                    // Deadline fired — SIGKILL the container. wait_container
                    // below will pick up the kill exit code (typically 137).
                    let _ = docker
                        .kill_container(&id, None::<KillContainerOptions>)
                        .await;
                    killed_by_deadline = true;
                    break;
                }
            }
        }

        let mut wait_stream = docker.wait_container(&id, None);
        let mut exit_code = 0i64;
        while let Some(response) = wait_stream.next().await {
            match response {
                Ok(r) => exit_code = r.status_code,
                // Bollard converts a non-zero container exit into this
                // error variant. For Bellows the exit code is data
                // (policy::classify_exit routes on it), not a failure
                // condition — un-wrap the variant back into a normal
                // i64 here. Other bollard errors still propagate.
                Err(bollard::errors::Error::DockerContainerWaitError { code, .. }) => {
                    exit_code = code;
                }
                Err(other) => return Err(other.into()),
            }
        }

        Ok(ContainerOutcome {
            exit_code,
            captured: captured.into_string(),
            killed_by_deadline,
        })
    }
    .await;

    let remove_options = RemoveContainerOptionsBuilder::default().force(true).build();
    let _ = docker.remove_container(&id, Some(remove_options)).await;

    lifecycle
}

pub async fn run_agent(
    workspace: &Workspace,
    auth: &Auth,
    issue_number: u64,
    repo_slug: &str,
    log_writer: &mut dyn Write,
    deadline: Option<Duration>,
) -> Result<AgentRun, SandboxError> {
    let image_tag = ensure_policy_image().await?;

    let docker = Docker::connect_with_local_defaults()?;
    let run_id = Uuid::new_v4().to_string();

    // tempfile gives an absolute path already; canonicalize() on Windows
    // returns `\\?\C:\...` extended-length paths that Docker Desktop's
    // bind-mount handler rejects, so we use the path as-is.
    let workspace_path = workspace.path().to_string_lossy().to_string();

    let labels = build_managed_labels(&run_id, issue_number, None);

    let mut env = vec![format!("BELLOWS_ISSUE_NUMBER={issue_number}")];
    env.extend(auth.extra_env());

    // Structured Mount API rather than `binds: Vec<String>` to avoid
    // collision with bind syntax's `:` separator on Windows drive
    // letters. Auth contributes credentials volumes; build_cache_mounts
    // contributes the per-repo target + shared cargo registry caches.
    let mut mounts = vec![Mount {
        target: Some("/workspace".to_string()),
        source: Some(workspace_path),
        typ: Some(MountType::BIND),
        ..Default::default()
    }];
    mounts.extend(auth.extra_mounts());
    mounts.extend(build_cache_mounts(repo_slug));

    let host_config = HostConfig {
        mounts: Some(mounts),
        ..Default::default()
    };

    let config = ContainerCreateBody {
        image: Some(image_tag),
        env: Some(env),
        labels: Some(labels),
        host_config: Some(host_config),
        ..Default::default()
    };

    let outcome = run_container(
        &docker,
        config,
        log_writer,
        CaptureMode::BoundedTail(OUTPUT_TAIL_CAP_BYTES),
        deadline,
    )
    .await?;

    Ok(AgentRun {
        exit_code: outcome.exit_code,
        stderr_tail: outcome.captured,
        killed_by_deadline: outcome.killed_by_deadline,
    })
}

/// Workspace-side files written by the policy image's `run-cargo-checks`
/// script. The runner reads these after the container exits so it can
/// attribute clippy / test failures separately, then removes them so
/// they don't end up in any subsequent commit.
const CARGO_CLIPPY_OUTPUT_FILE: &str = ".bellows-cargo-clippy-output";
const CARGO_TEST_OUTPUT_FILE: &str = ".bellows-cargo-test-output";
const CARGO_CHECKS_RESULTS_FILE: &str = ".bellows-cargo-checks-results";

/// Result of running the cargo checks gate, carrying both the
/// per-check `GateOutcome` (clippy + test exit codes & captured output)
/// and the wall-clock kill flag the runner needs to set
/// `PhaseOutcomes::wall_clock_exceeded`.
pub struct CargoChecksRun {
    pub gate: GateOutcome,
    pub killed_by_deadline: bool,
}

/// Spawn a fresh container from the policy image and run the cargo
/// checks gate: `cargo clippy --all-targets --all-features -- -D
/// warnings` followed by `cargo test --all-targets --all-features`.
/// Both run inside the same container (entrypoint overridden to
/// `run-cargo-checks`) so clippy's compilation artifacts are reused
/// by test. The flag set matches the GitHub Actions CI workflow so
/// the two verdicts agree by construction.
///
/// Returns a `CargoChecksRun` carrying each check's exit code + captured
/// output (in `gate`) plus a `killed_by_deadline` flag. `cargo_test` in
/// the gate is `None` when clippy failed and the gate short-circuited
/// before running tests. Either being `None` and the other being `Some`
/// with a non-zero exit signals the gate failed.
///
/// `deadline` is the budget for THIS gate run. When `Some` and the
/// deadline fires, the container is killed and `killed_by_deadline` is
/// set on the returned `CargoChecksRun`.
///
/// No credentials volume — the gate has no Anthropic dependency.
pub async fn run_cargo_checks(
    workspace: &Workspace,
    issue_number: u64,
    repo_slug: &str,
    log_writer: &mut dyn Write,
    deadline: Option<Duration>,
) -> Result<CargoChecksRun, SandboxError> {
    let image_tag = ensure_policy_image().await?;

    let docker = Docker::connect_with_local_defaults()?;
    let run_id = Uuid::new_v4().to_string();

    let workspace_path = workspace.path().to_string_lossy().to_string();

    let labels = build_managed_labels(&run_id, issue_number, Some("cargo-checks-gate"));

    let mut mounts = vec![Mount {
        target: Some("/workspace".to_string()),
        source: Some(workspace_path),
        typ: Some(MountType::BIND),
        ..Default::default()
    }];
    mounts.extend(build_cache_mounts(repo_slug));

    let host_config = HostConfig {
        mounts: Some(mounts),
        ..Default::default()
    };

    // Route through the policy image's root-mode entrypoint so the
    // cache-volume mount points get chowned to bellows before
    // run-cargo-checks runs as bellows. Skipping the prep here would
    // re-introduce the EACCES-on-first-write regression that
    // `/workspace/target` and `/usr/local/cargo/registry` are exposed
    // to whenever Docker freshly creates one of those named volumes.
    let config = ContainerCreateBody {
        image: Some(image_tag),
        entrypoint: Some(build_cargo_checks_entrypoint()),
        cmd: Some(vec![]),
        working_dir: Some("/workspace".to_string()),
        labels: Some(labels),
        host_config: Some(host_config),
        ..Default::default()
    };

    // Container exit is normally redundant (per-check codes are in the
    // results file) — but if the script crashed BEFORE writing results,
    // a missing/empty file would otherwise classify as "(None, None)" =
    // non-Rust workspace = Success. Use the container exit as a tripwire
    // for that scenario: non-zero container exit + no usable results
    // file ⇒ raise CargoChecksScriptCrashed instead of silently passing.
    let outcome = run_container(&docker, config, log_writer, CaptureMode::Full, deadline).await?;

    let workspace_path = workspace.path();
    let clippy_output = read_and_remove(workspace_path.join(CARGO_CLIPPY_OUTPUT_FILE))
        .await?
        .unwrap_or_default();
    let test_output = read_and_remove(workspace_path.join(CARGO_TEST_OUTPUT_FILE))
        .await?
        .unwrap_or_default();
    let results_text = read_and_remove(workspace_path.join(CARGO_CHECKS_RESULTS_FILE)).await?;

    let (clippy_exit, test_exit) = match results_text.as_deref() {
        Some(text) => parse_checks_results(text),
        None => (None, None),
    };

    // Wall-clock kill is a legitimate "no results file" path — the script
    // never ran to completion. Don't conflate it with the script-crashed
    // tripwire (which signals "container exited non-zero AND no results").
    if !outcome.killed_by_deadline
        && outcome.exit_code != 0
        && clippy_exit.is_none()
        && test_exit.is_none()
    {
        return Err(SandboxError::CargoChecksScriptCrashed {
            exit_code: outcome.exit_code,
        });
    }

    Ok(CargoChecksRun {
        gate: GateOutcome {
            cargo_clippy: clippy_exit.map(|exit_code| CheckResult {
                exit_code,
                output: clippy_output,
            }),
            cargo_test: test_exit.map(|exit_code| CheckResult {
                exit_code,
                output: test_output,
            }),
        },
        killed_by_deadline: outcome.killed_by_deadline,
    })
}

/// Read a file at `path`, remove it, and return its contents. Returns
/// `Ok(None)` if the file doesn't exist (treated by the caller as
/// "the corresponding check did not produce output").
async fn read_and_remove(path: PathBuf) -> Result<Option<String>, SandboxError> {
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => {
            let _ = tokio::fs::remove_file(&path).await;
            Ok(Some(content))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SandboxError::Io(e)),
    }
}

/// Parse the tiny `clippy_exit=N` / `test_exit=N` results file written
/// by `run-cargo-checks`. Empty `test_exit=` value means the test step
/// did not run (clippy short-circuited it). Missing or malformed lines
/// return `None` for that field — the runner treats `None` as "check
/// did not run."
fn parse_checks_results(text: &str) -> (Option<i64>, Option<i64>) {
    let mut clippy = None;
    let mut test = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("clippy_exit=") {
            clippy = rest.trim().parse::<i64>().ok();
        } else if let Some(rest) = line.strip_prefix("test_exit=") {
            test = rest.trim().parse::<i64>().ok();
        }
    }
    (clippy, test)
}

fn compute_dir_content_hash(dir: &Path) -> Result<String, SandboxError> {
    if !dir.exists() {
        return Err(SandboxError::PolicyImageMissing(
            dir.display().to_string(),
        ));
    }

    let mut files: Vec<PathBuf> = Vec::new();
    walk_recursively(dir, &mut files)?;
    files.sort();

    let mut hasher = Sha256::new();
    for path in &files {
        let rel = path
            .strip_prefix(dir)
            .expect("walked path is always under dir");
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        let content = std::fs::read(path)?;
        hasher.update(&content);
        hasher.update(b"\0");
    }
    let digest = hasher.finalize();
    Ok(digest.iter().map(|b| format!("{:02x}", b)).collect())
}

fn walk_recursively(dir: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_recursively(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

async fn ensure_image_built(hash: &str, tag: &str) -> Result<(), SandboxError> {
    let output = tokio::process::Command::new("docker")
        .args(["images", "--quiet", tag])
        .output()
        .await?;
    if !output.status.success() {
        return Err(SandboxError::ImageQueryFailed(output.status));
    }
    if !output.stdout.is_empty() {
        return Ok(());
    }

    let status = tokio::process::Command::new("docker")
        .args([
            "build",
            "--tag",
            tag,
            "--label",
            &format!("bellows-policy-hash={hash}"),
            POLICY_IMAGE_DIR,
        ])
        .status()
        .await?;
    if !status.success() {
        return Err(SandboxError::ImageBuildFailed(status));
    }
    Ok(())
}

/// Build the two cache-volume mounts every agent container carries:
/// a per-repo `target/` volume and the shared cargo registry volume.
///
/// Docker stamps the `volume_options` labels onto the volume at
/// first-create only — existing volumes are not retroactively
/// re-labelled. Acceptable because the very first run on a repo
/// creates the volume, and `bellows prune` (issue #13) only needs
/// to find volumes that bellows itself created.
fn build_cache_mounts(repo_slug: &str) -> Vec<Mount> {
    let target_labels = HashMap::from([
        ("bellows-managed".to_string(), "true".to_string()),
        ("bellows-volume-kind".to_string(), VOLUME_KIND_TARGET.to_string()),
        ("bellows-repo-slug".to_string(), repo_slug.to_string()),
    ]);
    let registry_labels = HashMap::from([
        ("bellows-managed".to_string(), "true".to_string()),
        (
            "bellows-volume-kind".to_string(),
            VOLUME_KIND_CARGO_REGISTRY.to_string(),
        ),
    ]);

    vec![
        labelled_volume_mount(
            WORKSPACE_TARGET_PATH_IN_CONTAINER,
            &crate::target_volume_name_from_slug(repo_slug),
            target_labels,
        ),
        labelled_volume_mount(
            CARGO_REGISTRY_PATH_IN_CONTAINER,
            CARGO_REGISTRY_VOLUME_NAME,
            registry_labels,
        ),
    ]
}

/// The entrypoint override applied to the cargo-checks container.
/// Front-loaded with the root-mode prep so the cache-volume mount
/// points get chowned to bellows before `run-cargo-checks` runs.
fn build_cargo_checks_entrypoint() -> Vec<String> {
    vec![
        POLICY_PREP_ENTRYPOINT.to_string(),
        CARGO_CHECKS_USER_SCRIPT.to_string(),
    ]
}

fn labelled_volume_mount(target: &str, source: &str, labels: HashMap<String, String>) -> Mount {
    Mount {
        target: Some(target.to_string()),
        source: Some(source.to_string()),
        typ: Some(MountType::VOLUME),
        volume_options: Some(bollard::models::MountVolumeOptions {
            labels: Some(labels),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build the label set every Bellows-managed container carries. Pulled
/// out of the inline body in `run_agent` / `run_cargo_checks` so the
/// label shape is unit-testable without spinning up Docker.
///
/// Always sets `bellows-managed=true`, `bellows-run-id=<run_id>`, and
/// `bellows-issue-number=<issue_number>`. Optionally sets
/// `bellows-purpose=<purpose>` when `purpose` is `Some` (the
/// cargo-checks-gate uses this to distinguish itself from the agent
/// run; `bellows kill <N>` uses `bellows-issue-number` to find either
/// kind via a server-side label filter).
fn build_managed_labels(
    run_id: &str,
    issue_number: u64,
    purpose: Option<&str>,
) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    labels.insert("bellows-managed".to_string(), "true".to_string());
    labels.insert("bellows-run-id".to_string(), run_id.to_string());
    labels.insert(
        "bellows-issue-number".to_string(),
        issue_number.to_string(),
    );
    if let Some(p) = purpose {
        labels.insert("bellows-purpose".to_string(), p.to_string());
    }
    labels
}

/// Build the bollard list-containers label filter for finding the
/// container associated with a specific issue. Used by
/// `find_containers_for_issue` to locate the running agent or
/// cargo-checks container so `bellows kill <N>` can force-remove it.
/// Pulled out as a pure function so the filter shape is unit-testable
/// without docker.
fn build_issue_container_filter(issue_number: u64) -> HashMap<String, Vec<String>> {
    let mut filters: HashMap<String, Vec<String>> = HashMap::new();
    filters.insert(
        "label".to_string(),
        vec![
            "bellows-managed=true".to_string(),
            format!("bellows-issue-number={}", issue_number),
        ],
    );
    filters
}

/// Find every container associated with a specific issue. Used by
/// `bellows kill <N>` to locate the live agent or cargo-checks
/// container(s) before force-removing them. Returns ALL matching
/// container IDs (suitable for passing to `kill_container`).
///
/// Multiple containers can legitimately match `bellows-issue-number=<N>`
/// at the same time: if a prior phase's lifecycle-end force-remove
/// failed transiently, the stopped corpse remains AND the next
/// phase's container (running) shares the same `bellows-issue-number`
/// label. Keeping only the first match (the old behaviour) could
/// remove the corpse while leaving the live container running —
/// exactly the failure mode the kill is supposed to prevent. So this
/// function returns every match and the caller removes each.
///
/// Uses a server-side label filter (`bellows-managed=true` +
/// `bellows-issue-number=<N>`) so the daemon does the matching,
/// mirroring the slice-7 orphan-cleanup pattern.
pub async fn find_containers_for_issue(
    docker: &Docker,
    issue_number: u64,
) -> Result<Vec<String>, SandboxError> {
    let filters = build_issue_container_filter(issue_number);
    let options = ListContainersOptionsBuilder::default()
        .all(true)
        .filters(&filters)
        .build();
    let containers = docker.list_containers(Some(options)).await?;
    Ok(containers.into_iter().filter_map(|c| c.id).collect())
}

/// Force-remove a container by ID. Used by `bellows kill <N>` after
/// `find_containers_for_issue` locates the target. Removes via bollard
/// with `force=true` (SIGKILL semantics) — slice 10 is intentionally
/// blunt; a graceful SIGTERM-then-SIGKILL phase is a future enhancement.
pub async fn kill_container(docker: &Docker, id: &str) -> Result<(), SandboxError> {
    let options = RemoveContainerOptionsBuilder::default().force(true).build();
    docker.remove_container(id, Some(options)).await?;
    Ok(())
}

/// One leftover container Bellows is cleaning up at startup. Holds just
/// the fields surfaced in the per-orphan log line; the full bollard
/// summary isn't propagated past `cleanup_orphan_containers`.
struct OrphanInfo {
    short_id: String,
    run_id: Option<String>,
    purpose: Option<String>,
}

/// Format a per-orphan log line. Pure function so the line shape is
/// unit-testable without docker. Includes the short-id always, and the
/// run-id / purpose only when present (not all bellows containers carry
/// purpose — e.g. the agent run doesn't).
fn format_orphan_log_line(info: &OrphanInfo) -> String {
    let mut line = format!("bellows: cleaned up orphan container {}", info.short_id);
    if let Some(rid) = &info.run_id {
        line.push_str(&format!(" (run-id: {rid})"));
    }
    if let Some(p) = &info.purpose {
        line.push_str(&format!(" (purpose: {p})"));
    }
    line
}

/// Extract bellows label fields from a bollard container's labels map.
/// Pure transformation so the extraction is unit-testable without docker.
/// `id` is the full 64-char container ID; the function shortens it to
/// the docker-conventional 12 chars for human-readable logs.
fn orphan_info_from_labels(id: &str, labels: &HashMap<String, String>) -> OrphanInfo {
    OrphanInfo {
        short_id: id.chars().take(12).collect(),
        run_id: labels.get("bellows-run-id").cloned(),
        purpose: labels.get("bellows-purpose").cloned(),
    }
}

/// Force-remove every container carrying the `bellows-managed=true`
/// label. Called once at `bellows run` startup, before the polling loop.
/// Containers that completed normally were already removed by their
/// own lifecycle (see `run_container`'s drop path); anything still
/// present is by definition an orphan from a prior bellows process
/// that didn't shut down cleanly (Ctrl-C, SIGKILL, panic, machine
/// sleep).
///
/// Single-instance assumption: this rule assumes only one `bellows
/// run` process exists at a time. Running two instances in parallel
/// would clobber the other's running containers. Acceptable for v1;
/// `bellows-process-id` labeling is a future enhancement for
/// multi-instance support.
///
/// GitHub state is NOT touched. Issues that were `agent-in-progress`
/// when the prior bellows died stay there until the operator
/// manually re-labels them — auto-reclaim could replay a partially-
/// completed run on stale workspace state.
///
/// Returns one already-formatted log line per successfully-removed
/// orphan so the caller (main.rs) can route them through its own
/// `log()` helper that fans out to both stdout and the log file —
/// the operator running bellows interactively wants to see *which*
/// container was cleaned up at a glance, not just a count.
///
/// Per-removal failures are logged to `log_writer` directly (file-
/// only path) and do NOT stop the function attempting the rest;
/// they're absent from the returned Vec.
pub async fn cleanup_orphan_containers(
    docker: &Docker,
    log_writer: &mut dyn Write,
) -> Result<Vec<String>, SandboxError> {
    let mut filters: HashMap<String, Vec<String>> = HashMap::new();
    filters.insert(
        "label".to_string(),
        vec!["bellows-managed=true".to_string()],
    );
    let options = ListContainersOptionsBuilder::default()
        .all(true) // include stopped containers as well as running
        .filters(&filters)
        .build();

    let containers = docker.list_containers(Some(options)).await?;
    let remove_options = RemoveContainerOptionsBuilder::default().force(true).build();

    let mut success_lines = Vec::new();
    for c in containers {
        let Some(id) = c.id else {
            continue;
        };
        let info = orphan_info_from_labels(&id, &c.labels.unwrap_or_default());

        match docker
            .remove_container(&id, Some(remove_options.clone()))
            .await
        {
            Ok(()) => {
                success_lines.push(format_orphan_log_line(&info));
            }
            Err(e) => {
                let _ = writeln!(
                    log_writer,
                    "bellows: failed to remove orphan container {} ({e})",
                    info.short_id,
                );
            }
        }
    }
    Ok(success_lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn format_orphan_log_line_contains_short_id_and_orphan_word() {
        // Tracer bullet for slice 7. The line a human reads to know
        // bellows cleaned up a leftover container from a prior process.
        // Must surface the short-id and use the word "orphan" so the
        // line is greppable.
        let info = OrphanInfo {
            short_id: "abc123def456".to_string(),
            run_id: None,
            purpose: None,
        };
        let line = format_orphan_log_line(&info);
        assert!(line.contains("abc123def456"), "missing short-id: {line}");
        assert!(line.to_lowercase().contains("orphan"), "missing 'orphan': {line}");
    }

    #[test]
    fn format_orphan_log_line_includes_run_id_and_purpose_when_present() {
        // For a cargo-checks-gate orphan we have both run-id (uuid)
        // and purpose ("cargo-checks-gate"). The log line should let an
        // operator tell at a glance which kind of phase the orphan was.
        let info = OrphanInfo {
            short_id: "deadbeefcafe".to_string(),
            run_id: Some("11111111-2222-3333-4444-555555555555".to_string()),
            purpose: Some("cargo-checks-gate".to_string()),
        };
        let line = format_orphan_log_line(&info);
        assert!(line.contains("deadbeefcafe"));
        assert!(
            line.contains("11111111-2222-3333-4444-555555555555"),
            "missing run-id: {line}",
        );
        assert!(line.contains("cargo-checks-gate"), "missing purpose: {line}");
    }

    #[test]
    fn orphan_info_from_labels_shortens_id_and_extracts_known_labels() {
        // The agent-run container has bellows-managed + bellows-run-id
        // but NO bellows-purpose. The cargo-checks-gate has all three.
        // Either way, orphan_info_from_labels should pluck what's there
        // and shorten the 64-char container id to docker's conventional
        // 12 chars.
        let full_id = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let mut labels = HashMap::new();
        labels.insert("bellows-managed".to_string(), "true".to_string());
        labels.insert(
            "bellows-run-id".to_string(),
            "deadbeef-1234-5678-9abc-def012345678".to_string(),
        );
        labels.insert("unrelated-other-label".to_string(), "ignored".to_string());

        let info = orphan_info_from_labels(full_id, &labels);
        assert_eq!(info.short_id, "abcdef012345"); // first 12 chars
        assert_eq!(
            info.run_id.as_deref(),
            Some("deadbeef-1234-5678-9abc-def012345678"),
        );
        assert_eq!(info.purpose, None); // bellows-purpose not present
    }

    #[test]
    fn hash_changes_when_file_contents_change() {
        let a = TempDir::new().unwrap();
        std::fs::write(a.path().join("f"), "alpha").unwrap();
        let h_a = compute_dir_content_hash(a.path()).unwrap();

        let b = TempDir::new().unwrap();
        std::fs::write(b.path().join("f"), "beta").unwrap();
        let h_b = compute_dir_content_hash(b.path()).unwrap();

        assert_ne!(h_a, h_b);
    }

    #[test]
    fn hash_is_stable_across_calls_with_identical_contents() {
        let a = TempDir::new().unwrap();
        std::fs::write(a.path().join("f"), "x").unwrap();
        std::fs::write(a.path().join("g"), "y").unwrap();
        let h_a = compute_dir_content_hash(a.path()).unwrap();

        let b = TempDir::new().unwrap();
        std::fs::write(b.path().join("f"), "x").unwrap();
        std::fs::write(b.path().join("g"), "y").unwrap();
        let h_b = compute_dir_content_hash(b.path()).unwrap();

        assert_eq!(h_a, h_b);
    }

    #[test]
    fn hash_differs_when_filenames_differ() {
        let a = TempDir::new().unwrap();
        std::fs::write(a.path().join("foo"), "x").unwrap();
        let h_a = compute_dir_content_hash(a.path()).unwrap();

        let b = TempDir::new().unwrap();
        std::fs::write(b.path().join("bar"), "x").unwrap();
        let h_b = compute_dir_content_hash(b.path()).unwrap();

        assert_ne!(h_a, h_b);
    }

    #[test]
    fn hash_errors_when_directory_does_not_exist() {
        let nope = std::path::Path::new("does-not-exist-bellows-test");
        let err = compute_dir_content_hash(nope).unwrap_err();
        assert!(matches!(err, SandboxError::PolicyImageMissing(_)));
    }

    #[test]
    fn output_tail_keeps_last_n_bytes_when_exceeded() {
        let mut tail = OutputTail::new(8);
        tail.append(b"abcdef");
        tail.append(b"ghij"); // total 10 bytes appended; cap is 8
        assert_eq!(tail.into_string(), "cdefghij");
    }

    #[test]
    fn output_tail_handles_single_chunk_larger_than_cap() {
        let mut tail = OutputTail::new(4);
        tail.append(b"oneverybigchunk");
        assert_eq!(tail.into_string(), "hunk");
    }

    #[test]
    fn output_tail_under_cap_keeps_everything() {
        let mut tail = OutputTail::new(64);
        tail.append(b"hello ");
        tail.append(b"world");
        assert_eq!(tail.into_string(), "hello world");
    }

    #[test]
    fn parse_checks_results_reads_both_exits() {
        let (clippy, test) = parse_checks_results("clippy_exit=0\ntest_exit=0\n");
        assert_eq!(clippy, Some(0));
        assert_eq!(test, Some(0));
    }

    #[test]
    fn parse_checks_results_reads_clippy_failed_test_skipped() {
        // Empty test_exit value means the test step did not run because
        // clippy short-circuited the gate. The script's wrapper writes
        // `test_exit=` (no value) in that case.
        let (clippy, test) = parse_checks_results("clippy_exit=101\ntest_exit=\n");
        assert_eq!(clippy, Some(101));
        assert_eq!(test, None);
    }

    #[test]
    fn parse_checks_results_reads_test_failed() {
        let (clippy, test) = parse_checks_results("clippy_exit=0\ntest_exit=101\n");
        assert_eq!(clippy, Some(0));
        assert_eq!(test, Some(101));
    }

    #[test]
    fn parse_checks_results_returns_none_for_missing_lines() {
        let (clippy, test) = parse_checks_results("");
        assert!(clippy.is_none());
        assert!(test.is_none());
    }

    #[test]
    fn build_managed_labels_for_agent_run_includes_issue_number_and_omits_purpose() {
        // Slice 10 contract: every container Bellows spawns must carry
        // `bellows-issue-number=<N>` so `bellows kill <N>` can find it
        // via a server-side label filter. The agent run carries no
        // `bellows-purpose`; the cargo-checks-gate does.
        let labels = build_managed_labels("run-uuid-here", 42, None);
        assert_eq!(labels.get("bellows-managed").map(String::as_str), Some("true"));
        assert_eq!(
            labels.get("bellows-run-id").map(String::as_str),
            Some("run-uuid-here"),
        );
        assert_eq!(
            labels.get("bellows-issue-number").map(String::as_str),
            Some("42"),
            "agent run must carry bellows-issue-number for `bellows kill <N>`",
        );
        assert!(
            !labels.contains_key("bellows-purpose"),
            "agent run does not carry bellows-purpose",
        );
    }

    #[test]
    fn build_managed_labels_for_cargo_checks_includes_purpose() {
        let labels =
            build_managed_labels("run-uuid", 42, Some("cargo-checks-gate"));
        assert_eq!(
            labels.get("bellows-issue-number").map(String::as_str),
            Some("42"),
        );
        assert_eq!(
            labels.get("bellows-purpose").map(String::as_str),
            Some("cargo-checks-gate"),
        );
    }

    #[test]
    fn build_cache_mounts_produces_target_and_registry_volumes() {
        // Slice 4 acceptance: every agent container is spawned with
        // two named-volume mounts, one per-repo (target/) and one
        // shared (cargo registry). Pin both shapes in one assertion
        // so the helper can't silently drop a mount or swap them.
        let mounts = build_cache_mounts("marad2001-bellows");
        assert_eq!(mounts.len(), 2, "expected target + registry: {:?}", mounts);

        let target = mounts
            .iter()
            .find(|m| m.target.as_deref() == Some(WORKSPACE_TARGET_PATH_IN_CONTAINER))
            .expect("target mount missing");
        assert_eq!(target.typ, Some(MountType::VOLUME));
        assert_eq!(
            target.source.as_deref(),
            Some("bellows-target-marad2001-bellows"),
        );

        let registry = mounts
            .iter()
            .find(|m| m.target.as_deref() == Some(CARGO_REGISTRY_PATH_IN_CONTAINER))
            .expect("registry mount missing");
        assert_eq!(registry.typ, Some(MountType::VOLUME));
        assert_eq!(registry.source.as_deref(), Some(CARGO_REGISTRY_VOLUME_NAME));
    }

    #[test]
    fn build_cache_mounts_target_volume_carries_managed_kind_and_repo_slug_labels() {
        // Slice 4 acceptance: per-repo target volume labels are the
        // discovery key for `bellows prune` (issue #13). The brief
        // pins three label keys: bellows-managed=true,
        // bellows-volume-kind=target, bellows-repo-slug=<slug>.
        let mounts = build_cache_mounts("marad2001-bellows");
        let target = mounts
            .iter()
            .find(|m| m.target.as_deref() == Some(WORKSPACE_TARGET_PATH_IN_CONTAINER))
            .expect("target mount missing");
        let labels = target
            .volume_options
            .as_ref()
            .and_then(|v| v.labels.as_ref())
            .expect("target mount must carry volume_options.labels");
        assert_eq!(labels.get("bellows-managed").map(String::as_str), Some("true"));
        assert_eq!(
            labels.get("bellows-volume-kind").map(String::as_str),
            Some(VOLUME_KIND_TARGET),
        );
        assert_eq!(
            labels.get("bellows-repo-slug").map(String::as_str),
            Some("marad2001-bellows"),
        );
    }

    #[test]
    fn build_cache_mounts_registry_volume_carries_managed_and_kind_labels_but_no_repo_slug() {
        // Slice 4 acceptance: the shared cargo registry is not
        // per-repo — labelling it with a single repo's slug would
        // mis-direct `bellows prune` into removing it whenever that
        // one repo's per-repo volumes are pruned. The registry
        // carries only bellows-managed + bellows-volume-kind.
        let mounts = build_cache_mounts("marad2001-bellows");
        let registry = mounts
            .iter()
            .find(|m| m.target.as_deref() == Some(CARGO_REGISTRY_PATH_IN_CONTAINER))
            .expect("registry mount missing");
        let labels = registry
            .volume_options
            .as_ref()
            .and_then(|v| v.labels.as_ref())
            .expect("registry mount must carry volume_options.labels");
        assert_eq!(labels.get("bellows-managed").map(String::as_str), Some("true"));
        assert_eq!(
            labels.get("bellows-volume-kind").map(String::as_str),
            Some(VOLUME_KIND_CARGO_REGISTRY),
        );
        assert!(
            !labels.contains_key("bellows-repo-slug"),
            "shared registry must not carry bellows-repo-slug: {:?}",
            labels,
        );
    }

    #[test]
    fn build_cargo_checks_entrypoint_runs_prep_then_user_script() {
        // The cargo-checks gate overrides the policy image's default
        // ENTRYPOINT, so without explicitly re-applying the root-mode
        // prep here the chown step would be skipped — and the bellows
        // user would hit EACCES on the first cargo write into either
        // cache volume. Pin: prep is element 0, user script is element 1,
        // both are absolute paths into /usr/local/bin/ (where the policy
        // image actually installs them).
        let entrypoint = build_cargo_checks_entrypoint();
        assert_eq!(
            entrypoint.len(),
            2,
            "expected [prep, user-script]: {:?}",
            entrypoint,
        );
        assert_eq!(
            entrypoint[0], "/usr/local/bin/entrypoint",
            "prep entrypoint must come first so chown runs before the user script",
        );
        assert_eq!(
            entrypoint[1], "/usr/local/bin/run-cargo-checks",
            "second arg must be the cargo-checks user script",
        );
    }

    #[test]
    fn build_issue_container_filter_uses_managed_and_issue_number() {
        // Used by find_containers_for_issue. The filter must restrict to
        // bellows-managed containers AND scope to the requested issue
        // number — otherwise a kill could hit the wrong run.
        let filter = build_issue_container_filter(42);
        let label_values = filter.get("label").expect("label key required");
        assert!(
            label_values.iter().any(|v| v == "bellows-managed=true"),
            "filter must include bellows-managed=true: {:?}",
            label_values,
        );
        assert!(
            label_values.iter().any(|v| v == "bellows-issue-number=42"),
            "filter must include bellows-issue-number=N: {:?}",
            label_values,
        );
    }
}
