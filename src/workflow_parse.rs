//! ADR-0004 GitHub Actions workflow parser.
//!
//! Bellows's cargo-checks gate mirrors the target repo's CI clippy /
//! test commands by reading `.github/workflows/*.yml` at workspace
//! prepare time. This module owns the read + extract step: find the
//! workflow named `CI`, locate the first `cargo clippy` and
//! `cargo test` invocations in a Linux-runner job's steps, return them
//! as complete command strings.
//!
//! Failure is always silent — a missing workflow, malformed YAML, or
//! commands wrapped in a shell script the parser can't follow all
//! produce `None` for the affected command. The caller (the
//! workspace prepare path) then falls back to operator-declared
//! `[gates].*_flags` defaults from `orchestrator.toml`. There is no
//! recoverable error type because parsing fallback is the operational
//! safety net that lets bellows keep running against any target repo.

use std::path::{Path, PathBuf};

use yaml_rust2::{Yaml, YamlLoader};

/// Commands bellows extracted from the target repo's CI workflow,
/// alongside provenance for the operator-visible run-log line.
///
/// `clippy` / `test` are `None` when bellows could not extract a
/// literal `cargo clippy ...` / `cargo test ...` line — the caller
/// substitutes a fallback from `Config.gates` for any `None` field.
///
/// `source` reports whether at least one command was extracted from a
/// workflow file (`ParsedFromWorkflow(path)`) or none were
/// (`FallbackFromConfig`). It is the file-level provenance, not the
/// per-command one; the caller can compare each field against its
/// fallback value to attribute provenance per command if needed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExtractedCommands {
    pub clippy: Option<String>,
    pub test: Option<String>,
    pub source: Provenance,
}

/// Where a gate command came from. Surfaced in the run-log line so an
/// operator reading the pipeline output can tell whether bellows
/// mirrored CI verbatim or fell back to the operator-declared default.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// Bellows extracted the command from the named workflow file.
    /// The `PathBuf` carries the actual file (e.g.
    /// `.github/workflows/ci.yml`) for log attribution.
    ParsedFromWorkflow(PathBuf),
    /// Bellows could not parse a literal command from any workflow
    /// file and the caller will substitute the operator-declared
    /// `[gates].*_flags` default.
    #[default]
    FallbackFromConfig,
}

/// Walk `repo_root/.github/workflows/*.yml` and `.yaml`, find the
/// workflow whose top-level `name:` is `CI`, and extract the first
/// `cargo clippy ...` and `cargo test ...` lines from its Linux-runner
/// job's steps. Returns `ExtractedCommands::default()` when no such
/// workflow exists, when the YAML cannot be parsed, or when no literal
/// `cargo clippy` / `cargo test` step is found.
///
/// Never errors — every failure mode is downgraded to `None` for the
/// affected command so the cargo-checks gate falls back to the
/// operator-declared `[gates].*_flags` default.
pub fn parse_ci_workflow(repo_root: &Path) -> std::io::Result<ExtractedCommands> {
    let dir = repo_root.join(".github").join("workflows");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ExtractedCommands::default());
        }
        Err(e) => return Err(e),
    };

    // Collect workflow file paths in a deterministic order so the
    // verdict doesn't flap across filesystems that don't enumerate
    // directories in a consistent order.
    let mut workflow_paths: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_yaml = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("yml") || e.eq_ignore_ascii_case("yaml"));
        if is_yaml {
            workflow_paths.push(path);
        }
    }
    workflow_paths.sort();

    for path in &workflow_paths {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let docs = match YamlLoader::load_from_str(&content) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let Some(doc) = docs.first() else { continue };
        if doc["name"].as_str() != Some("CI") {
            continue;
        }
        let (clippy, test) = extract_from_workflow(doc);
        if clippy.is_some() || test.is_some() {
            return Ok(ExtractedCommands {
                clippy,
                test,
                source: Provenance::ParsedFromWorkflow(path.clone()),
            });
        }
        // Workflow named CI but no literal cargo clippy / test line —
        // treat identically to "no workflow" so both commands fall
        // back to config.
        return Ok(ExtractedCommands::default());
    }

    Ok(ExtractedCommands::default())
}

