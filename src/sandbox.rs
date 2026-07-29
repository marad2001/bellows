use crate::narrate;
use std::collections::HashMap;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountType};
use bollard::query_parameters::{
    InspectContainerOptionsBuilder, KillContainerOptions, ListContainersOptionsBuilder,
    ListVolumesOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
    RemoveVolumeOptionsBuilder,
};
use bollard::Docker;
use sha2::{Digest, Sha256};
use tokio_stream::StreamExt;
use uuid::Uuid;

use crate::auth::Auth;
use crate::chain_walker::format_idleness_log;
use crate::policy::{CheckResult, GateOutcome, Stall, StallTracker};
use crate::workspace::{GateCommands, Workspace};

const POLICY_IMAGE_DIR: &str = "policy-image";

/// Where the deploy-keys volume mounts inside the container (issue #69
/// / ADR-0002). The path is the bellows-user's `~/.ssh/`, so cargo /
/// git can pick up the `config` + `known_hosts` + key files at the
/// standard openssh location with no extra config inside the sandbox.
const SSH_KEYS_PATH_IN_CONTAINER: &str = "/home/bellows/.ssh";

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

/// Docker label key that flags a volume (or container) as Bellows-managed.
/// Both `bellows prune` discovery and the slice-7 orphan cleanup match on
/// this; one literal so a rename can't desync the two.
pub const BELLOWS_MANAGED_LABEL: &str = "bellows-managed";
/// Docker label key naming the **kind** of cache volume — `target` (per-repo)
/// or `cargo-registry` (shared). Absence of this label is the signal that a
/// volume is NOT a cache volume (e.g. the credentials volume), so `prune`
/// post-filters on its presence + value to never touch credentials.
pub const VOLUME_KIND_LABEL: &str = "bellows-volume-kind";
/// Docker label key on per-repo `target/` volumes carrying the repo slug
/// the volume belongs to. Read by `bellows prune` to render the per-repo
/// volume row's `repo-slug` column.
pub const REPO_SLUG_LABEL: &str = "bellows-repo-slug";

pub(crate) const VOLUME_KIND_TARGET: &str = "target";
pub(crate) const VOLUME_KIND_CARGO_REGISTRY: &str = "cargo-registry";

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
    /// Issue #164: the **Stall** shape observed while this container
    /// ran, if any. `Some(Stall::Oscillation)` means the workspace
    /// returned to a previously-seen state — the runner consults
    /// `chain_walker::decide_oscillation_advance_action` to decide
    /// whether that earns an **Advance**. `Some(Stall::Idleness)` is
    /// recorded for the operator and never acted on. `None` when the
    /// run was not sampled (every phase but implement) or nothing was
    /// seen.
    pub stall: Option<Stall>,
}

/// Issue #164: periodic sampling of the implement-phase workspace
/// while the container runs, so a wedged engine is caught rather than
/// burning the whole budget.
///
/// The workspace is a host directory bind-mounted into the container
/// (see the `Mount` built in `run_agent`), which is what lets bellows
/// run git against it from the host while the agent writes.
#[derive(Debug, Clone)]
pub struct StallWatch {
    /// Host path of the bind-mounted workspace to sample.
    pub workspace_path: PathBuf,
    /// How often to sample (`[agent].oscillation_sample_seconds`).
    pub interval: Duration,
    /// Consecutive identical samples that constitute **Idleness**.
    pub idleness_samples: usize,
    /// The base SHA the phase started from. An **Advance** discards the
    /// workspace, so the container is only ever interrupted while HEAD
    /// is still here — if the agent self-committed mid-run there is
    /// committed work to lose and the oscillation is recorded instead.
    pub base_sha: String,
    /// How long into this container's run an **Oscillation** may still
    /// kill it. `None` means observe-and-log only: the run's advance
    /// allowance is already spent, or too little budget remains for a
    /// fresh engine to be worth handing to, so killing the container
    /// would cost the run its remaining time for nothing.
    pub kill_within: Option<Duration>,
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

/// Borrow-friendly view of one `[[repo]]` block, just the fields the
/// deploy-keys validator needs (issue #69 / ADR-0002). Decoupled from
/// `config::RepoConfig` so the validator is callable from tests
/// without round-tripping through TOML, and from production code by
/// mapping `&[RepoConfig]` → `Vec<DeployKeyRepo>`.
#[derive(Debug, Clone)]
pub struct DeployKeyRepo {
    pub url: String,
    pub deploy_keys: Vec<String>,
}

/// One missing reference: a `[[repo]] deploy_keys` name that wasn't
/// found in the configured `ssh_keys_volume`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingDeployKey {
    pub key: String,
    pub repo_url: String,
}

/// The error raised when one or more `[[repo]] deploy_keys` references
/// have no matching file in the deploy-keys volume. Carries the full
/// list of misses so the operator can fix every gap in one sitting
/// rather than rerunning startup validation N times.
#[derive(Debug, Clone, thiserror::Error)]
pub struct MissingDeployKeysError {
    pub missing: Vec<MissingDeployKey>,
}

impl std::fmt::Display for MissingDeployKeysError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "bellows: refusing to start — {} deploy-key reference{} in `[[repo]] deploy_keys` not present in the deploy-keys volume:",
            self.missing.len(),
            if self.missing.len() == 1 { "" } else { "s" },
        )?;
        for m in &self.missing {
            write!(f, "\n  - key `{}` referenced by repo `{}`", m.key, m.repo_url)?;
        }
        write!(
            f,
            "\nrun `bellows setup-deploy-keys add <name>` for each missing key, or remove the reference from `[[repo]] deploy_keys` if the repo no longer needs it.",
        )?;
        Ok(())
    }
}

/// Pure validator: every name in any `[[repo]] deploy_keys` list must
/// be present as a key in `present` (typically the set of regular
/// filenames in the `ssh_keys_volume`). Returns Ok when every
/// reference resolves; otherwise an error carrying every miss so the
/// operator sees the full list in one pass.
///
/// Pure function — the docker-side wrapper
/// (`validate_deploy_keys_against_volume`) is responsible for asking
/// the daemon what filenames exist, then delegating to this function
/// for the pure logic.
pub fn validate_deploy_keys_against_present(
    repos: &[DeployKeyRepo],
    present: &std::collections::HashSet<String>,
) -> Result<(), MissingDeployKeysError> {
    let mut missing = Vec::new();
    for r in repos {
        for k in &r.deploy_keys {
            if !present.contains(k) {
                missing.push(MissingDeployKey {
                    key: k.clone(),
                    repo_url: r.url.clone(),
                });
            }
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(MissingDeployKeysError { missing })
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
    #[error("volume `{name}` is currently in use by a container and cannot be removed")]
    VolumeInUse { name: String },
    #[error("volume `{name}` does not exist")]
    VolumeNotFound { name: String },
    #[error(transparent)]
    MissingDeployKeys(#[from] MissingDeployKeysError),
    #[error("could not list filenames in deploy-keys volume `{volume}`: docker run exited with status {status}")]
    DeployKeysVolumeListFailed { volume: String, status: String },
    #[error("auth env: {0}")]
    AuthEnv(#[source] anyhow::Error),
}

/// How long a single Docker API request may wait for the daemon's
/// response headers before bollard gives up with
/// `RequestTimeoutError` (issue #194). Tighter than bollard's
/// two-minute default so a daemon that has stopped answering becomes a
/// *retryable* error in bounded time rather than an open-ended wait.
///
/// This bounds the wait for the response to arrive, not the lifetime
/// of the body that follows it (bollard applies it around the request
/// future in `execute_request`, then streams the body separately), so
/// a container that runs for hours, and the `follow=true` log stream
/// attached to it, are unaffected.
const DAEMON_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Portion of a budgeted attempt kept for stopping, observing, and
/// removing a container after its workload deadline. Without a reserve,
/// the workload timer and the attempt timer would fire together, leaving
/// no opportunity to send SIGKILL or reconcile the known container id
/// while still honoring the attempt's wall-clock bound.
const CONTAINER_CLEANUP_RESERVE: Duration = Duration::from_secs(5);

/// The one place bellows builds a Docker client. Local-defaults
/// connector (unix socket / named pipe, or `DOCKER_HOST` when set),
/// plus the [`DAEMON_REQUEST_TIMEOUT`] every call inherits. Building a
/// client does not touch the daemon — a failure here is a malformed
/// `DOCKER_HOST` or a missing socket path, not an unreachable daemon.
pub fn connect_docker() -> Result<Docker, SandboxError> {
    Ok(with_daemon_timeout(Docker::connect_with_local_defaults()?))
}

/// Apply [`DAEMON_REQUEST_TIMEOUT`] to a client. Split out from
/// [`connect_docker`] so the bound is testable without a live daemon
/// socket — the sandbox the gate runs in has no `/var/run/docker.sock`.
fn with_daemon_timeout(docker: Docker) -> Docker {
    docker.with_timeout(DAEMON_REQUEST_TIMEOUT)
}

/// How many times one container lifecycle may be attempted against a
/// daemon that keeps dropping the connection (issue #194). Small on
/// purpose: a dropped socket is usually a momentary blip and comes
/// back on the next attempt, while a daemon that fails three attempts
/// in a row is sick in a way bellows cannot fix by asking again.
const DAEMON_TRANSPORT_MAX_ATTEMPTS: u32 = 3;

/// Backoff before the second attempt; the third waits twice this. Kept
/// short — the daemon is a local process, so there is no far-end
/// recovery to wait out, only the moment it takes for a restarted or
/// briefly-wedged daemon to accept connections again.
const DAEMON_TRANSPORT_BACKOFF_BASE: Duration = Duration::from_secs(2);

/// How hard bellows tries a container lifecycle against a daemon that
/// keeps dropping the connection. A parameter of `run_container`
/// rather than a pair of constants read inline so the lifecycle tests
/// can exercise the real attempt accounting without spending the real
/// backoff seconds.
#[derive(Debug, Clone, Copy)]
struct TransportRetryPolicy {
    max_attempts: u32,
    backoff_base: Duration,
}

impl Default for TransportRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: DAEMON_TRANSPORT_MAX_ATTEMPTS,
            backoff_base: DAEMON_TRANSPORT_BACKOFF_BASE,
        }
    }
}

/// Why a transport retry was not attempted. Both outcomes surface the
/// original error unchanged; the distinction is for the operator
/// reading the log — a sick daemon (`AttemptsExhausted`) and a phase
/// that simply ran out of clock (`BudgetExhausted`) call for different
/// follow-ups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryStop {
    AttemptsExhausted,
    BudgetExhausted,
}

/// Time left in this container run's wall-clock budget after `elapsed`.
/// `None` in, `None` out — an unbudgeted run stays unbudgeted across
/// retries. An overrun clamps to zero rather than wrapping.
fn remaining_budget(budget: Option<Duration>, elapsed: Duration) -> Option<Duration> {
    budget.map(|b| b.saturating_sub(elapsed))
}

/// Await one part of a container attempt without letting that operation
/// extend the attempt's absolute deadline. Docker has its own per-request
/// timeout, but a phase may have much less time left than that client-wide
/// bound. Report expiry in the same transport shape Bollard uses for a
/// daemon request timeout so a pre-create expiry reaches the existing
/// budget-aware retry accounting.
async fn before_attempt_deadline<T>(
    deadline: Option<tokio::time::Instant>,
    future: impl Future<Output = T>,
) -> Result<T, SandboxError> {
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, future)
            .await
            .map_err(|_| SandboxError::Bollard(bollard::errors::Error::RequestTimeoutError)),
        None => Ok(future.await),
    }
}

