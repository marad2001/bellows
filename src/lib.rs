pub mod config;
pub mod runner;
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
