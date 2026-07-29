//! The orchestrator must not run on the platform's default main-thread
//! stack.
//!
//! `block_on` polls the pipeline on the thread that calls it, so the
//! whole await chain — `main` -> `run` -> `run_once` -> the phase helper
//! -> `sandbox` -> bollard's docker call — is one chain of live stack
//! frames. In a debug build `run_once`'s async body alone reserves
//! ~540 KB, and the cargo-checks gate (two frames deeper than the
//! implement phase) took the chain to ~940 KB. Windows reserves 1 MB for
//! the main thread, so bellows died with STATUS_STACK_OVERFLOW inside
//! `run_cargo_checks`, one second into phase 2, on every run.
//!
//! Restoring `#[tokio::main]` would silently put the pipeline back on
//! that 1 MB stack — and the symptom is a bare process death with
//! nothing in `bellows.log` after the phase-2 announcement, which reads
//! like a docker or network fault rather than a stack fault. That is
//! expensive to diagnose twice, so it is pinned here instead.
//!
//! Structural tests over the source tree are an established pattern in
//! this repo — see `tests/run_log_timestamps.rs` and `tests/readme.rs`.

use std::path::{Path, PathBuf};

fn main_rs() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("main.rs")
}

fn body() -> String {
    let path = main_rs();
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Whitespace-stripped matching, so a rustfmt reflow of an attribute or
/// a builder chain cannot make the guard silently stop matching.
///
/// Comment lines are dropped first. The comments in `main.rs` explain
/// what went wrong by naming `#[tokio::main]`, and a scan that cannot
/// tell prose from code would read its own explanation as the offence.
fn strip_ws(s: &str) -> String {
    s.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .flat_map(str::chars)
        .filter(|c| !c.is_whitespace())
        .collect()
}

#[test]
fn main_does_not_use_the_tokio_main_attribute() {
    assert!(
        !strip_ws(&body()).contains("#[tokio::main]"),
        "src/main.rs uses #[tokio::main], which polls the pipeline on the \
         main thread — 1 MB on Windows, where run_once's frame chain needs \
         roughly one. Build the runtime on a thread with an explicit \
         stack_size instead.",
    );
}

#[test]
fn the_runtime_runs_on_a_thread_with_an_explicit_stack_size() {
    let stripped = strip_ws(&body());
    assert!(
        stripped.contains("thread::Builder::new()"),
        "src/main.rs must build the orchestrator thread itself",
    );
    assert!(
        stripped.contains(".stack_size("),
        "the orchestrator thread must be given an explicit stack_size; \
         inheriting the platform default is what overflowed",
    );
    assert!(
        stripped.contains(".block_on("),
        "the runtime must be driven from that thread",
    );
}

#[test]
fn the_stack_is_sized_well_clear_of_the_measured_high_water_mark() {
    // The measured deepest chain was ~940 KB and grows with every local
    // a phase gains. An over-large reserve costs address space, not
    // memory — pages are committed on demand — so the floor here is
    // deliberately far above the observed need rather than snug.
    const FLOOR_BYTES: usize = 16 * 1024 * 1024;
    assert!(
        bellows_orchestrator_stack_bytes() >= FLOOR_BYTES,
        "the orchestrator stack is {} bytes, below the {FLOOR_BYTES}-byte floor",
        bellows_orchestrator_stack_bytes(),
    );
}

/// Read the constant out of the source rather than importing it: `main.rs`
/// is a binary crate, so its items are not reachable from an integration
/// test. Parsing the declaration keeps the number in one place.
fn bellows_orchestrator_stack_bytes() -> usize {
    let stripped = strip_ws(&body());
    let decl = "constORCHESTRATOR_STACK_BYTES:usize=";
    let start = stripped
        .find(decl)
        .unwrap_or_else(|| panic!("src/main.rs must declare ORCHESTRATOR_STACK_BYTES"))
        + decl.len();
    let expr = &stripped[start..stripped[start..]
        .find(';')
        .unwrap_or_else(|| panic!("ORCHESTRATOR_STACK_BYTES declaration must end in `;`"))
        + start];
    // The declaration is a product of literals (`64 * 1024 * 1024`).
    expr.split('*')
        .map(|factor| {
            factor
                .trim()
                .replace('_', "")
                .parse::<usize>()
                .unwrap_or_else(|e| panic!("ORCHESTRATOR_STACK_BYTES factor {factor:?}: {e}"))
        })
        .product()
}