/// How long to wait before attempt `attempt + 1`, or why there will
/// not be one. `attempt` is the 1-based number of the attempt that
/// just failed.
///
/// The budget check is what keeps the retry allowance inside the
/// phase's wall-clock budget rather than on top of it: the backoff has
/// to fit in what is left, and the retried attempt then inherits the
/// remainder as its own deadline (see `run_container`).
fn next_retry_delay(
    attempt: u32,
    policy: TransportRetryPolicy,
    budget: Option<Duration>,
    elapsed: Duration,
) -> Result<Duration, RetryStop> {
    if attempt >= policy.max_attempts {
        return Err(RetryStop::AttemptsExhausted);
    }
    let delay = policy.backoff_base * 2u32.pow(attempt - 1);
    match remaining_budget(budget, elapsed) {
        Some(remaining) if remaining <= delay => Err(RetryStop::BudgetExhausted),
        _ => Ok(delay),
    }
}

/// The line an operator reads when bellows is about to retry a dropped
/// daemon connection. Names the attempt and the bound so repeated
/// retries read as a sick daemon rather than as a long silence, and
/// quotes the error verbatim so the eventual failure and the retries
/// that preceded it are greppable by the same string.
fn format_daemon_retry_log(
    attempt: u32,
    max_attempts: u32,
    err: &SandboxError,
    delay: Duration,
) -> String {
    format!(
        "bellows: docker daemon transport failure (attempt {attempt}/{max_attempts}): {err}; \
         retrying in {}s",
        delay.as_secs(),
    )
}

/// The closing line of a retry sequence that gave up. The error itself
/// is returned unchanged to the caller — this line only records *why*
/// bellows stopped asking, which the error alone cannot say.
fn format_daemon_retry_stop_log(
    attempt: u32,
    max_attempts: u32,
    err: &SandboxError,
    stop: RetryStop,
) -> String {
    let reason = match stop {
        RetryStop::AttemptsExhausted => "retries exhausted".to_string(),
        RetryStop::BudgetExhausted => {
            "no wall-clock budget left to retry within".to_string()
        }
    };
    format!(
        "bellows: docker daemon transport failure (attempt {attempt}/{max_attempts}): {err}; \
         {reason}",
    )
}

/// Whether a Bollard error is a failure of the *transport* between
/// bellows and the Docker daemon, rather than an answer either the
/// daemon or a container has already given (issue #194).
///
/// This is the retry predicate: a transport failure means bellows never
/// learned anything about the operation it asked for, so re-issuing it
/// is the only way to find out. Everything else — including every
/// verdict about the code under test — is surfaced unchanged.
///
/// Retryable, with the error text that motivated each:
///   - [`IOError`](bollard::errors::Error::IOError): the connection to
///     the daemon dropped mid-request. Observed verbatim on seven runs
///     aborted in the 2026-07-25 → 2026-07-28 window on
///     `marad2001/workboard-financial-advice` (#39, #46, #280, #314,
///     #672, #675) as `error reading a body from connection`, a
///     `std::io::ErrorKind::Other` custom error. Covers the writer-side
///     half (`BrokenPipe`, `ConnectionReset`) of the same drop.
///   - [`RequestTimeoutError`](bollard::errors::Error::RequestTimeoutError):
///     the daemon never answered within the client timeout (see
///     [`DAEMON_REQUEST_TIMEOUT`]). The eighth abort in that window.
///   - [`HyperResponseError`](bollard::errors::Error::HyperResponseError)
///     and [`HyperLegacyError`](bollard::errors::Error::HyperLegacyError):
///     the same class one layer down, when hyper reports the connection
///     failure before bollard maps it to an IO error. Not observed in
///     the aborted runs, but indistinguishable in kind — a dropped
///     socket surfaces as whichever layer noticed first.
///
/// Deliberately NOT retryable:
///   - [`DockerContainerWaitError`](bollard::errors::Error::DockerContainerWaitError):
///     a container that started and exited non-zero. That is a verdict
///     about the code, not about the connection; retrying it would
///     re-run the cargo gate and re-bill the agent phase. `run_container`
///     already un-wraps this variant back into a plain exit code, so it
///     never reaches the retry loop as an error — the predicate says no
///     a second time so a future caller can't get it wrong.
///   - [`DockerResponseServerError`](bollard::errors::Error::DockerResponseServerError):
///     the daemon received the request and replied. The transport
///     worked; re-sending gets the same answer.
///   - Everything else (JSON decode failures, URL/encoding errors,
///     certificate problems): deterministic, so a retry cannot help.
pub fn is_transport_failure(err: &bollard::errors::Error) -> bool {
    use bollard::errors::Error;
    matches!(
        err,
        Error::IOError { .. }
            | Error::RequestTimeoutError
            | Error::HyperResponseError { .. }
            | Error::HyperLegacyError { .. }
    )
}

/// List every regular filename in the named deploy-keys volume (issue
/// #69 / ADR-0002 startup validation). Spawns a one-shot policy-image
/// container with the volume mounted read-only and runs `ls -1A`
/// inside it; that path is portable across Docker Desktop (where the
/// host filesystem cannot reach the volume's mountpoint) and Linux
/// Docker Engine alike. If the volume does not exist yet, docker
/// creates it empty on first mount — the validator then sees an
/// empty `present` set and surfaces every reference as missing.
pub async fn list_deploy_keys_in_volume(
    volume: &str,
) -> Result<std::collections::HashSet<String>, SandboxError> {
    let image_tag = ensure_policy_image().await?;
    // `--user 0` so root inside the container can read regardless of
    // whether the volume was populated with bellows uid 1000 ownership.
    // `ls -1A` prints one filename per line and omits `.`/`..`.
    let output = tokio::process::Command::new("docker")
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
            "ls -1A /sshvol 2>/dev/null || true",
        ])
        .output()
        .await?;
    if !output.status.success() {
        return Err(SandboxError::DeployKeysVolumeListFailed {
            volume: volume.to_string(),
            status: format!("{}", output.status),
        });
    }
    // Filter the volume's own metadata files (`config`, `known_hosts`)
    // out of the present-set so the validator and the operator-facing
    // `setup-deploy-keys list` agree on what counts as a key. Without
    // this, `[[repo]] deploy_keys = ["config"]` would validate falsely
    // and then mount-shadow the config file at container startup.
    let present = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && l != "config" && l != "known_hosts")
        .collect();
    Ok(present)
}

/// Startup validation entry point (issue #69 / ADR-0002 AC9). Walks
/// every `[[repo]] deploy_keys` reference; if every name resolves to
/// a file in the configured `ssh_keys_volume`, returns Ok. Otherwise
/// returns `SandboxError::MissingDeployKeys` carrying every miss so
/// the operator sees the full punch list and can rerun
/// `setup-deploy-keys add` for each in one sitting.
///
/// Short-circuits without touching docker when no `[[repo]]` opted in
/// — both the cheap path AND the preservation of the "no creds in
/// sandbox by default" posture (we don't spawn a container at all).
pub async fn validate_deploy_keys(
    repos: &[DeployKeyRepo],
    ssh_keys_volume: &str,
) -> Result<(), SandboxError> {
    if repos.iter().all(|r| r.deploy_keys.is_empty()) {
        return Ok(());
    }
    let present = list_deploy_keys_in_volume(ssh_keys_volume).await?;
    validate_deploy_keys_against_present(repos, &present)?;
    Ok(())
}

/// Build (or reuse the cached) policy image and return its tag. Used by
/// both `run_agent` and `bellows setup-auth`.
pub async fn ensure_policy_image() -> Result<String, SandboxError> {
    let hash = compute_dir_content_hash(Path::new(POLICY_IMAGE_DIR))?;
    let image_tag = format!("bellows-policy:{}", &hash[..12]);
    ensure_image_built(&hash, &image_tag).await?;
    Ok(image_tag)
}

/// Write one bellows line to both the console and the run log, the way
/// the runner's own `announce` does — the operator watching the console
/// and the operator reading `bellows.log` see the same line.
fn announce(line: &str, log_writer: &mut dyn Write) {
    println!("{line}");
    crate::run_log::narrate(log_writer, line);
}

/// How the lifecycle helper should retain the container's stdout/stderr.
#[derive(Debug, Clone, Copy)]
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

#[derive(Debug)]
struct ContainerOutcome {
    exit_code: i64,
    captured: String,
    /// True when the lifecycle was terminated by the deadline firing
    /// rather than by the container exiting on its own. The runner uses
    /// this to set `PhaseOutcomes::wall_clock_exceeded` and short-
    /// circuit the rest of the pipeline.
    killed_by_deadline: bool,
    /// Issue #164: the **Stall** shape observed by the workspace
    /// sampler, when one was configured.
    stall: Option<Stall>,
}

/// One tick of the issue-#164 stall sampler: sample the workspace,
/// feed the hash to the tracker, and narrate whatever shape became
/// visible. Returns the newly-observed **Stall** shape, or `None` when
/// the tick learned nothing new.
///
/// A failed sample (a torn read while the container writes, or a
/// transient git failure) is swallowed: the classifier needs a hash to
/// recur three times before it reports **Oscillation**, so a missed
/// sample can at worst delay or skip a detection, never invent one.
async fn observe_stall(
    watch: &StallWatch,
    tracker: &mut StallTracker,
    log_writer: &mut dyn Write,
    within_kill_window: bool,
) -> Option<StallObservation> {
    let hash = crate::workspace::sample_workspace_state(&watch.workspace_path)
        .await
        .ok()?;
    let shape = tracker.observe(hash)?;
    // The at-base-SHA guard, applied here so the container is only ever
    // interrupted when an advance is genuinely available. Checked only
    // on the tick that reports an Oscillation, not on every sample.
    // A failed read is read conservatively as "do not interrupt".
    let kill = shape == Stall::Oscillation
        && within_kill_window
        && crate::workspace::head_sha_at(&watch.workspace_path)
            .await
            .is_ok_and(|head| head == watch.base_sha);
    let line = match shape {
        Stall::Idleness => {
            format_idleness_log(tracker.idleness_samples(), watch.interval.as_secs())
        }
        Stall::Oscillation => format!(
            "bellows: implement workspace oscillation detected (it returned to a \
             previously-seen state){}",
            if kill {
                "; stopping the agent container so the phase can advance"
            } else {
                ""
            },
        ),
    };
    announce(&line, log_writer);
    Some(StallObservation { shape, kill })
}

/// What one sampler tick learned: the **Stall** shape that became
/// visible, and whether it earns interrupting the container.
struct StallObservation {
    shape: Stall,
    kill: bool,
}

