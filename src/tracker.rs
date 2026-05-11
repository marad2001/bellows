use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub labels: Vec<Label>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub name: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ClaimError {
    #[error("issue was already claimed by another orchestrator")]
    Contended,
    #[error(transparent)]
    Octocrab(#[from] octocrab::Error),
}

#[derive(serde::Serialize)]
struct ListIssuesParams<'a> {
    labels: &'a str,
    state: &'a str,
}

/// Query params for the oldest-first `needs-triage` listing used by the
/// slice-T2 backlog drain (issue #22). Sibling of `ListIssuesParams`
/// — separate struct because `sort` / `direction` only apply when the
/// caller cares about ordering, and the existing `find_next_issue`
/// path explicitly does not (its tiebreak is "first un-claimed in
/// API-default order", and surfacing GitHub's sort/direction defaults
/// on every list call would be churn).
#[derive(serde::Serialize)]
struct ListNeedsTriageParams<'a> {
    labels: &'a str,
    state: &'a str,
    sort: &'a str,
    direction: &'a str,
}

/// List every open issue carrying `needs_triage_label`, ordered
/// oldest-first. Used by slice T2 (`bellows triage` with no issue
/// argument) to walk the `needs-triage` backlog serially through T1's
/// per-issue triage path.
///
/// Oldest-first is part of the contract — workspace state flows
/// between issues, and an older issue's verdict may set a precedent
/// (e.g. `wontfix-enhancement` writing to `.out-of-scope/`) that a
/// later issue's triage should see. Walking newest-first would invert
/// that "earlier issues set precedent" intuition.
pub async fn list_needs_triage_issues(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    needs_triage_label: &str,
) -> Result<Vec<Issue>, octocrab::Error> {
    let params = ListNeedsTriageParams {
        labels: needs_triage_label,
        state: "open",
        sort: "created",
        direction: "asc",
    };
    let route = format!("/repos/{owner}/{repo}/issues");
    let issues: Vec<Issue> = client.get(&route, Some(&params)).await?;
    Ok(issues)
}

pub async fn find_next_issue(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    pickup_label: &str,
    in_progress_label: &str,
) -> Result<Option<Issue>, octocrab::Error> {
    let params = ListIssuesParams {
        labels: pickup_label,
        state: "open",
    };
    let route = format!("/repos/{owner}/{repo}/issues");
    let issues: Vec<Issue> = client.get(&route, Some(&params)).await?;
    Ok(issues.into_iter().find(|issue| {
        !issue.labels.iter().any(|l| l.name == in_progress_label)
    }))
}

/// Minimal shape of an open PR for the pre-claim check (#42). We only
/// care about the PR number (for the block-set log line) and the head
/// ref's name (to filter to bellows-authored `agent/*` branches).
#[derive(Debug, Deserialize)]
struct PullRequest {
    number: u64,
    head: PullRequestRef,
}

#[derive(Debug, Deserialize)]
struct PullRequestRef {
    #[serde(rename = "ref")]
    ref_name: String,
}

#[derive(serde::Serialize)]
struct ListPullsParams<'a> {
    state: &'a str,
    per_page: u32,
}

/// List the numbers of open PRs whose head ref starts with `agent/`
/// (the bellows branch-naming convention enforced by `agent_branch_name`).
///
/// Used by `run_once`'s pre-claim PR check (#42): when any such PR is
/// open, the polling tick must refuse to claim the next issue because
/// the prior PR may still gate master. The strict `agent/` prefix is
/// intentional — human-authored PRs on non-`agent/*` branches must not
/// stall bellows.
///
/// The GitHub list-pulls endpoint doesn't accept a head-prefix filter
/// server-side, so we paginate (well — request `per_page=100`, matching
/// the rest of bellows's GitHub-list calls; in practice bellows-on-bellows
/// has 0 or 1 open agent PRs at a time) and filter client-side.
pub async fn list_open_agent_prs(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
) -> Result<Vec<u64>, octocrab::Error> {
    let route = format!("/repos/{owner}/{repo}/pulls");
    let params = ListPullsParams {
        state: "open",
        per_page: 100,
    };
    let prs: Vec<PullRequest> = client.get(&route, Some(&params)).await?;
    Ok(prs
        .into_iter()
        .filter(|p| p.head.ref_name.starts_with("agent/"))
        .map(|p| p.number)
        .collect())
}

