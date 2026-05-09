use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountType};
use bollard::query_parameters::LogsOptionsBuilder;
use bollard::Docker;
use sha2::{Digest, Sha256};
use tokio_stream::StreamExt;
use uuid::Uuid;

use crate::workspace::Workspace;

const POLICY_IMAGE_DIR: &str = "policy-image";

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("docker daemon not reachable: {0}")]
    Bollard(#[from] bollard::errors::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("docker build failed (status {0})")]
    ImageBuildFailed(std::process::ExitStatus),
    #[error("docker images failed (status {0})")]
    ImageQueryFailed(std::process::ExitStatus),
    #[error("container exited non-zero (status code {0})")]
    ContainerNonZeroExit(i64),
    #[error("policy-image dir not found at {0}")]
    PolicyImageMissing(String),
}

pub async fn run_agent(
    workspace: &Workspace,
    issue_number: u64,
    log_writer: &mut dyn Write,
) -> Result<(), SandboxError> {
    let hash = compute_image_hash()?;
    let image_tag = format!("bellows-policy:{}", &hash[..12]);

    ensure_image_built(&hash, &image_tag).await?;

    let docker = Docker::connect_with_local_defaults()?;
    let run_id = Uuid::new_v4().to_string();

    // tempfile gives an absolute path already; canonicalize() on Windows
    // returns `\\?\C:\...` extended-length paths that Docker Desktop's
    // bind-mount handler rejects, so we use the path as-is.
    let workspace_path = workspace.path().to_string_lossy().to_string();

    let mut labels = HashMap::new();
    labels.insert("bellows-managed".to_string(), "true".to_string());
    labels.insert("bellows-run-id".to_string(), run_id.clone());

    let env = vec![format!("BELLOWS_ISSUE_NUMBER={issue_number}")];

    // Use the structured Mount API instead of `binds: Vec<String>` so the
    // host source path doesn't collide with bind syntax's `:` separator
    // on Windows (drive letters like `C:\...`).
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
        env: Some(env),
        labels: Some(labels),
        host_config: Some(host_config),
        ..Default::default()
    };

    let container = docker.create_container(None, config).await?;
    let id = container.id;

    docker.start_container(&id, None).await?;

    let log_options = LogsOptionsBuilder::default()
        .follow(true)
        .stdout(true)
        .stderr(true)
        .build();
    let mut log_stream = docker.logs(&id, Some(log_options));
    while let Some(frame) = log_stream.next().await {
        let frame = frame?;
        let bytes = match frame {
            bollard::container::LogOutput::StdOut { message } => message,
            bollard::container::LogOutput::StdErr { message } => message,
            _ => continue,
        };
        log_writer.write_all(&bytes)?;
        log_writer.flush()?;
    }

    let mut wait_stream = docker.wait_container(&id, None);
    let mut exit_code = 0i64;
    while let Some(result) = wait_stream.next().await {
        match result {
            Ok(response) => exit_code = response.status_code,
            Err(e) => return Err(e.into()),
        }
    }

    docker.remove_container(&id, None).await?;

    if exit_code != 0 {
        return Err(SandboxError::ContainerNonZeroExit(exit_code));
    }

    Ok(())
}

fn compute_image_hash() -> Result<String, SandboxError> {
    let dir = Path::new(POLICY_IMAGE_DIR);
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
        let rel = path.strip_prefix(dir).unwrap_or(path);
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