/// Run a container through its full lifecycle, retrying creation a
/// bounded number of times when the *connection to the daemon* fails
/// (issue #194).
///
/// A lost create response is safe to retry because no workload can
/// have started. Once create returns a container id, start/log/wait
/// transport failures are ambiguous: the workload may already be
/// running or finished, so creating a replacement could execute it
/// twice. Those errors are surfaced on the first attempt. Which create
/// errors qualify is [`is_transport_failure`]; everything else —
/// above all a container that started and exited non-zero, which never
/// surfaces as an error here at all (see the
/// `DockerContainerWaitError` arm below) — is returned untouched.
///
/// Retries are spent *inside* `deadline`, never on top of it: each
/// attempt inherits what is left of the budget, and a backoff that
/// would not fit stops the sequence. When the retries run out the
/// original error is returned unchanged, so the operator diagnoses the
/// daemon rather than a bellows wrapper.
///
/// The one container that can outlive an attempt is one the daemon
/// created but whose create *response* was lost — bellows never
/// learned its id. That one cannot have started and is reconciled by
/// `cleanup_orphan_containers` at the next `bellows run` startup,
/// which is why that sweep matches `created` as well as `exited`
/// containers.
async fn run_container(
    docker: &Docker,
    config: ContainerCreateBody,
    log_writer: &mut dyn Write,
    capture_mode: CaptureMode,
    deadline: Option<Duration>,
    stall_watch: Option<StallWatch>,
    retry: TransportRetryPolicy,
) -> Result<ContainerOutcome, SandboxError> {
    let started = std::time::Instant::now();
    let mut attempt = 1u32;
    loop {
        let attempt_deadline = remaining_budget(deadline, started.elapsed());
        let err = match run_container_once(
            docker,
            config.clone(),
            log_writer,
            capture_mode,
            attempt_deadline,
            stall_watch.clone(),
        )
        .await
        {
            Ok(outcome) => return Ok(outcome),
            Err(ContainerAttemptError::AfterCreate(err)) => return Err(err),
            Err(ContainerAttemptError::Create(err)) => err,
        };

        let transport = matches!(&err, SandboxError::Bollard(e) if is_transport_failure(e));
        if !transport {
            return Err(err);
        }

        match next_retry_delay(attempt, retry, deadline, started.elapsed()) {
            Ok(delay) => {
                announce(
                    &format_daemon_retry_log(attempt, retry.max_attempts, &err, delay),
                    log_writer,
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(stop) => {
                announce(
                    &format_daemon_retry_stop_log(attempt, retry.max_attempts, &err, stop),
                    log_writer,
                );
                // The original error, with no bellows wrapper around
                // it: the operator diagnoses the daemon's own words.
                return Err(err);
            }
        }
    }
}

/// Carries the only fact the retry boundary needs: whether a container
/// id was received. Before that point no workload could have started;
/// after it, every transport result is potentially ambiguous.
enum ContainerAttemptError {
    Create(SandboxError),
    AfterCreate(SandboxError),
}

/// One attempt at the container lifecycle: create, start, stream
/// stdout/stderr to `log_writer` while capturing per `capture_mode`,
/// wait for exit, force-remove. A failed removal is surfaced rather
/// than proceeding as though cleanup succeeded.
///
/// Non-zero container exit is returned as `exit_code`, NOT as a sandbox
/// error — the caller (run_agent / run_cargo_checks) and ultimately
/// policy::classify_exit decide what a non-zero exit means.
///
/// `deadline` is the wall-clock budget for THIS entire attempt — what
/// `run_container` has left after any earlier attempts. It covers
/// create, start, lifecycle observation, and removal. A small tail is
/// reserved after the workload deadline for SIGKILL, wait, and removal;
/// when the workload deadline fires, `killed_by_deadline` is set. When
/// `None`, the container runs to natural completion regardless of
/// elapsed time.
///
/// `stall_watch` (issue #164) opts this run into periodic **Stall**
/// sampling of the bind-mounted workspace. It cooperates with the log
/// stream rather than replacing it: sampling is a third branch of the
/// same `select!`, so logs keep streaming while bellows watches for a
/// wedged engine.
async fn run_container_once(
    docker: &Docker,
    config: ContainerCreateBody,
    log_writer: &mut dyn Write,
    capture_mode: CaptureMode,
    deadline: Option<Duration>,
    stall_watch: Option<StallWatch>,
) -> Result<ContainerOutcome, ContainerAttemptError> {
    // One absolute instant for the whole attempt. Re-arming a duration
    // after create or start would let each setup request spend the
    // Docker client's independent timeout before this budget began.
    let deadline_at = deadline.map(|duration| tokio::time::Instant::now() + duration);

    let container = before_attempt_deadline(deadline_at, docker.create_container(None, config))
        .await
        .map_err(ContainerAttemptError::Create)?
        .map_err(SandboxError::from)
        .map_err(ContainerAttemptError::Create)?;
    let id = container.id;

    // Once an id is known, stop setup/workload slightly before the hard
    // attempt bound so kill/wait/remove can still make progress inside it.
    let lifecycle_deadline_at = deadline_at.map(|attempt_deadline| {
        let now = tokio::time::Instant::now();
        attempt_deadline
            .checked_sub(CONTAINER_CLEANUP_RESERVE)
            .unwrap_or(now)
            .max(now)
    });

    // Once the container exists on the daemon it must be removed even if
    // start/log/wait fail. Run the lifecycle inside an inner async block
    // and force-remove unconditionally afterwards.
    let lifecycle: Result<ContainerOutcome, SandboxError> = async {
        before_attempt_deadline(lifecycle_deadline_at, docker.start_container(&id, None))
            .await??;

        // Box the deadline future so we can race it against the log
        // stream in tokio::select! while keeping a single sleep for
        // the whole attempt (not re-armed after setup or each loop).
        // When deadline is None, fall back to a never-completing future
        // so the deadline branch effectively never wins.
        let mut deadline_future: Pin<Box<dyn Future<Output = ()> + Send>> =
            match lifecycle_deadline_at {
                Some(deadline) => Box::pin(tokio::time::sleep_until(deadline)),
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

        // Issue #164 stall sampling. The first tick fires one interval
        // in, not immediately: a sample taken before the agent has had
        // a chance to touch anything says nothing.
        let started = std::time::Instant::now();
        let mut tracker = stall_watch
            .as_ref()
            .map(|w| StallTracker::new(w.idleness_samples));
        let mut sample_interval = stall_watch.as_ref().map(|w| {
            let mut interval =
                tokio::time::interval_at(tokio::time::Instant::now() + w.interval, w.interval);
            // A slow git sample must not cause a burst of catch-up
            // ticks; delay the schedule instead.
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval
        });
        let mut stall: Option<Stall> = None;

        loop {
            tokio::select! {
                _ = async {
                    match sample_interval.as_mut() {
                        Some(interval) => { interval.tick().await; }
                        // No watch configured — this branch never wins.
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    let watch = stall_watch
                        .as_ref()
                        .expect("sample interval only exists alongside a stall watch");
                    let tracker = tracker
                        .as_mut()
                        .expect("tracker only exists alongside a stall watch");
                    // The budget floor, expressed against this
                    // container's own clock: past it, too little of the
                    // run's budget would be left for a fresh engine to
                    // do anything with, so an oscillation is recorded
                    // and the container left to finish.
                    let within_kill_window = watch
                        .kill_within
                        .is_some_and(|window| started.elapsed() <= window);
                    if let Some(observation) =
                        observe_stall(watch, tracker, log_writer, within_kill_window).await
                    {
                        match observation.shape {
                            Stall::Oscillation => stall = Some(Stall::Oscillation),
                            // Never overwrites an Oscillation: the
                            // actionable shape is the one the runner
                            // needs to see.
                            Stall::Idleness => {
                                stall.get_or_insert(Stall::Idleness);
                            }
                        }
                        if observation.kill {
                            let _ = before_attempt_deadline(
                                deadline_at,
                                docker.kill_container(&id, None::<KillContainerOptions>),
                            )
                            .await;
                            break;
                        }
                    }
                }
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
                            // run-log-raw: the container tee relays the
                            // agent's and cargo's own stdout verbatim.
                            // Issue #195 deliberately leaves it
                            // unstamped — it is the bulk of the file,
                            // and prefixing it would inflate the log by
                            // roughly half and break every diff and
                            // code block the agent emits.
                            log_writer.write_all(&bytes)?;
                            log_writer.flush()?;
                            captured.append(&bytes);
                        }
                    }
                }
                _ = &mut deadline_future => {
                    // Deadline fired — SIGKILL the container. wait_container
                    // below will pick up the kill exit code (typically 137).
                    let _ = before_attempt_deadline(
                        deadline_at,
                        docker.kill_container(&id, None::<KillContainerOptions>),
                    )
                    .await;
                    killed_by_deadline = true;
                    break;
                }
            }
        }

        let mut wait_stream = docker.wait_container(&id, None);
        let mut exit_code = 0i64;
        while let Some(response) = before_attempt_deadline(deadline_at, wait_stream.next()).await? {
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
            stall,
        })
    }
    .await;

    let remove_options = RemoveContainerOptionsBuilder::default().force(true).build();
    let cleanup = before_attempt_deadline(
        deadline_at,
        docker.remove_container(&id, Some(remove_options)),
    )
    .await
    .and_then(|result| result.map_err(SandboxError::from));

    match (lifecycle, cleanup) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(cleanup_err)) => Err(ContainerAttemptError::AfterCreate(cleanup_err)),
        (Err(lifecycle_err), _) => Err(ContainerAttemptError::AfterCreate(lifecycle_err)),
    }
}

// Issue #69 added two more arguments (ssh_keys_volume + deploy_keys);
// the existing call surface was already at clippy's
// too_many_arguments boundary. Bundling into a struct would just
// rename the boilerplate without simplifying it, so suppressed here
// — the runner is the only caller and the call sites are kept tidy
// with their own one-line summaries.
#[allow(clippy::too_many_arguments)]
pub async fn run_agent(
    workspace: &Workspace,
    auth: &Auth,
    issue_number: u64,
    repo: &str,
    repo_slug: &str,
    ssh_keys_volume: &str,
    deploy_keys: &[String],
    log_writer: &mut dyn Write,
    deadline: Option<Duration>,
    stall_watch: Option<StallWatch>,
) -> Result<AgentRun, SandboxError> {
    let env = build_agent_env(issue_number, auth)?;

    let image_tag = ensure_policy_image().await?;

    let docker = connect_docker()?;
    let run_id = Uuid::new_v4().to_string();

    // tempfile gives an absolute path already; canonicalize() on Windows
    // returns `\\?\C:\...` extended-length paths that Docker Desktop's
    // bind-mount handler rejects, so we use the path as-is.
    let workspace_path = workspace.path().to_string_lossy().to_string();

    let labels = build_managed_labels(&run_id, issue_number, repo, None);

    // Structured Mount API rather than `binds: Vec<String>` to avoid
    // collision with bind syntax's `:` separator on Windows drive
    // letters. Auth contributes credentials volumes; build_cache_mounts
    // contributes the per-repo target + shared cargo registry caches.
    // build_ssh_keys_mount contributes the read-only deploy-keys mount
    // when (and only when) the active [[repo]] opted in via deploy_keys.
    let mut mounts = vec![Mount {
        target: Some("/workspace".to_string()),
        source: Some(workspace_path),
        typ: Some(MountType::BIND),
        ..Default::default()
    }];
    mounts.extend(auth.extra_mounts());
    mounts.extend(build_cache_mounts(repo_slug));
    if let Some(m) = build_ssh_keys_mount(ssh_keys_volume, deploy_keys) {
        mounts.push(m);
    }

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
        stall_watch,
        TransportRetryPolicy::default(),
    )
    .await?;

    Ok(AgentRun {
        exit_code: outcome.exit_code,
        stderr_tail: outcome.captured,
        killed_by_deadline: outcome.killed_by_deadline,
        stall: outcome.stall,
    })
}

