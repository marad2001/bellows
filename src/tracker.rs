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

pub async fn finalise_success(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    issue_number: u64,
    pr_number: u64,
    in_progress_label: &str,
    done_label: &str,
    log_body: &str,
) -> Result<Issue, octocrab::Error> {
    let comment_route = format!("/repos/{owner}/{repo}/issues/{pr_number}/comments");
    let comment_body = serde_json::json!({ "body": log_body });
    let _: serde_json::Value = client.post(&comment_route, Some(&comment_body)).await?;

    let issue_route = format!("/repos/{owner}/{repo}/issues/{issue_number}");
    let current: Issue = client.get(&issue_route, None::<&()>).await?;
    let mut new_labels: Vec<String> = current
        .labels
        .iter()
        .map(|l| l.name.clone())
        .filter(|n| n != in_progress_label)
        .collect();
    new_labels.push(done_label.to_string());
    new_labels.sort();

    let body = serde_json::json!({ "labels": new_labels });
    let updated: Issue = client.patch(&issue_route, Some(&body)).await?;
    Ok(updated)
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
