pub mod config;
pub mod runner;
pub mod sandbox;
pub mod tracker;
pub mod workspace;

const MAX_SLUG_LEN: usize = 50;

pub fn slugify_title(title: &str) -> String {
    let mut slug = String::with_capacity(title.len().min(MAX_SLUG_LEN));
    let mut pending_separator = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_separator && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(ch.to_ascii_lowercase());
            pending_separator = false;
        } else {
            pending_separator = true;
        }
    }
    if slug.len() > MAX_SLUG_LEN {
        slug.truncate(MAX_SLUG_LEN);
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
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