fn build_agent_env(issue_number: u64, auth: &Auth) -> Result<Vec<String>, SandboxError> {
    let mut env = vec![format!("BELLOWS_ISSUE_NUMBER={issue_number}")];
    env.extend(auth.try_extra_env().map_err(SandboxError::AuthEnv)?);
    Ok(env)
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
/// `env_override` (issue #186) forces build env for this invocation,
/// winning over anything mirrored from the target's CI. The OOM retry
/// path uses it to serialise linking (`CARGO_BUILD_JOBS=1`); an empty
/// slice is the normal case and changes nothing.
///
/// No credentials volume — the gate has no Anthropic dependency.
// See run_agent's note on the too_many_arguments suppression.
#[allow(clippy::too_many_arguments)]
pub async fn run_cargo_checks(
    workspace: &Workspace,
    issue_number: u64,
    repo: &str,
    repo_slug: &str,
    ssh_keys_volume: &str,
    deploy_keys: &[String],
    log_writer: &mut dyn Write,
    deadline: Option<Duration>,
    env_override: &[(String, String)],
) -> Result<CargoChecksRun, SandboxError> {
    let image_tag = ensure_policy_image().await?;

    let docker = connect_docker()?;
    let run_id = Uuid::new_v4().to_string();

    let workspace_path = workspace.path().to_string_lossy().to_string();

    let labels = build_managed_labels(&run_id, issue_number, repo, Some("cargo-checks-gate"));

    let mut mounts = vec![Mount {
        target: Some("/workspace".to_string()),
        source: Some(workspace_path),
        typ: Some(MountType::BIND),
        ..Default::default()
    }];
    mounts.extend(build_cache_mounts(repo_slug));
    // Same per-repo SSH deploy-keys mount as the agent container —
    // both phases need private-dep access via cargo. Brief: "Applies
    // symmetrically to run_agent and run_cargo_checks. Both phases
    // need private-dep access."
    if let Some(m) = build_ssh_keys_mount(ssh_keys_volume, deploy_keys) {
        mounts.push(m);
    }

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
    //
    // ADR-0004: pass the snapshotted gate commands into the container
    // via BELLOWS_CLIPPY_CMD / BELLOWS_TEST_CMD env vars. The script
    // eval's each one verbatim so the gate mirrors target CI rather
    // than running bellows's old hardcoded flag set.
    let config = ContainerCreateBody {
        image: Some(image_tag),
        entrypoint: Some(build_cargo_checks_entrypoint()),
        cmd: Some(vec![]),
        env: Some(build_cargo_checks_env(
            workspace.gate_commands(),
            env_override,
        )),
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
    // The cargo-checks gate is bellows' own container, not an agent's
    // — there is no engine to stall, so no sampling.
    let outcome = run_container(
        &docker,
        config,
        log_writer,
        CaptureMode::Full,
        deadline,
        None,
        TransportRetryPolicy::default(),
    )
    .await?;

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
        (BELLOWS_MANAGED_LABEL.to_string(), "true".to_string()),
        (VOLUME_KIND_LABEL.to_string(), VOLUME_KIND_TARGET.to_string()),
        (REPO_SLUG_LABEL.to_string(), repo_slug.to_string()),
    ]);
    let registry_labels = HashMap::from([
        (BELLOWS_MANAGED_LABEL.to_string(), "true".to_string()),
        (
            VOLUME_KIND_LABEL.to_string(),
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

/// Mount filter for the per-repo SSH deploy keys (issue #69 / ADR-0002).
/// Returns `Some(Mount)` only when the active `[[repo]]` block opted in
/// by declaring at least one name in `deploy_keys`; otherwise `None`,
/// preserving the "no creds in sandbox by default" posture for every
/// repo (including bellows-on-bellows) that did not opt in.
///
/// The mount is always READ-ONLY: that is the security boundary per
/// ADR-0002 — an escaping agent cannot tamper with the keys.
///
/// The names inside `deploy_keys` are not consulted here; they're
/// resolved against the volume's filesystem at startup
/// (`validate_deploy_keys`), where a missing key short-circuits
/// the run with a clear error. This function only decides "mount or
/// no mount" based on whether the list is empty.
fn build_ssh_keys_mount(ssh_keys_volume: &str, deploy_keys: &[String]) -> Option<Mount> {
    if deploy_keys.is_empty() {
        return None;
    }
    Some(Mount {
        target: Some(SSH_KEYS_PATH_IN_CONTAINER.to_string()),
        source: Some(ssh_keys_volume.to_string()),
        typ: Some(MountType::VOLUME),
        read_only: Some(true),
        ..Default::default()
    })
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

/// ADR-0004: build the env list bellows hands the cargo-checks
/// container so `run-cargo-checks` evaluates the snapshotted clippy
/// and test commands verbatim. Pulled out into a pure function so
/// the env shape is unit-testable without spinning up Docker.
///
/// Issue #180: each command is prefixed with the build-relevant env its
/// CI step ran under, as a POSIX `VAR=value cmd` assignment prefix. The
/// prefix rides inside the existing `BELLOWS_*_CMD` string rather than
/// arriving as separate container env because the two commands can carry
/// *different* env (sibling `clippy:` / `test:` jobs), and container-wide
/// env cannot express that. It also means the policy image's
/// `run-cargo-checks` script needs no change — it already `sh -c`'s each
/// command, and `sh` applies assignment prefixes natively.
/// `env_override` (issue #186) is merged over the CI-mirrored env, so a
/// retry's forced value cannot be undone by a same-named variable the
/// target's CI happens to set.
fn build_cargo_checks_env(
    gate_commands: &GateCommands,
    env_override: &[(String, String)],
) -> Vec<String> {
    vec![
        format!(
            "BELLOWS_CLIPPY_CMD={}",
            with_env_prefix(&gate_commands.clippy_env, env_override, &gate_commands.clippy),
        ),
        format!(
            "BELLOWS_TEST_CMD={}",
            with_env_prefix(&gate_commands.test_env, env_override, &gate_commands.test),
        ),
    ]
}

/// Render `KEY='value' ... <cmd>`. Values are single-quoted, which is
/// airtight for `sh` because `workflow_parse::env_value_is_safe` has
/// already rejected any value containing a single quote. An empty env
/// list (and no override) returns the command untouched, so repos whose
/// CI declares no build env see byte-identical behaviour to before #180.
///
/// `overrides` are applied last and win on a name collision. The merge
/// happens here rather than by emitting two assignments for the same
/// name, because `VAR=a VAR=b cmd` is not portably defined — one
/// assignment per name keeps the composed command unambiguous.
fn with_env_prefix(
    env: &[(String, String)],
    overrides: &[(String, String)],
    cmd: &str,
) -> String {
    if env.is_empty() && overrides.is_empty() {
        return cmd.to_string();
    }
    let mut merged: std::collections::BTreeMap<&str, &str> = env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    for (key, value) in overrides {
        merged.insert(key.as_str(), value.as_str());
    }
    let mut out = String::new();
    for (key, value) in merged {
        out.push_str(key);
        out.push_str("='");
        out.push_str(value);
        out.push_str("' ");
    }
    out.push_str(cmd);
    out
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
/// Always sets `bellows-managed=true`, `bellows-run-id=<run_id>`,
/// `bellows-issue-number=<issue_number>`, and `bellows-repo=<owner>/<name>`.
/// The `bellows-repo` label was added in issue #35 so the kill path can
/// disambiguate cross-repo issue-number collisions (issue #42 in repo A
/// vs issue #42 in repo B). Optionally sets `bellows-purpose=<purpose>`
/// when `purpose` is `Some` (the cargo-checks-gate uses this to
/// distinguish itself from the agent run).
fn build_managed_labels(
    run_id: &str,
    issue_number: u64,
    repo: &str,
    purpose: Option<&str>,
) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    labels.insert("bellows-managed".to_string(), "true".to_string());
    labels.insert("bellows-run-id".to_string(), run_id.to_string());
    labels.insert(
        "bellows-issue-number".to_string(),
        issue_number.to_string(),
    );
    labels.insert("bellows-repo".to_string(), repo.to_string());
    if let Some(p) = purpose {
        labels.insert("bellows-purpose".to_string(), p.to_string());
    }
    labels
}

/// Build the bollard list-containers label filter for finding the
/// container associated with a specific issue in a specific repo. Used
/// by `find_containers_for_issue` to locate the running agent or
/// cargo-checks container so `bellows kill <repo>/<N>` can force-remove
/// it. Pulled out as a pure function so the filter shape is
/// unit-testable without docker.
///
/// Filters on both `bellows-repo=<owner>/<name>` AND
/// `bellows-issue-number=<N>` so cross-repo issue-number collisions are
/// disambiguated (the operator who targets repo B's `#42` does not
/// accidentally remove repo A's `#42` container). The brief calls this
/// out explicitly as an issue #35 acceptance criterion.
fn build_issue_container_filter(repo: &str, issue_number: u64) -> HashMap<String, Vec<String>> {
    let mut filters: HashMap<String, Vec<String>> = HashMap::new();
    filters.insert(
        "label".to_string(),
        vec![
            "bellows-managed=true".to_string(),
            format!("bellows-repo={}", repo),
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
    repo: &str,
    issue_number: u64,
) -> Result<Vec<String>, SandboxError> {
    let filters = build_issue_container_filter(repo, issue_number);
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

fn build_orphan_container_filter() -> HashMap<String, Vec<String>> {
    let mut filters: HashMap<String, Vec<String>> = HashMap::new();
    filters.insert(
        "label".to_string(),
        vec![format!("{}=true", BELLOWS_MANAGED_LABEL)],
    );
    // `exited` is the ordinary orphan: a container that ran and whose
    // bellows process died before removing it. `created` (issue #194)
    // is the transport-failure orphan: the daemon created a container
    // and the create *response* was lost, so bellows never learned the
    // id and its own retry could not remove it. Both are safe to
    // force-remove; `running` is deliberately absent so the pre-claim
    // probe can still report a live container as Blocked.
    filters.insert(
        "status".to_string(),
        vec!["exited".to_string(), "created".to_string()],
    );
    filters
}

/// Bollard-backed implementation of [`crate::runner::AgentContainerProbe`].
///
/// Queries the local Docker daemon for any running container carrying
/// the `bellows-managed=true` label. Returns the first match's
/// container id + start time, or `None` if no such container is
/// running. Used by `runner::run_once`'s pre-claim concurrency=1 gate
/// (issue #126 / ADR-0009 slice 4), replacing the old
/// open-`agent/*`-PR proxy with a direct enforcement of the invariant.
pub struct DockerContainerProbe {
    docker: Docker,
}

impl DockerContainerProbe {
    /// Build a probe wired to the local Docker daemon — same connection
    /// style every other `sandbox.rs` daemon call uses.
    pub fn new() -> Result<Self, SandboxError> {
        let docker = connect_docker()?;
        Ok(Self { docker })
    }
}

impl crate::runner::AgentContainerProbe for DockerContainerProbe {
    fn detect<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        Option<crate::runner::RunningAgentContainer>,
                        crate::runner::ProbeError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let mut filters: HashMap<String, Vec<String>> = HashMap::new();
            filters.insert(
                "label".to_string(),
                vec![format!("{}=true", BELLOWS_MANAGED_LABEL)],
            );
            // Server-side filter on `status=running` so stopped corpses
            // (the slice-7 orphan-cleanup target) do NOT trip the
            // concurrency=1 gate. The orphan-cleanup path at startup
            // is the right tool for those.
            filters.insert("status".to_string(), vec!["running".to_string()]);
            let options = ListContainersOptionsBuilder::default()
                .filters(&filters)
                .build();
            let containers = self
                .docker
                .list_containers(Some(options))
                .await
                .map_err(|e| crate::runner::ProbeError::Daemon(format!("{e}")))?;
            // Return the first match; for the concurrency=1 invariant
            // we only need to know IF any is running, not enumerate
            // all of them. If two are somehow running (a state the
            // invariant says shouldn't exist), reporting one is
            // enough for the operator to start investigating.
            for c in containers {
                let Some(id) = c.id else {
                    continue;
                };
                let inspect_options = InspectContainerOptionsBuilder::default()
                    .size(false)
                    .build();
                let details = match self
                    .docker
                    .inspect_container(&id, Some(inspect_options))
                    .await
                {
                    Ok(details) => details,
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => continue,
                    Err(e) => return Err(crate::runner::ProbeError::Daemon(format!("{e}"))),
                };
                let started_at = started_at_from_inspect(&id, details)?;
                return Ok(Some(crate::runner::RunningAgentContainer {
                    container_id: id,
                    started_at,
                }));
            }
            Ok(None)
        })
    }
}

fn started_at_from_inspect(
    container_id: &str,
    details: bollard::models::ContainerInspectResponse,
) -> Result<chrono::DateTime<chrono::Utc>, crate::runner::ProbeError> {
    let raw_started_at = details
        .state
        .and_then(|state| state.started_at)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            crate::runner::ProbeError::Daemon(format!(
                "docker inspect for container {container_id} did not include State.StartedAt",
            ))
        })?;

    chrono::DateTime::parse_from_rfc3339(&raw_started_at)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| {
            crate::runner::ProbeError::Daemon(format!(
                "docker inspect for container {container_id} returned invalid State.StartedAt `{raw_started_at}`: {e}",
            ))
        })
}

/// Force-remove stopped containers carrying the `bellows-managed=true`
/// label. Called once at `bellows run` startup, before the polling loop.
/// Containers that completed normally were already removed by their
/// own lifecycle (see `run_container`'s drop path); a stopped managed
/// container still present is an orphan from a prior bellows process
/// that didn't finish cleanup.
///
/// Running managed containers are intentionally ignored here. The
/// pre-claim container probe reports them as
/// `Blocked(AgentContainerRunning)`, so startup cleanup must not remove
/// a live container before the concurrency gate can block.
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
    let filters = build_orphan_container_filter();
    let options = ListContainersOptionsBuilder::default()
        .all(true) // required for Docker to return stopped containers
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
                narrate!(log_writer,
                    "bellows: failed to remove orphan container {} ({e})",
                    info.short_id,
                );
            }
        }
    }
    Ok(success_lines)
}

