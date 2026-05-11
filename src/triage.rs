//! Slice T2 (#22): backlog drain for `bellows triage`. Iterates the
//! open `needs-triage` issues serially and tallies the per-issue
//! verdicts into an end-of-run summary. The per-issue triage call
//! itself (T1 / issue #21) is injected as an async closure so the
//! drain logic is testable independently of a live container.
//!
//! Serial-by-design: workspace state flows between issues. If issue N
//! produces a `wontfix` verdict that commits a new
//! `.out-of-scope/` precedent and pushes it, issue N+1's container
//! picks up the updated workspace state because bellows's working
//! copy on the host reflects the commit. Parallelising the drain
//! would break that property (T1's per-issue isolation is on the
//! sandbox-container axis, not the workspace axis).

use std::fmt;

use crate::tracker::Issue;

/// The four canonical triage roles in docs/agents/triage-labels.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    ReadyForAgent,
    NeedsInfo,
    ReadyForHuman,
    Wontfix,
}

impl Verdict {
    /// The canonical label string for this verdict. Used by the apply
    /// step (to set the GitHub label) and by the summary printer.
    pub fn label(self) -> &'static str {
        match self {
            Verdict::ReadyForAgent => "ready-for-agent",
            Verdict::NeedsInfo => "needs-info",
            Verdict::ReadyForHuman => "ready-for-human",
            Verdict::Wontfix => "wontfix",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Per-verdict tally of how the backlog drain went, plus a separate
/// `failed` bucket for issues whose per-issue triage call returned
/// `Err(_)` (crash, malformed verdict, apply error). Failures stay in
/// their own bucket — they MUST NOT silently roll into a verdict
/// count, because the operator scanning the summary needs to know
/// whether any per-issue call crashed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BacklogSummary {
    pub ready_for_agent: u32,
    pub needs_info: u32,
    pub wontfix: u32,
    pub ready_for_human: u32,
    pub failed: u32,
}

impl BacklogSummary {
    pub fn total(&self) -> u32 {
        self.ready_for_agent
            + self.needs_info
            + self.wontfix
            + self.ready_for_human
            + self.failed
    }

    pub fn record_verdict(&mut self, v: Verdict) {
        match v {
            Verdict::ReadyForAgent => self.ready_for_agent += 1,
            Verdict::NeedsInfo => self.needs_info += 1,
            Verdict::ReadyForHuman => self.ready_for_human += 1,
            Verdict::Wontfix => self.wontfix += 1,
        }
    }

    pub fn record_failure(&mut self) {
        self.failed += 1;
    }
}

impl fmt::Display for BacklogSummary {
    /// End-of-run report the operator sees on stdout. Format is one
    /// header line (total processed) plus one indented line per
    /// verdict bucket. The `failed` line is included only when any
    /// failures occurred — a clean run keeps the report uncluttered.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "bellows triage: processed {} issue(s)", self.total())?;
        writeln!(f, "  {} -> ready-for-agent", self.ready_for_agent)?;
        writeln!(f, "  {} -> needs-info", self.needs_info)?;
        writeln!(f, "  {} -> wontfix", self.wontfix)?;
        writeln!(f, "  {} -> ready-for-human", self.ready_for_human)?;
        if self.failed > 0 {
            writeln!(f, "  {} failed", self.failed)?;
        }
        Ok(())
    }
}

/// Walk `issues` serially, calling `triage_one` against each one.
/// Returns the aggregated tally.
///
/// `triage_one` is an `FnMut(issue_number, dry_run) -> Future<Result<Verdict, String>>`
/// — async so it can talk to Docker / GitHub, and returning a
/// `Result` so per-issue failures can be caught and tallied. A
/// returned `Err(_)` does NOT abort the drain: the failure is
/// recorded in `summary.failed` and the loop proceeds to the next
/// issue. (Different from slice X1's halt-on-phase-failure: there,
/// halting protects one PR; here, halting would block the whole
/// backlog drain.)
///
/// The serial `for` loop is the workspace-state-flows-between-issues
/// contract. Do not change to `join_all` or similar without first
/// understanding the contract in the module docstring.
pub async fn drain_backlog<F, Fut>(
    issues: Vec<Issue>,
    dry_run: bool,
    mut triage_one: F,
) -> BacklogSummary
where
    F: FnMut(u64, bool) -> Fut,
    Fut: std::future::Future<Output = Result<Verdict, String>>,
{
    let mut summary = BacklogSummary::default();
    for issue in issues {
        match triage_one(issue.number, dry_run).await {
            Ok(v) => summary.record_verdict(v),
            Err(_) => summary.record_failure(),
        }
    }
    summary
}