#[derive(Debug, Deserialize)]
struct Comment {
    body: Option<String>,
}

#[derive(serde::Serialize)]
struct ListCommentsParams {
    per_page: u32,
}

/// Look up the latest agent-brief comment on an issue (the one whose body
/// includes the `## Agent Brief` header that `/triage` posts). Returns
/// `Ok(None)` if no such comment exists.
pub async fn fetch_agent_brief(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    issue_number: u64,
) -> Result<Option<String>, octocrab::Error> {
    let route = format!("/repos/{owner}/{repo}/issues/{issue_number}/comments");
    let params = ListCommentsParams { per_page: 100 };
    let comments: Vec<Comment> = client.get(&route, Some(&params)).await?;

    Ok(comments
        .into_iter()
        .filter_map(|c| c.body)
        .rfind(|b| b.contains("## Agent Brief")))
}

/// Inputs for `finalise`. Bundled into a struct rather than passed as
/// 8 positional arguments — clippy's too_many_arguments threshold and
/// readability both improve.
pub struct FinaliseRequest<'a> {
    pub owner: &'a str,
    pub repo: &'a str,
    pub issue_number: u64,
    pub pr_number: u64,
    pub in_progress_label: &'a str,
    pub outcome_label: &'a str,
    pub log_body: &'a str,
}

/// Result of `finalise`. Carries the post-transition issue plus a flag
/// indicating whether the in-progress label was already gone before
/// finalise ran — the slice-10 `bellows kill <N>` operator path: when
/// the operator transitioned the label out from under the running
/// orchestrator, finalise still posts the log comment so the
/// abandoned-mid-run state is recorded, but skips the PATCH (the
/// operator's label is already correct) and signals back so run_once
/// can return `RunOutcome::Cancelled` instead of `RunOutcome::Finalised`.
pub struct FinaliseOutcome {
    pub issue: Issue,
    pub externally_cancelled: bool,
}

/// Lightweight GET on an issue's current labels to check whether
/// `in_progress_label` is still present. Used by `run_once` BEFORE
/// opening the PR to detect mid-run cancellation by `bellows kill <N>`
/// (slice 10) — without this check, the cancellation is only detected
/// in `finalise`, which is too late: the PR has already been opened
/// (potentially as ready-for-review with a "Success" log header) and
/// post_pr_comment has already posted the findings comment.
///
/// Returns `Ok(true)` if the in-progress label is still on the issue,
/// `Ok(false)` if it's been removed externally (operator cancelled).
/// Errors propagate as-is — the runner treats fetch failures as
/// "assume still in progress" (best-effort, doesn't block the pipeline).
pub async fn issue_in_progress(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    issue_number: u64,
    in_progress_label: &str,
) -> Result<bool, octocrab::Error> {
    let route = format!("/repos/{owner}/{repo}/issues/{issue_number}");
    let current: Issue = client.get(&route, None::<&()>).await?;
    Ok(current.labels.iter().any(|l| l.name == in_progress_label))
}