/// Kind of Bellows-managed cache volume discovered by `bellows prune`.
/// Cache volumes are the only thing prune touches; the credentials volume
/// does NOT match any of these variants (it lacks `bellows-volume-kind`),
/// so a label-filter discovery cannot reach it by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheVolumeKind {
    /// Per-repo `target/` cache. Slug is the same value used in the
    /// volume name (`bellows-target-<slug>`) and in `--target <slug>`.
    Target { repo_slug: String },
    /// Shared cargo registry cache (`bellows-cargo-registry`). One per host.
    CargoRegistry,
}

/// One Bellows-managed cache volume, as surfaced by `bellows prune`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheVolume {
    pub name: String,
    pub kind: CacheVolumeKind,
    /// Size in bytes if the Docker daemon reported it (volumes API
    /// returns `UsageData` only when called against `/system/df`-style
    /// queries; for a label-filtered `list_volumes` it's typically
    /// absent). `None` when not available.
    pub size_bytes: Option<i64>,
}

/// Build the bollard list-volumes label filter for finding every
/// Bellows-managed volume. Returns one literal label predicate
/// (`bellows-managed=true`) so the daemon does the matching; the
/// post-filter in `classify_volume_from_labels` then drops anything
/// that isn't a cache volume (e.g. the credentials volume).
///
/// Pure function so the filter shape is unit-testable without docker.
fn build_cache_volume_list_filter() -> HashMap<String, Vec<String>> {
    let mut filters: HashMap<String, Vec<String>> = HashMap::new();
    filters.insert(
        "label".to_string(),
        vec![format!("{}=true", BELLOWS_MANAGED_LABEL)],
    );
    filters
}

/// Classify a volume by its labels into one of the prune-eligible
/// `CacheVolumeKind` variants. Returns `None` when the volume is
/// Bellows-managed but is NOT a cache volume — most importantly the
/// credentials volume, which carries no `bellows-volume-kind` label
/// and therefore cannot be touched by `prune`. Also returns `None`
/// for any unknown `bellows-volume-kind` value, future-proofing
/// against a new kind being added later without prune learning to
/// handle it.
///
/// Pure function so the classification logic is unit-testable without
/// docker.
pub fn classify_volume_from_labels(
    labels: &HashMap<String, String>,
) -> Option<CacheVolumeKind> {
    let kind = labels.get(VOLUME_KIND_LABEL)?;
    match kind.as_str() {
        VOLUME_KIND_TARGET => {
            let slug = labels.get(REPO_SLUG_LABEL)?.clone();
            Some(CacheVolumeKind::Target { repo_slug: slug })
        }
        VOLUME_KIND_CARGO_REGISTRY => Some(CacheVolumeKind::CargoRegistry),
        _ => None,
    }
}

/// List every Bellows-managed cache volume on the host.
///
/// Uses the `bellows-managed=true` server-side label filter so the
/// daemon does the bulk match, then post-filters with
/// `classify_volume_from_labels` to keep ONLY volumes that carry a
/// `bellows-volume-kind=target|cargo-registry` label. This two-step
/// shape is the credentials-volume guard: even if a future credentials
/// volume picks up `bellows-managed=true` (it does not today), it has
/// no `bellows-volume-kind`, so it will be dropped here.
///
/// Volumes returned with `size_bytes = None` are normal — the `/volumes`
/// list endpoint typically does not populate `UsageData` (only the
/// `/system/df` summary does), so prune's table omits the column when
/// the daemon didn't report it.
pub async fn list_cache_volumes(docker: &Docker) -> Result<Vec<CacheVolume>, SandboxError> {
    let filters = build_cache_volume_list_filter();
    let options = ListVolumesOptionsBuilder::default().filters(&filters).build();
    let response = docker.list_volumes(Some(options)).await?;

    let mut out = Vec::new();
    for v in response.volumes.unwrap_or_default() {
        if let Some(kind) = classify_volume_from_labels(&v.labels) {
            out.push(CacheVolume {
                name: v.name,
                kind,
                size_bytes: v.usage_data.map(|u| u.size).filter(|s| *s >= 0),
            });
        }
    }
    Ok(out)
}

