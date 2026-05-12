// ADR-0004 integration tests for the parameterised
// `policy-image/run-cargo-checks` script. The script is invoked
// inside the policy image with `BELLOWS_CLIPPY_CMD` /
// `BELLOWS_TEST_CMD` env vars carrying the snapshotted gate
// commands; it must run those commands verbatim (not bellows's old
// hardcoded `cargo clippy --all-targets --all-features -- -D warnings`
// pair), tee captured output to the workspace-side files the runner
// reads, write the per-check exit codes to the results file, and
// short-circuit the test step when clippy fails.
//
// We invoke the script under `sh` with a stubbed `cargo` on PATH so
// the test exercises the actual script logic without needing Docker
// or a Rust toolchain inside the container.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn script_path() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("policy-image")
        .join("run-cargo-checks")
        .leak()
}

fn stub_cargo(bin_dir: &Path, body: &str) {
    let cargo = bin_dir.join("cargo");
    std::fs::write(&cargo, body).unwrap();
    let mut perms = std::fs::metadata(&cargo).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&cargo, perms).unwrap();
}

fn run_script(
    workspace: &Path,
    bin_dir: &Path,
    clippy_cmd: &str,
    test_cmd: &str,
) -> std::process::Output {
    // Prepend the stub-cargo dir to PATH so the stub wins over any
    // real cargo binary on the host. Inherit the rest of PATH so
    // /bin/sh remains resolvable.
    let parent_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{}", bin_dir.display(), parent_path);
    Command::new("/bin/sh")
        .arg(script_path())
        .env("BELLOWS_CLIPPY_CMD", clippy_cmd)
        .env("BELLOWS_TEST_CMD", test_cmd)
        // The script `cd`s to /workspace inside the policy image; the
        // BELLOWS_WORKSPACE env var lets the same script be exercised
        // here against an arbitrary temp dir.
        .env("BELLOWS_WORKSPACE", workspace)
        .env("PATH", path)
        .output()
        .expect("run-cargo-checks script must execute under sh")
}

#[test]
fn script_runs_clippy_command_from_env_and_test_command_after_clippy_passes() {
    // ADR-0004 acceptance: when bellows sets BELLOWS_CLIPPY_CMD and
    // BELLOWS_TEST_CMD on the container, the script eval's each one
    // VERBATIM rather than running bellows's old hardcoded flag set.
    // Both succeed in this test (stubbed cargo exits 0 with the
    // received args echoed); the script writes the captured output
    // to the workspace files and `clippy_exit=0` / `test_exit=0` to
    // the results file.
    let tmp_workspace = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    stub_cargo(
        bin_dir.path(),
        "#!/bin/sh\necho \"cargo called with: $*\"\nexit 0\n",
    );

    let output = run_script(
        tmp_workspace.path(),
        bin_dir.path(),
        "cargo clippy --all-targets -- -D clippy::correctness",
        "cargo test --features in-memory",
    );
    assert!(
        output.status.success(),
        "script must exit 0 when both checks pass; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let clippy_out =
        std::fs::read_to_string(tmp_workspace.path().join(".bellows-cargo-clippy-output")).unwrap();
    let test_out =
        std::fs::read_to_string(tmp_workspace.path().join(".bellows-cargo-test-output")).unwrap();
    let results =
        std::fs::read_to_string(tmp_workspace.path().join(".bellows-cargo-checks-results"))
            .unwrap();

    assert!(
        clippy_out.contains("clippy --all-targets -- -D clippy::correctness"),
        "clippy output must capture the actual command's argv: {clippy_out:?}",
    );
    assert!(
        test_out.contains("test --features in-memory"),
        "test output must capture the actual command's argv: {test_out:?}",
    );
    assert!(
        results.contains("clippy_exit=0"),
        "results must record clippy_exit=0: {results:?}",
    );
    assert!(
        results.contains("test_exit=0"),
        "results must record test_exit=0: {results:?}",
    );
}

#[test]
fn script_short_circuits_test_when_clippy_fails() {
    // The existing slice X1 contract is preserved: if clippy fails
    // the test step is skipped and `test_exit` is recorded as empty.
    // ADR-0004 changes the WHICH-command, not the orchestration.
    let tmp_workspace = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    stub_cargo(
        bin_dir.path(),
        // Fail on clippy invocation, succeed otherwise.
        "#!/bin/sh\nif [ \"$1\" = clippy ]; then echo \"clippy failed\"; exit 101; fi\nexit 0\n",
    );

    let output = run_script(
        tmp_workspace.path(),
        bin_dir.path(),
        "cargo clippy --strict",
        "cargo test --any",
    );
    assert!(
        !output.status.success(),
        "script must exit non-zero when clippy fails",
    );

    let results =
        std::fs::read_to_string(tmp_workspace.path().join(".bellows-cargo-checks-results"))
            .unwrap();
    assert!(
        results.contains("clippy_exit=101"),
        "results must record clippy_exit=101: {results:?}",
    );
    // `test_exit=` followed by end-of-line or whitespace — i.e. empty
    // value, matching the legacy "did not run" sentinel the runner's
    // results-file parser understands.
    let has_empty_test_exit = results
        .lines()
        .any(|line| line == "test_exit=" || line.starts_with("test_exit= "));
    assert!(
        has_empty_test_exit,
        "results must record an empty test_exit when clippy short-circuited test: {results:?}",
    );
}

#[test]
fn script_logs_actual_command_and_provenance_via_run_log_lines() {
    // ADR-0004 acceptance: the script logs the actual command it is
    // about to run BEFORE executing, so an operator watching the
    // container output sees the verbatim posture. Bellows runs this
    // inside a container so the script's stdout reaches the run-log
    // via run_container's log-streaming path. The provenance line
    // itself is owned by the runner (which knows the snapshot
    // source); the SCRIPT's job is just to echo each command before
    // running it.
    let tmp_workspace = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    stub_cargo(bin_dir.path(), "#!/bin/sh\nexit 0\n");

    let output = run_script(
        tmp_workspace.path(),
        bin_dir.path(),
        "cargo clippy --custom-flag-A",
        "cargo test --custom-flag-B",
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cargo clippy --custom-flag-A"),
        "script must echo the clippy command before running: {stdout:?}",
    );
    assert!(
        stdout.contains("cargo test --custom-flag-B"),
        "script must echo the test command before running: {stdout:?}",
    );
}