/// Post a log comment on the PR and transition the issue's labels from
/// `in_progress_label` to `outcome_label`. Generic over the outcome —
/// the caller decides whether the run was a success (`agent-done`),
/// a failure (`agent-failed`), or one of the more specific failure
/// kinds added in later slices.
///
/// If the GET shows that `in_progress_label` is no longer on the issue
/// (the slice-10 `bellows kill <N>` operator path), the comment is still
/// posted but the label PATCH is skipped — the operator's manual
/// transition has already moved the issue to the cancelled state, and a
/// second PATCH would clobber that.
pub async fn finalise(
    client: &octocrab::Octocrab,
    req: FinaliseRequest<'_>,
) -> Result<FinaliseOutcome, octocrab::Error> {
    let comment_route = format!(
        "/repos/{owner}/{repo}/issues/{pr_number}/comments",
        owner = req.owner,
        repo = req.repo,
        pr_number = req.pr_number,
    );
    let comment_body = serde_json::json!({ "body": req.log_body });
    let _: serde_json::Value = client.post(&comment_route, Some(&comment_body)).await?;

    let issue_route = format!(
        "/repos/{owner}/{repo}/issues/{issue_number}",
        owner = req.owner,
        repo = req.repo,
        issue_number = req.issue_number,
    );
    let current: Issue = client.get(&issue_route, None::<&()>).await?;

    if !current.labels.iter().any(|l| l.name == req.in_progress_label) {
        return Ok(FinaliseOutcome {
            issue: current,
            externally_cancelled: true,
        });
    }

    let mut new_labels: Vec<String> = current
        .labels
        .iter()
        .map(|l| l.name.clone())
        .filter(|n| n != req.in_progress_label)
        .collect();
    new_labels.push(req.outcome_label.to_string());
    new_labels.sort();

    let body = serde_json::json!({ "labels": new_labels });
    let updated: Issue = client.patch(&issue_route, Some(&body)).await?;
    Ok(FinaliseOutcome {
        issue: updated,
        externally_cancelled: false,
    })
}

/// GitHub-side handler for `bellows kill <N>`. Posts a short
/// AI-disclaimer-style cancellation comment so a human reading the
/// issue knows what happened, then transitions the issue's labels by
/// removing `from_label` (typically `agent-in-progress`) and adding
/// `to_label` (typically `agent-cancelled`). Sibling of `finalise` —
/// same comment-then-label-swap shape, but with a fixed comment body
/// rather than the runner's per-phase log summary.
pub async fn transition_to_cancelled(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    issue_number: u64,
    from_label: &str,
    to_label: &str,
) -> Result<Issue, octocrab::Error> {
    let now = chrono::Utc::now();
    let comment_body = format!(
        "> *bellows: cancelled by operator at {}*",
        now.to_rfc3339(),
    );

    let comment_route = format!("/repos/{owner}/{repo}/issues/{issue_number}/comments");
    let payload = serde_json::json!({ "body": comment_body });
    let _: serde_json::Value = client.post(&comment_route, Some(&payload)).await?;

    let issue_route = format!("/repos/{owner}/{repo}/issues/{issue_number}");
    let current: Issue = client.get(&issue_route, None::<&()>).await?;

    let mut new_labels: Vec<String> = current
        .labels
        .iter()
        .map(|l| l.name.clone())
        .filter(|n| n != from_label)
        .collect();
    if !new_labels.iter().any(|n| n == to_label) {
        new_labels.push(to_label.to_string());
    }
    new_labels.sort();

    let body = serde_json::json!({ "labels": new_labels });
    let updated: Issue = client.patch(&issue_route, Some(&body)).await?;
    Ok(updated)
}

/// Post a freestanding comment on an issue or PR (PRs share the issues
/// comments endpoint on GitHub). Used by the runner to surface the
/// review-phase findings file as a `## Review findings` comment without
/// the label-swap baked into `finalise`.
pub async fn post_pr_comment(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    pr_number: u64,
    body: &str,
) -> Result<(), octocrab::Error> {
    let route = format!("/repos/{owner}/{repo}/issues/{pr_number}/comments");
    let payload = serde_json::json!({ "body": body });
    let _: serde_json::Value = client.post(&route, Some(&payload)).await?;
    Ok(())
}

pub async fn claim(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    issue_number: u64,
    pickup_label: &str,
    in_progress_label: &str,
) -> Result<Issue, ClaimError> {
    let route = format!("/repos/{owner}/{repo}/issues/{issue_number}");

    let current: Issue = client.get(&route, None::<&()>).await?;
    if !current.labels.iter().any(|l| l.name == pickup_label) {
        return Err(ClaimError::Contended);
    }

    let mut new_labels: Vec<String> = current
        .labels
        .iter()
        .map(|l| l.name.clone())
        .filter(|n| n != pickup_label)
        .collect();
    new_labels.push(in_progress_label.to_string());
    new_labels.sort();

    let body = serde_json::json!({ "labels": new_labels });
    let updated: Issue = client.patch(&route, Some(&body)).await?;
    Ok(updated)
}