/// Remove a single Docker volume by name. Maps the two failure modes
/// the operator might hit into concrete `SandboxError` variants:
/// `VolumeNotFound` for a 404 (operator typed a slug that doesn't
/// exist) and `VolumeInUse` for a 409 (some container still has it
/// mounted — shouldn't happen with concurrency=1, but worth surfacing
/// clearly rather than as a generic docker error).
pub async fn remove_cache_volume(docker: &Docker, name: &str) -> Result<(), SandboxError> {
    let options = RemoveVolumeOptionsBuilder::default().force(false).build();
    match docker.remove_volume(name, Some(options)).await {
        Ok(()) => Ok(()),
        Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => {
            Err(SandboxError::VolumeNotFound {
                name: name.to_string(),
            })
        }
        Err(bollard::errors::Error::DockerResponseServerError { status_code: 409, .. }) => {
            Err(SandboxError::VolumeInUse {
                name: name.to_string(),
            })
        }
        Err(other) => Err(SandboxError::Bollard(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Engine;
    use chrono::{DateTime, Utc};
    use serde_json::json;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The production retry bound with the backoff collapsed, so the
    /// lifecycle tests exercise the real attempt accounting without
    /// spending the real seconds.
    fn fast_retry_policy() -> TransportRetryPolicy {
        TransportRetryPolicy {
            backoff_base: Duration::from_millis(10),
            ..TransportRetryPolicy::default()
        }
    }

    /// A wiremock-backed Docker client whose request timeout is short
    /// enough that a deliberately-delayed mock response surfaces as
    /// bollard's `RequestTimeoutError` — one of the two transport
    /// shapes observed aborting real runs.
    fn mock_docker(mock: &MockServer) -> Docker {
        Docker::connect_with_http(&mock.uri(), 1, bollard::API_DEFAULT_VERSION)
            .expect("mock Docker connection")
    }

    /// Longer than `mock_docker`'s request timeout, so any response
    /// carrying this delay is never received.
    const UNANSWERED: Duration = Duration::from_secs(30);

    async fn mount_successful_lifecycle(mock: &MockServer, container_id: &str, exit_code: i64) {
        Mock::given(method("POST"))
            .and(path(format!("/containers/{container_id}/start")))
            .respond_with(ResponseTemplate::new(204))
            .mount(mock)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/containers/{container_id}/logs")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(Vec::new()))
            .mount(mock)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/containers/{container_id}/wait")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "StatusCode": exit_code })),
            )
            .mount(mock)
            .await;
    }

    #[tokio::test]
    async fn dropped_daemon_connection_is_retried_and_the_run_continues() {
        // Issue #194. Seven runs died here: the daemon connection went
        // away moments after the phase started, before the container
        // had produced anything, and the whole run aborted. One retry
        // is all it takes for the phase to go on.
        let mock = MockServer::start().await;
        let container_id = "aaaaaaaaaaaa1111111111111111111111111111111111111111111111111111";

        // Attempt 1: the daemon never answers the create.
        Mock::given(method("POST"))
            .and(path("/containers/create"))
            .respond_with(ResponseTemplate::new(201).set_delay(UNANSWERED))
            .up_to_n_times(1)
            .expect(1)
            .mount(&mock)
            .await;
        // Attempt 2: the daemon is back.
        Mock::given(method("POST"))
            .and(path("/containers/create"))
            .respond_with(ResponseTemplate::new(201).set_body_json(
                json!({ "Id": container_id, "Warnings": [] }),
            ))
            .expect(1)
            .mount(&mock)
            .await;
        mount_successful_lifecycle(&mock, container_id, 0).await;
        Mock::given(method("DELETE"))
            .and(path(format!("/containers/{container_id}")))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&mock)
            .await;

        let mut log = Vec::new();
        let outcome = run_container(
            &mock_docker(&mock),
            ContainerCreateBody::default(),
            &mut log,
            CaptureMode::Full,
            None,
            None,
            fast_retry_policy(),
        )
        .await
        .expect("a transport failure must not abort the run");

        assert_eq!(outcome.exit_code, 0);
        let log = String::from_utf8(log).expect("log is utf8");
        assert!(
            log.contains("(attempt 1/3)"),
            "the retry must be visible in the run log: {log}",
        );
    }

    #[tokio::test]
    async fn an_ambiguous_start_never_creates_a_replacement() {
        // Once create returned an id, a lost start response is
        // ambiguous: the daemon may have accepted it and the workload
        // may already be mutating the workspace. Even when cleanup also
        // fails, bellows must surface the infrastructure error rather
        // than create a replacement and execute the workload twice.
        let mock = MockServer::start().await;
        let container_id = "cccccccccccc3333333333333333333333333333333333333333333333333333";

        Mock::given(method("POST"))
            .and(path("/containers/create"))
            .respond_with(ResponseTemplate::new(201).set_body_json(
                json!({ "Id": container_id, "Warnings": [] }),
            ))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/containers/{container_id}/start")))
            .respond_with(ResponseTemplate::new(204).set_delay(UNANSWERED))
            .mount(&mock)
            .await;
        Mock::given(method("DELETE"))
            .and(path(format!("/containers/{container_id}")))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&mock)
            .await;

        let mut log = Vec::new();
        run_container(
            &mock_docker(&mock),
            ContainerCreateBody::default(),
            &mut log,
            CaptureMode::Full,
            None,
            None,
            fast_retry_policy(),
        )
        .await
        .expect_err("an ambiguous start must be surfaced, not replayed");

        let log = String::from_utf8(log).expect("log is utf8");
        assert!(
            !log.contains("retrying"),
            "post-create failures must not be narrated as replacement retries: {log}",
        );
    }

    #[tokio::test]
    async fn a_container_that_exited_non_zero_is_never_retried() {
        // The distinction the whole retry rests on. Bollard reports a
        // non-zero container exit as DockerContainerWaitError, which
        // *looks* like an error and is in fact a verdict about the code
        // under test. Retrying it would silently re-run the cargo gate
        // and re-bill the agent phase that produced it — so the
        // lifecycle is attempted exactly once and the exit code comes
        // back as data.
        let mock = MockServer::start().await;
        let container_id = "bbbbbbbbbbbb2222222222222222222222222222222222222222222222222222";

        Mock::given(method("POST"))
            .and(path("/containers/create"))
            .respond_with(ResponseTemplate::new(201).set_body_json(
                json!({ "Id": container_id, "Warnings": [] }),
            ))
            .expect(1) // exactly one attempt — verified on drop
            .mount(&mock)
            .await;
        mount_successful_lifecycle(&mock, container_id, 1).await;
        Mock::given(method("DELETE"))
            .and(path(format!("/containers/{container_id}")))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&mock)
            .await;

        let mut log = Vec::new();
        let outcome = run_container(
            &mock_docker(&mock),
            ContainerCreateBody::default(),
            &mut log,
            CaptureMode::Full,
            None,
            None,
            fast_retry_policy(),
        )
        .await
        .expect("a non-zero exit is data, not a sandbox error");

        assert_eq!(outcome.exit_code, 1, "the verdict must reach the caller");
        let log = String::from_utf8(log).expect("log is utf8");
        assert!(
            !log.contains("attempt"),
            "a failing container must not be narrated as a transport retry: {log}",
        );
    }

    #[tokio::test]
    async fn exhausted_transport_retries_surface_the_original_error_unchanged() {
        // A daemon that is down stays down: bellows tries the bound,
        // narrates every attempt, and then hands the operator the
        // daemon's own error rather than a bellows wrapper around it.
        // Pointed at a closed port, so every attempt fails at the
        // transport with no timing dependency.
        let closed_addr = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
            listener.local_addr().expect("addr")
            // dropped here — nothing is listening on that port now
        };
        let docker = Docker::connect_with_http(
            &format!("http://{closed_addr}"),
            1,
            bollard::API_DEFAULT_VERSION,
        )
        .expect("client construction does not connect");

        let mut log = Vec::new();
        let err = run_container(
            &docker,
            ContainerCreateBody::default(),
            &mut log,
            CaptureMode::Full,
            None,
            None,
            fast_retry_policy(),
        )
        .await
        .expect_err("an unreachable daemon must still fail the run");

        let SandboxError::Bollard(inner) = &err else {
            panic!("expected the bollard error to survive the retry loop, got {err:?}");
        };
        assert!(
            is_transport_failure(inner),
            "an unreachable daemon must be classified as a transport failure: {inner:?}",
        );
        assert_eq!(
            err.to_string(),
            format!("docker: {inner}"),
            "the error chain the operator sees must be unchanged by retrying",
        );

        let log = String::from_utf8(log).expect("log is utf8");
        for attempt in 1..=3 {
            assert!(
                log.contains(&format!("(attempt {attempt}/3)")),
                "attempt {attempt} must be visible in the run log: {log}",
            );
        }
        assert!(
            log.to_lowercase().contains("retries exhausted"),
            "the operator must see the bound was reached: {log}",
        );
    }

    #[test]
    fn the_daemon_client_bounds_how_long_a_wedged_daemon_can_stall_a_request() {
        // Issue #194. The retry can only start once the failing request
        // gives up, so the client-level timeout is what turns "the
        // daemon stopped answering" into a retryable error in bounded
        // time instead of an open-ended wait. Bollard's own default is
        // two minutes; bellows asks for less.
        // Built against a URL rather than via `connect_docker` because
        // the container this gate runs in has no docker socket;
        // `connect_docker` is that same call plus this one line.
        let docker = with_daemon_timeout(
            Docker::connect_with_http("http://127.0.0.1:2375", 120, bollard::API_DEFAULT_VERSION)
                .expect("client construction does not connect"),
        );

        assert_eq!(docker.timeout(), DAEMON_REQUEST_TIMEOUT);
        assert!(
            DAEMON_REQUEST_TIMEOUT < Duration::from_secs(120),
            "the point of setting a timeout is to be tighter than bollard's default",
        );
    }

    #[tokio::test]
    async fn retries_are_not_attempted_once_the_wall_clock_budget_is_spent() {
        // The retry allowance is spent inside the phase's wall-clock
        // budget, never on top of it: a phase with a millisecond left
        // fails immediately rather than buying three more attempts and
        // two backoffs of runway.
        let closed_addr = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
            listener.local_addr().expect("addr")
        };
        let docker = Docker::connect_with_http(
            &format!("http://{closed_addr}"),
            1,
            bollard::API_DEFAULT_VERSION,
        )
        .expect("client construction does not connect");

        let mut log = Vec::new();
        run_container(
            &docker,
            ContainerCreateBody::default(),
            &mut log,
            CaptureMode::Full,
            Some(Duration::from_millis(1)),
            None,
            fast_retry_policy(),
        )
        .await
        .expect_err("an unreachable daemon still fails");

        let log = String::from_utf8(log).expect("log is utf8");
        assert!(
            log.contains("(attempt 1/3)") && !log.contains("(attempt 2/3)"),
            "the first failure must end the sequence when the budget is spent: {log}",
        );
        assert!(
            log.contains("wall-clock"),
            "the operator must see that the clock, not the daemon, stopped the retries: {log}",
        );
    }

    #[tokio::test]
    async fn delayed_create_cannot_outlive_the_wall_clock_budget() {
        // The phase deadline covers daemon setup too. In particular, it
        // must beat the Docker client's independent request timeout when
        // a connected daemon accepts create but does not answer it.
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/containers/create"))
            .respond_with(ResponseTemplate::new(201).set_delay(Duration::from_secs(2)))
            .expect(1)
            .mount(&mock)
            .await;

        let started = std::time::Instant::now();
        let mut log = Vec::new();
        run_container(
            &mock_docker(&mock),
            ContainerCreateBody::default(),
            &mut log,
            CaptureMode::Full,
            Some(Duration::from_millis(50)),
            None,
            fast_retry_policy(),
        )
        .await
        .expect_err("a create that exceeds the phase budget must fail");

        assert!(
            started.elapsed() < Duration::from_millis(500),
            "the phase budget must win before the Docker request timeout"
        );
        let log = String::from_utf8(log).expect("log is utf8");
        assert!(
            log.contains("(attempt 1/3)") && !log.contains("(attempt 2/3)"),
            "an expired phase must not retry its timed-out create: {log}",
        );
    }

    #[test]
    fn retry_log_line_carries_the_attempt_number_the_bound_and_the_cause() {
        // A genuinely sick daemon must read as repeated retries in
        // bellows.log, not as a long silence — so the line names the
        // attempt, the bound, how long bellows is about to wait, and
        // the error verbatim.
        let err = SandboxError::Bollard(bollard::errors::Error::IOError {
            err: std::io::Error::other("error reading a body from connection"),
        });
        let line = format_daemon_retry_log(1, 3, &err, Duration::from_secs(2));

        assert!(line.contains("1/3"), "missing attempt/bound: {line}");
        assert!(
            line.contains("error reading a body from connection"),
            "missing the cause: {line}",
        );
        assert!(line.contains("2s"), "missing the backoff: {line}");
        assert!(line.contains("docker"), "not greppable as docker: {line}");
    }

    #[test]
    fn retry_stop_line_says_which_bound_was_hit() {
        let err = SandboxError::Bollard(bollard::errors::Error::RequestTimeoutError);

        let attempts = format_daemon_retry_stop_log(3, 3, &err, RetryStop::AttemptsExhausted);
        assert!(attempts.contains("3/3"), "missing attempt/bound: {attempts}");
        assert!(
            attempts.to_lowercase().contains("exhaust"),
            "an operator must see the bound was hit: {attempts}",
        );

        let budget = format_daemon_retry_stop_log(1, 3, &err, RetryStop::BudgetExhausted);
        assert!(
            budget.contains("wall-clock"),
            "budget stop must name the wall-clock budget, not the attempt bound: {budget}",
        );
    }

    #[test]
    fn transport_retry_backs_off_between_attempts_and_stops_at_the_bound() {
        // Issue #194. A daemon that dropped one connection is usually
        // fine on the next try, so the first backoff is short; a daemon
        // that keeps dropping gets a little more room before the last
        // attempt. The bound is what stops a genuinely sick daemon from
        // spinning forever.
        let policy = TransportRetryPolicy::default();
        assert_eq!(
            next_retry_delay(1, policy, None, Duration::ZERO),
            Ok(DAEMON_TRANSPORT_BACKOFF_BASE),
        );
        assert_eq!(
            next_retry_delay(2, policy, None, Duration::ZERO),
            Ok(DAEMON_TRANSPORT_BACKOFF_BASE * 2),
        );
        assert_eq!(
            next_retry_delay(policy.max_attempts, policy, None, Duration::ZERO),
            Err(RetryStop::AttemptsExhausted),
            "the last attempt must not schedule another one",
        );
    }

    #[test]
    fn transport_retry_never_outlives_the_wall_clock_budget() {
        // The retry budget is spent *inside* the container run's
        // wall-clock budget, not on top of it: a phase with seconds
        // left does not get three more attempts plus their backoffs.
        let policy = TransportRetryPolicy::default();
        let budget = Some(Duration::from_secs(60));

        assert_eq!(
            next_retry_delay(1, policy, budget, Duration::from_secs(10)),
            Ok(DAEMON_TRANSPORT_BACKOFF_BASE),
            "plenty of budget left — retry normally",
        );
        assert_eq!(
            next_retry_delay(1, policy, budget, Duration::from_secs(59)),
            Err(RetryStop::BudgetExhausted),
            "the backoff alone would overrun the budget",
        );
        assert_eq!(
            next_retry_delay(1, policy, budget, Duration::from_secs(120)),
            Err(RetryStop::BudgetExhausted),
            "already over budget — no retry at any price",
        );
    }

    #[test]
    fn remaining_budget_shrinks_each_attempts_deadline() {
        // Each retried attempt inherits what is *left* of the budget,
        // so three attempts of a 60s-budget phase still total 60s.
        assert_eq!(
            remaining_budget(Some(Duration::from_secs(60)), Duration::from_secs(25)),
            Some(Duration::from_secs(35)),
        );
        assert_eq!(
            remaining_budget(Some(Duration::from_secs(60)), Duration::from_secs(90)),
            Some(Duration::ZERO),
            "an overrun clamps to zero rather than wrapping",
        );
        assert_eq!(
            remaining_budget(None, Duration::from_secs(90)),
            None,
            "an unbudgeted run stays unbudgeted across retries",
        );
    }

    #[test]
    fn build_agent_env_surfaces_env_file_errors_without_panicking() {
        let dir = TempDir::new().unwrap();
        let auth = Auth::EnvFile {
            engine: Engine::Opencode,
            model: None,
            env_file_path: dir.path().join("missing.env"),
        };

        let err = build_agent_env(42, &auth).expect_err("missing env-file must error");

        assert!(matches!(err, SandboxError::AuthEnv(_)));
    }

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
    fn orphan_cleanup_filter_targets_stopped_containers_only() {
        // Startup cleanup must not delete a still-running managed
        // container before the pre-claim gate can report it as
        // Blocked(AgentContainerRunning). Docker exposes stopped
        // containers to list filters as status=exited.
        let filter = build_orphan_container_filter();

        let label_values = filter.get("label").expect("label key required");
        assert_eq!(
            label_values,
            &vec!["bellows-managed=true".to_string()],
            "cleanup must stay scoped to bellows-managed containers: {:?}",
            label_values,
        );

        let status_values = filter.get("status").expect("status key required");
        assert!(
            status_values.iter().any(|v| v == "exited"),
            "cleanup must target stopped containers: {:?}",
            status_values,
        );
        assert!(
            !status_values.iter().any(|v| v == "running"),
            "cleanup must not include running containers: {:?}",
            status_values,
        );
    }

    #[test]
    fn orphan_cleanup_filter_also_targets_never_started_containers() {
        // Issue #194. When a create request is answered by the daemon
        // but the *response* is lost to a dropped connection, bellows
        // never learns the container's id, so the retry cannot remove
        // it — it is left sitting in `created`, never started, never
        // exited. That is the one orphan shape the retry itself cannot
        // reconcile, so the startup sweep has to.
        let filter = build_orphan_container_filter();
        let status_values = filter.get("status").expect("status key required");

        assert!(
            status_values.iter().any(|v| v == "created"),
            "a container created by an attempt whose connection dropped \
             would never be swept: {status_values:?}",
        );
    }

    #[tokio::test]
    async fn docker_container_probe_reports_inspect_state_started_at() {
        // Docker's list summary `Created` is the container creation
        // time, not the process start time surfaced by inspect's
        // `State.StartedAt`. The status contract promises the latter.
        let mock = MockServer::start().await;
        let container_id =
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let created_at: DateTime<Utc> =
            DateTime::parse_from_rfc3339("2026-05-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc);
        let started_at: DateTime<Utc> =
            DateTime::parse_from_rfc3339("2026-05-17T10:30:00.123456789Z")
                .unwrap()
                .with_timezone(&Utc);

        Mock::given(method("GET"))
            .and(path("/containers/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "Id": container_id,
                    "Created": created_at.timestamp(),
                    "Labels": { "bellows-managed": "true" },
                    "State": "running"
                }
            ])))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("GET"))
            .and(path(format!("/containers/{container_id}/json")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Id": container_id,
                "Created": created_at.to_rfc3339(),
                "State": {
                    "Status": "running",
                    "Running": true,
                    "StartedAt": started_at.to_rfc3339()
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let docker = Docker::connect_with_http(&mock.uri(), 5, bollard::API_DEFAULT_VERSION)
            .expect("mock Docker connection");
        let probe = DockerContainerProbe { docker };

        let detected = <DockerContainerProbe as crate::runner::AgentContainerProbe>::detect(&probe)
            .await
            .expect("probe should read mocked Docker responses")
            .expect("mocked running container should be detected");

        assert_eq!(detected.container_id, container_id);
        assert_eq!(
            detected.started_at, started_at,
            "probe must report inspect State.StartedAt, not list Created",
        );
        assert_ne!(detected.started_at, created_at);
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
        let labels = build_managed_labels("run-uuid-here", 42, "marad2001/test-repo", None);
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
            build_managed_labels("run-uuid", 42, "marad2001/test-repo", Some("cargo-checks-gate"));
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
    fn build_managed_labels_includes_bellows_repo_label_for_cross_repo_disambiguation() {
        // Issue #35 acceptance criterion: every spawned container must
        // carry `bellows-repo=<owner>/<name>` so the kill path can tell
        // repo A's issue #42 from repo B's issue #42. Pin both the agent
        // and cargo-checks shapes — the label is in the SAME position on
        // both kinds of container because the kill filter doesn't care
        // which one it's looking at.
        let agent = build_managed_labels("run-uuid", 42, "marad2001/repo-a", None);
        assert_eq!(
            agent.get("bellows-repo").map(String::as_str),
            Some("marad2001/repo-a"),
            "agent run container must carry bellows-repo=<owner>/<name>",
        );

        let gate = build_managed_labels(
            "run-uuid",
            42,
            "marad2001/repo-b",
            Some("cargo-checks-gate"),
        );
        assert_eq!(
            gate.get("bellows-repo").map(String::as_str),
            Some("marad2001/repo-b"),
            "cargo-checks-gate container must carry bellows-repo=<owner>/<name>",
        );
    }

    #[test]
    fn validate_deploy_keys_passes_when_every_referenced_key_is_present() {
        // Issue #69 (ADR-0002) acceptance criterion: startup validation
        // succeeds when every name listed under any `[[repo]]
        // deploy_keys` is present in the volume. The validator does
        // not care about the order of keys or about extra keys in the
        // volume that aren't referenced — only that every referenced
        // key has a file on disk.
        let repos: Vec<DeployKeyRepo> = vec![
            DeployKeyRepo {
                url: "https://github.com/marad2001/workboard-financial-advice".to_string(),
                deploy_keys: vec!["workboard-core".to_string(), "workboard-shared".to_string()],
            },
            DeployKeyRepo {
                url: "https://github.com/marad2001/bellows".to_string(),
                deploy_keys: vec![],
            },
        ];
        let present: std::collections::HashSet<String> = [
            "workboard-core",
            "workboard-shared",
            "an-extra-key-nobody-references",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let result = validate_deploy_keys_against_present(&repos, &present);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
    }

    #[test]
    fn validate_deploy_keys_fails_when_a_referenced_key_is_missing() {
        // Acceptance: refuse to start when any `[[repo]] deploy_keys`
        // references a key name that's not present in the volume. The
        // error message must name the missing key AND the repo that
        // referenced it — a generic "missing keys" error would leave
        // the operator hunting through the config to find the
        // offending [[repo]].
        let repos = vec![DeployKeyRepo {
            url: "https://github.com/marad2001/workboard-financial-advice".to_string(),
            deploy_keys: vec!["workboard-core".to_string()],
        }];
        let present: std::collections::HashSet<String> =
            ["unrelated-key"].into_iter().map(String::from).collect();
        let err = validate_deploy_keys_against_present(&repos, &present).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("workboard-core"),
            "error must name the missing key: {msg}",
        );
        assert!(
            msg.contains("workboard-financial-advice"),
            "error must name the offending repo URL: {msg}",
        );
    }

    #[test]
    fn validate_deploy_keys_lists_every_missing_reference_in_one_error() {
        // Fail-fast on the first miss is tempting, but reporting every
        // missing key in one pass lets the operator run
        // `bellows setup-deploy-keys add` for each gap in one sitting
        // rather than re-running validation N times. Pin the contract
        // here so a future "simplification" can't quietly regress it.
        let repos = vec![
            DeployKeyRepo {
                url: "https://github.com/marad2001/repo-a".to_string(),
                deploy_keys: vec!["key-a".to_string()],
            },
            DeployKeyRepo {
                url: "https://github.com/marad2001/repo-b".to_string(),
                deploy_keys: vec!["key-b".to_string(), "key-c".to_string()],
            },
        ];
        let present: std::collections::HashSet<String> = std::collections::HashSet::new();
        let err = validate_deploy_keys_against_present(&repos, &present).unwrap_err();
        let msg = format!("{err}");
        for needle in ["key-a", "key-b", "key-c", "repo-a", "repo-b"] {
            assert!(msg.contains(needle), "error must mention {needle}: {msg}");
        }
    }

    #[test]
    fn validate_deploy_keys_passes_when_no_repo_opts_in() {
        // The volume can be totally empty when no [[repo]] references
        // any deploy key. The brief: "no creds in sandbox by default"
        // means the absence of an SSH volume must not block bellows
        // from running. Validation walks every [[repo]] and finds
        // nothing to check.
        let repos = vec![
            DeployKeyRepo {
                url: "https://github.com/marad2001/repo-a".to_string(),
                deploy_keys: vec![],
            },
            DeployKeyRepo {
                url: "https://github.com/marad2001/repo-b".to_string(),
                deploy_keys: vec![],
            },
        ];
        let present: std::collections::HashSet<String> = std::collections::HashSet::new();
        let result = validate_deploy_keys_against_present(&repos, &present);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
    }

    #[test]
    fn build_ssh_keys_mount_returns_none_when_deploy_keys_empty() {
        // Issue #69 (ADR-0002) acceptance: containers spawned for
        // `[[repo]]` entries with empty or unset `deploy_keys` get no
        // SSH mount — that's how the "no creds in sandbox by default"
        // posture is preserved. The mount filter is the single
        // chokepoint enforcing this; if it ever returned a mount for an
        // empty list, every container (including bellows-on-bellows)
        // would get the keys.
        let mount = build_ssh_keys_mount("bellows-deploy-keys", &[]);
        assert!(mount.is_none(), "empty deploy_keys must produce no mount: {:?}", mount);
    }

    #[test]
    fn build_ssh_keys_mount_returns_read_only_mount_when_deploy_keys_non_empty() {
        // Issue #69 (ADR-0002) acceptance: a `[[repo]]` block that
        // declares at least one deploy key gets the configured
        // ssh_keys_volume mounted READ-ONLY at /home/bellows/.ssh/ —
        // read-only so an escaping agent cannot tamper with the keys
        // (the brief calls this out explicitly as the security
        // boundary).
        let mount = build_ssh_keys_mount("bellows-deploy-keys", &["workboard-core".to_string()])
            .expect("non-empty deploy_keys must produce a mount");
        assert_eq!(mount.typ, Some(MountType::VOLUME));
        assert_eq!(mount.source.as_deref(), Some("bellows-deploy-keys"));
        assert_eq!(mount.target.as_deref(), Some("/home/bellows/.ssh"));
        assert_eq!(
            mount.read_only,
            Some(true),
            "the deploy-keys mount must be read-only (security boundary per ADR-0002): {:?}",
            mount,
        );
    }

    #[test]
    fn build_ssh_keys_mount_honours_custom_volume_name() {
        // Acceptance criterion implication: the volume name is
        // configurable via `[auth].ssh_keys_volume`. The mount filter
        // must pipe that name through verbatim — if it hard-coded
        // "bellows-deploy-keys", an operator who renamed the volume
        // would mount the wrong one (or nothing at all if the default
        // name had no volume on the host).
        let mount =
            build_ssh_keys_mount("my-custom-keys", &["some-key".to_string()])
                .expect("non-empty deploy_keys must produce a mount");
        assert_eq!(mount.source.as_deref(), Some("my-custom-keys"));
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
    fn build_cargo_checks_env_carries_clippy_and_test_commands_from_gate_snapshot() {
        // ADR-0004 acceptance: bellows passes the snapshotted clippy
        // and test commands from the Workspace into the cargo-checks
        // container via the `BELLOWS_CLIPPY_CMD` and `BELLOWS_TEST_CMD`
        // env vars. The script reads these at the top and `eval`s
        // each one so the commands run verbatim, not via bellows's
        // hardcoded `--all-targets --all-features` defaults.
        let gc = crate::workspace::GateCommands {
            clippy: "cargo clippy --all-targets -- -D clippy::correctness".to_string(),
            clippy_source: crate::workflow_parse::Provenance::FallbackFromConfig,
            clippy_env: Vec::new(),
            test: "cargo test --features in-memory".to_string(),
            test_source: crate::workflow_parse::Provenance::FallbackFromConfig,
            test_env: Vec::new(),
        };
        let env = build_cargo_checks_env(&gc, &[]);
        assert!(
            env.iter().any(|e| e
                == "BELLOWS_CLIPPY_CMD=cargo clippy --all-targets -- -D clippy::correctness"),
            "clippy command must be set on BELLOWS_CLIPPY_CMD: {env:?}",
        );
        assert!(
            env.iter()
                .any(|e| e == "BELLOWS_TEST_CMD=cargo test --features in-memory"),
            "test command must be set on BELLOWS_TEST_CMD: {env:?}",
        );
    }

    #[test]
    fn build_cargo_checks_env_prefixes_each_command_with_its_mirrored_ci_env() {
        // Issue #180: the FA shape. The test step's linker-OOM guard
        // must reach the gate as a POSIX assignment prefix, and each
        // command carries only its OWN env (sibling clippy:/test: jobs).
        let gc = crate::workspace::GateCommands {
            clippy: "cargo clippy --all-targets -- -D clippy::correctness".to_string(),
            clippy_source: crate::workflow_parse::Provenance::ParsedFromWorkflow(
                std::path::PathBuf::from(".github/workflows/ci.yml"),
            ),
            clippy_env: vec![("CARGO_INCREMENTAL".to_string(), "0".to_string())],
            test: "cargo test --locked --workspace --lib --bins --tests --all-features".to_string(),
            test_source: crate::workflow_parse::Provenance::ParsedFromWorkflow(
                std::path::PathBuf::from(".github/workflows/ci.yml"),
            ),
            test_env: vec![("CARGO_PROFILE_TEST_DEBUG".to_string(), "0".to_string())],
        };
        let env = build_cargo_checks_env(&gc, &[]);
        assert!(
            env.iter().any(|e| e
                == "BELLOWS_TEST_CMD=CARGO_PROFILE_TEST_DEBUG='0' cargo test --locked --workspace --lib --bins --tests --all-features"),
            "test command must carry its mirrored env prefix: {env:?}",
        );
        assert!(
            env.iter().any(|e| e
                == "BELLOWS_CLIPPY_CMD=CARGO_INCREMENTAL='0' cargo clippy --all-targets -- -D clippy::correctness"),
            "clippy must carry only its own env, not the test step's: {env:?}",
        );
    }

    #[test]
    fn with_env_prefix_leaves_command_untouched_when_ci_declares_no_env() {
        // Issue #180 no-regression: repos whose CI sets no build env
        // (e.g. bellows itself) must see a byte-identical command.
        assert_eq!(
            with_env_prefix(&[], &[], "cargo test --all-features"),
            "cargo test --all-features",
        );
    }

    #[test]
    fn env_override_wins_over_a_same_named_ci_mirrored_value() {
        // Issue #186: the OOM retry forces CARGO_BUILD_JOBS=1. A target
        // repo whose CI sets its own value must not be able to undo it
        // via #180's mirroring — otherwise the retry links in parallel
        // again and OOMs a second time.
        let ci = vec![
            ("CARGO_BUILD_JOBS".to_string(), "8".to_string()),
            ("CARGO_PROFILE_TEST_DEBUG".to_string(), "0".to_string()),
        ];
        let overrides = vec![("CARGO_BUILD_JOBS".to_string(), "1".to_string())];
        let rendered = with_env_prefix(&ci, &overrides, "cargo test --workspace");
        assert_eq!(
            rendered,
            "CARGO_BUILD_JOBS='1' CARGO_PROFILE_TEST_DEBUG='0' cargo test --workspace",
            "override must win, and exactly one assignment per name",
        );
        assert!(
            !rendered.contains("CARGO_BUILD_JOBS='8'"),
            "the CI value must not survive alongside the override: {rendered}",
        );
    }

    #[test]
    fn env_override_applies_even_when_ci_declares_no_env() {
        // The common case for the retry: bellows itself, or any repo
        // with no CI build env, still gets serialised linking.
        let overrides = vec![("CARGO_BUILD_JOBS".to_string(), "1".to_string())];
        assert_eq!(
            with_env_prefix(&[], &overrides, "cargo test"),
            "CARGO_BUILD_JOBS='1' cargo test",
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
    fn build_cache_volume_list_filter_uses_bellows_managed_label_only() {
        // The list filter is the first half of prune's discovery: it
        // asks the daemon for every Bellows-managed volume. The second
        // half — `classify_volume_from_labels` — drops anything that
        // isn't a cache volume. Pin the filter shape so a future edit
        // doesn't accidentally widen it (e.g. dropping `=true` and
        // catching unrelated labels).
        let filter = build_cache_volume_list_filter();
        let label_values = filter.get("label").expect("label key required");
        assert_eq!(
            label_values,
            &vec!["bellows-managed=true".to_string()],
            "list filter must scope to bellows-managed=true only: {:?}",
            label_values,
        );
    }

    #[test]
    fn classify_volume_from_labels_returns_target_with_repo_slug() {
        // Per-repo target volume: kind=target + repo-slug=<slug>.
        let mut labels = HashMap::new();
        labels.insert("bellows-managed".to_string(), "true".to_string());
        labels.insert("bellows-volume-kind".to_string(), "target".to_string());
        labels.insert(
            "bellows-repo-slug".to_string(),
            "marad2001-bellows".to_string(),
        );
        let kind = classify_volume_from_labels(&labels);
        assert_eq!(
            kind,
            Some(CacheVolumeKind::Target {
                repo_slug: "marad2001-bellows".to_string(),
            }),
        );
    }

    #[test]
    fn classify_volume_from_labels_returns_cargo_registry_for_registry_kind() {
        // Shared cargo registry: kind=cargo-registry, no repo slug.
        let mut labels = HashMap::new();
        labels.insert("bellows-managed".to_string(), "true".to_string());
        labels.insert(
            "bellows-volume-kind".to_string(),
            "cargo-registry".to_string(),
        );
        let kind = classify_volume_from_labels(&labels);
        assert_eq!(kind, Some(CacheVolumeKind::CargoRegistry));
    }

    #[test]
    fn classify_volume_from_labels_returns_none_for_credentials_volume() {
        // Acceptance criterion from the brief: the credentials volume
        // is NEVER touched by prune. The credentials volume today
        // carries no `bellows-volume-kind` label at all (and even if a
        // future revision tags it `bellows-managed=true`, it would
        // still lack the kind). `classify_volume_from_labels` returns
        // None for that shape — so the discovery pipeline drops the
        // credentials volume before any removal can happen.
        let mut labels = HashMap::new();
        labels.insert("bellows-managed".to_string(), "true".to_string());
        // Deliberately no bellows-volume-kind.
        assert_eq!(classify_volume_from_labels(&labels), None);

        // Sanity check the inverse: an empty labels map also classifies
        // as None (an unrelated docker volume picked up by some other
        // filter would land here too).
        assert_eq!(classify_volume_from_labels(&HashMap::new()), None);
    }

    #[test]
    fn classify_volume_from_labels_returns_none_for_unknown_kind() {
        // Future-proofing: a new kind value that prune does not know
        // about must not be classified as a cache volume. Tomorrow's
        // bellows-volume-kind=workspace would otherwise be silently
        // removed by `--all`.
        let mut labels = HashMap::new();
        labels.insert("bellows-managed".to_string(), "true".to_string());
        labels.insert(
            "bellows-volume-kind".to_string(),
            "some-future-kind".to_string(),
        );
        assert_eq!(classify_volume_from_labels(&labels), None);
    }

    #[test]
    fn classify_volume_from_labels_returns_none_when_target_missing_repo_slug() {
        // Defensive: a target volume must carry its repo-slug for prune
        // to render the row. A malformed volume (kind=target but no
        // slug) classifies as not-a-cache-volume so prune ignores it
        // rather than panicking on a missing label.
        let mut labels = HashMap::new();
        labels.insert("bellows-volume-kind".to_string(), "target".to_string());
        assert_eq!(classify_volume_from_labels(&labels), None);
    }

    #[test]
    fn build_issue_container_filter_uses_managed_repo_and_issue_number() {
        // Used by find_containers_for_issue. The filter must restrict to
        // bellows-managed containers AND scope to BOTH the repo and the
        // requested issue number — otherwise a kill in a multi-repo
        // config could remove the wrong repo's container when issue
        // numbers collide. Issue #35 acceptance criterion.
        let filter = build_issue_container_filter("marad2001/test-repo", 42);
        let label_values = filter.get("label").expect("label key required");
        assert!(
            label_values.iter().any(|v| v == "bellows-managed=true"),
            "filter must include bellows-managed=true: {:?}",
            label_values,
        );
        assert!(
            label_values
                .iter()
                .any(|v| v == "bellows-repo=marad2001/test-repo"),
            "filter must include bellows-repo=<owner>/<name>: {:?}",
            label_values,
        );
        assert!(
            label_values.iter().any(|v| v == "bellows-issue-number=42"),
            "filter must include bellows-issue-number=N: {:?}",
            label_values,
        );
    }

    #[test]
    fn build_issue_container_filter_distinguishes_same_issue_number_across_repos() {
        // The cross-repo collision case the new filter is designed to
        // prevent: issue #42 in repo A vs issue #42 in repo B. The filter
        // values must differ on the bellows-repo predicate even when the
        // issue number is identical.
        let filter_a = build_issue_container_filter("marad2001/repo-a", 42);
        let filter_b = build_issue_container_filter("marad2001/repo-b", 42);
        let labels_a = filter_a.get("label").unwrap();
        let labels_b = filter_b.get("label").unwrap();
        assert!(labels_a.iter().any(|v| v == "bellows-repo=marad2001/repo-a"));
        assert!(labels_b.iter().any(|v| v == "bellows-repo=marad2001/repo-b"));
        assert_ne!(labels_a, labels_b);
    }
}