/// Walk a parsed workflow's `jobs.*` map, pick the preferred job (a
/// job running on a Linux runner; falling back to the first declared
/// job when none is Linux), then return the first `cargo clippy` and
/// `cargo test` lines from that job's steps.
fn extract_from_workflow(doc: &Yaml) -> (Option<String>, Option<String>) {
    let Some(jobs) = doc["jobs"].as_hash() else {
        return (None, None);
    };
    let mut linux_job: Option<&Yaml> = None;
    let mut first_job: Option<&Yaml> = None;
    for (_name, body) in jobs {
        if first_job.is_none() {
            first_job = Some(body);
        }
        if linux_job.is_none() && job_is_linux(body) {
            linux_job = Some(body);
        }
    }
    let chosen = linux_job.or(first_job);
    let Some(job) = chosen else {
        return (None, None);
    };
    extract_from_job(job)
}

/// Whether a `jobs.<name>` body runs on a Linux runner. Accepts a
/// literal `runs-on: ubuntu-*` string or a `runs-on: ${{ matrix.os }}`
/// reference whose backing matrix array contains any `ubuntu-*` entry.
fn job_is_linux(job: &Yaml) -> bool {
    let runs_on = &job["runs-on"];
    if let Some(s) = runs_on.as_str() {
        if is_ubuntu_runner(s) {
            return true;
        }
        if let Some(key) = matrix_reference_key(s) {
            return matrix_axis_has_ubuntu(job, &key);
        }
        return false;
    }
    if let Some(arr) = runs_on.as_vec() {
        return arr
            .iter()
            .any(|v| v.as_str().is_some_and(is_ubuntu_runner));
    }
    false
}

fn is_ubuntu_runner(s: &str) -> bool {
    s.trim().starts_with("ubuntu")
}

/// Recognise `${{ matrix.<key> }}` interpolation in a `runs-on:`
/// scalar. Returns `<key>` if the scalar is a matrix reference, else
/// None.
fn matrix_reference_key(s: &str) -> Option<String> {
    let trimmed = s.trim();
    let inner = trimmed
        .strip_prefix("${{")?
        .strip_suffix("}}")?
        .trim();
    let key = inner.strip_prefix("matrix.")?;
    if key.is_empty() {
        return None;
    }
    Some(key.to_string())
}

/// Whether `job.strategy.matrix.<key>` is an array containing at
/// least one `ubuntu-*` string. Defensive — returns false for any
/// missing or non-array shape.
fn matrix_axis_has_ubuntu(job: &Yaml, key: &str) -> bool {
    let axis = &job["strategy"]["matrix"][key];
    let Some(arr) = axis.as_vec() else {
        return false;
    };
    arr.iter().any(|v| v.as_str().is_some_and(is_ubuntu_runner))
}

/// Walk a job's `steps` array and return the first `cargo clippy` and
/// `cargo test` lines found. Steps with non-`run` payloads (e.g.
/// `uses:` action invocations) are skipped. Multi-line `run:` bodies
/// are scanned line-by-line so a step that prefixes with `set -e` or
/// a `cargo build` doesn't suppress extraction of a later
/// `cargo clippy` line in the same step.
fn extract_from_job(job: &Yaml) -> (Option<String>, Option<String>) {
    let Some(steps) = job["steps"].as_vec() else {
        return (None, None);
    };
    let mut clippy = None;
    let mut test = None;
    for step in steps {
        let Some(run) = step["run"].as_str() else {
            continue;
        };
        for line in run.lines() {
            let trimmed = line.trim();
            if clippy.is_none()
                && let Some(cmd) = match_cargo_command(trimmed, "clippy")
            {
                clippy = Some(cmd);
            }
            if test.is_none()
                && let Some(cmd) = match_cargo_command(trimmed, "test")
            {
                test = Some(cmd);
            }
            if clippy.is_some() && test.is_some() {
                return (clippy, test);
            }
        }
    }
    (clippy, test)
}

/// Match a trimmed line against `cargo <subcommand>` and return the
/// whole line as the captured command. Returns `None` for lines that
/// embed the subcommand inside a shell wrapper (e.g.
/// `./scripts/run-clippy.sh`), inside a quoted argument, or that
/// chain another command before it (e.g. `cargo build && cargo
/// clippy ...`) — those legitimately produce `None` and the caller
/// falls back to config for that command.
fn match_cargo_command(line: &str, subcommand: &str) -> Option<String> {
    let prefix = format!("cargo {}", subcommand);
    if line == prefix {
        return Some(line.to_string());
    }
    // Require a whitespace boundary after the subcommand so e.g.
    // `cargo testify` does not match `cargo test`.
    let with_space = format!("{} ", prefix);
    if line.starts_with(&with_space) {
        return Some(line.to_string());
    }
    None
}
