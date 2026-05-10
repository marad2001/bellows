use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountType};
use bollard::query_parameters::{LogsOptionsBuilder, RemoveContainerOptionsBuilder};
use bollard::Docker;
use sha2::{Digest, Sha256};
use tokio_stream::StreamExt;
use uuid::Uuid;

use crate::auth::Auth;
use crate::policy::{CheckResult, GateOutcome};
use crate::workspace::Workspace;

const POLICY_IMAGE_DIR: &str = "policy-image";

/// How many bytes of agent stdout/stderr to retain for the failure log
/// comment. Streaming to the log_writer is unaffected — this is a tee
/// for the post-run summary, not a cap on what's written.
const OUTPUT_TAIL_CAP_BYTES: usize = 64 * 1024;

/// Outcome of a finished agent run. Carries the container exit code so
/// the runner can pass it to `policy::classify_exit`, and a tail of the
/// container's stdout/stderr for embedding in failure log comments.
#[derive(Debug, Clone)]
pub struct AgentRun {
    pub exit_code: i64,
    pub stderr_tail: String,
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
}

/// Run a container through its full lifecycle: create, start, stream
/// stdout/stderr to `log_writer` while capturing per `capture_mode`,
/// wait for exit, force-remove. Container is removed even on error.
///
/// Non-zero container exit is returned as `exit_code`, NOT as a sandbox
/// error — the caller (run_agent / run_cargo_test) and ultimately
/// policy::classify_exit decide what a non-zero exit means.
async fn run_container(
    docker: &Docker,
    config: ContainerCreateBody,
    log_writer: &mut dyn Write,
    capture_mode: CaptureMode,
) -> Result<ContainerOutcome, SandboxError> {
    let container = docker.create_container(None, config).await?;
    let id = container.id;

    // Once the container exists on the daemon it must be removed even if
    // start/log/wait fail. Run the lifecycle inside an inner async block
    // and force-remove unconditionally afterwards.
    let lifecycle: Result<ContainerOutcome, SandboxError> = async {
        docker.start_container(&id, None).await?;

        let log_options = LogsOptionsBuilder::default()
            .follow(true)
            .stdout(true)
            .stderr(true)
            .build();
        let mut log_stream = docker.logs(&id, Some(log_options));
        let mut captured = Captured::new(capture_mode);
        while let Some(frame) = log_stream.next().await {
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
    log_writer: &mut dyn Write,
) -> Result<AgentRun, SandboxError> {
    let image_tag = ensure_policy_image().await?;

    let docker = Docker::connect_with_local_defaults()?;
    let run_id = Uuid::new_v4().to_string();

    // tempfile gives an absolute path already; canonicalize() on Windows
    // returns `\\?\C:\...` extended-length paths that Docker Desktop's
    // bind-mount handler rejects, so we use the path as-is.
    let workspace_path = workspace.path().to_string_lossy().to_string();

    let mut labels = HashMap::new();
    labels.insert("bellows-managed".to_string(), "true".to_string());
    labels.insert("bellows-run-id".to_string(), run_id);

    let mut env = vec![format!("BELLOWS_ISSUE_NUMBER={issue_number}")];
    env.extend(auth.extra_env());

    // Structured Mount API rather than `binds: Vec<String>` to avoid
    // collision with bind syntax's `:` separator on Windows drive
    // letters. Auth contributes any credentials/cache volumes it needs.
    let mut mounts = vec![Mount {
        target: Some("/workspace".to_string()),
        source: Some(workspace_path),
        typ: Some(MountType::BIND),
        ..Default::default()
    }];
    mounts.extend(auth.extra_mounts());

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
    )
    .await?;

    Ok(AgentRun {
        exit_code: outcome.exit_code,
        stderr_tail: outcome.captured,
    })
}

/// Workspace-side files written by the policy image's `run-cargo-checks`
/// script. The runner reads these after the container exits so it can
/// attribute clippy / test failures separately, then removes them so
/// they don't end up in any subsequent commit.
const CARGO_CLIPPY_OUTPUT_FILE: &str = ".bellows-cargo-clippy-output";
const CARGO_TEST_OUTPUT_FILE: &str = ".bellows-cargo-test-output";
const CARGO_CHECKS_RESULTS_FILE: &str = ".bellows-cargo-checks-results";

/// Spawn a fresh container from the policy image and run the cargo
/// checks gate: `cargo clippy --all-targets --all-features -- -D
/// warnings` followed by `cargo test`. Both run inside the same
/// container (entrypoint overridden to `run-cargo-checks`) so clippy's
/// compilation artifacts are reused by test.
///
/// Returns a `GateOutcome` carrying each check's exit code + captured
/// output. `cargo_test` is `None` when clippy failed and the gate
/// short-circuited before running tests. Either being `None` and the
/// other being `Some` with a non-zero exit signals the gate failed.
///
/// No credentials volume — the gate has no Anthropic dependency.
pub async fn run_cargo_checks(
    workspace: &Workspace,
    log_writer: &mut dyn Write,
) -> Result<GateOutcome, SandboxError> {
    let image_tag = ensure_policy_image().await?;

    let docker = Docker::connect_with_local_defaults()?;
    let run_id = Uuid::new_v4().to_string();

    let workspace_path = workspace.path().to_string_lossy().to_string();

    let mut labels = HashMap::new();
    labels.insert("bellows-managed".to_string(), "true".to_string());
    labels.insert("bellows-run-id".to_string(), run_id);
    labels.insert("bellows-purpose".to_string(), "cargo-checks-gate".to_string());

    let host_config = HostConfig {
        mounts: Some(vec![Mount {
            target: Some("/workspace".to_string()),
            source: Some(workspace_path),
            typ: Some(MountType::BIND),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let config = ContainerCreateBody {
        image: Some(image_tag),
        entrypoint: Some(vec!["/usr/local/bin/run-cargo-checks".to_string()]),
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
    let outcome = run_container(&docker, config, log_writer, CaptureMode::Full).await?;

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

    if outcome.exit_code != 0 && clippy_exit.is_none() && test_exit.is_none() {
        return Err(SandboxError::CargoChecksScriptCrashed {
            exit_code: outcome.exit_code,
        });
    }

    Ok(GateOutcome {
        cargo_clippy: clippy_exit.map(|exit_code| CheckResult {
            exit_code,
            output: clippy_output,
        }),
        cargo_test: test_exit.map(|exit_code| CheckResult {
            exit_code,
            output: test_output,
        }),
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
}
