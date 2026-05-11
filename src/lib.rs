pub mod auth;
pub mod config;
pub mod policy;
pub mod runner;
pub mod sandbox;
pub mod status;
pub mod tracker;
pub mod triage;
pub mod workspace;

const MAX_SLUG_LEN: usize = 50;

pub fn slugify_title(title: &str) -> String {
    let mut slug = slug_alnum_hyphens(title);
    if slug.len() > MAX_SLUG_LEN {
        slug.truncate(MAX_SLUG_LEN);
        while slug.ends_with('-') {
            slug.pop();
        }
    }
    slug
}

/// Lowercase ASCII alphanumerics with `-` for any run of other chars;
/// trailing hyphens stripped. Shared core of `slugify_title` (which
/// caps length) and `repo_slug` (which doesn't).
fn slug_alnum_hyphens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending && !out.is_empty() {
                out.push('-');
            }
            out.push(ch.to_ascii_lowercase());
            pending = false;
        } else {
            pending = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Docker-volume-name-safe slug for a repo URL's `owner/repo` segment.
/// Used both as the `bellows-repo-slug` label value and as the suffix
/// of the per-repo target volume name (`bellows-target-<slug>`).
///
/// Mixed-case input collapses to lowercase so an operator who retyped
/// the URL in different cases doesn't double the on-disk footprint;
/// `.git` is stripped so `bar.git` and `bar` share one cache. Total
/// function — non-URL inputs slugify whatever resembles `owner/repo`.
///
/// Lossy: every non-alphanumeric char collapses to `-` and the owner/
/// repo segments are then rejoined with `-`, so `foo/bar.baz` and
/// `foo-bar/baz` both slugify to `foo-bar-baz` and share one cache
/// volume. The collision is benign (Cargo.lock mismatch triggers a
/// cold rebuild rather than a correctness failure) but worth flagging
/// for operators picking volume names off the slug.
pub fn repo_slug(repo_url: &str) -> String {
    let trimmed = repo_url.trim().trim_end_matches('/');
    let without_git = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let segments: Vec<&str> = without_git.split('/').filter(|s| !s.is_empty()).collect();
    let owner_repo_start = segments.len().saturating_sub(2);
    segments[owner_repo_start..]
        .iter()
        .map(|s| slug_alnum_hyphens(s))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Docker volume name for the per-repo cargo `target/` cache. Sandbox
/// and prune tooling share this so a typo in one place can't desync
/// from the other.
pub fn repo_target_volume_name(repo_url: &str) -> String {
    target_volume_name_from_slug(&repo_slug(repo_url))
}

pub fn target_volume_name_from_slug(slug: &str) -> String {
    format!("bellows-target-{}", slug)
}

/// Branch name for the given issue. If the title slugifies to empty
/// (e.g. all-non-ASCII), the branch is `agent/{issue_number}` with no
/// trailing hyphen.
pub fn agent_branch_name(issue_number: u64, title: &str) -> String {
    let slug = slugify_title(title);
    if slug.is_empty() {
        format!("agent/{}", issue_number)
    } else {
        format!("agent/{}-{}", issue_number, slug)
    }
}
