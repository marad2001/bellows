use bellows::{agent_branch_name, slugify_title};

#[test]
fn slugify_lowercases_and_replaces_spaces_with_hyphens() {
    assert_eq!(slugify_title("Fix the foo bug"), "fix-the-foo-bug");
}

#[test]
fn slugify_collapses_non_alphanumeric_runs_and_trims() {
    assert_eq!(slugify_title("Fix the foo bug!"), "fix-the-foo-bug");
    assert_eq!(slugify_title("  leading and trailing  "), "leading-and-trailing");
    assert_eq!(slugify_title("Multiple   spaces"), "multiple-spaces");
    assert_eq!(slugify_title("Punctuation: foo, bar; baz!"), "punctuation-foo-bar-baz");
    assert_eq!(slugify_title("Fix résumé bug"), "fix-r-sum-bug");
    assert_eq!(slugify_title("日本語"), "");
}

#[test]
fn slugify_truncates_long_titles_to_max_length_without_trailing_separator() {
    let long_input: String = (0..30)
        .map(|i| format!("word{}", i))
        .collect::<Vec<_>>()
        .join(" ");
    let result = slugify_title(&long_input);
    assert!(result.len() <= 50, "len was {}: {:?}", result.len(), result);
    assert!(!result.ends_with('-'), "trailing '-': {:?}", result);
}

#[test]
fn agent_branch_name_for_normal_title() {
    assert_eq!(
        agent_branch_name(42, "Fix the foo bug"),
        "agent/42-fix-the-foo-bug"
    );
}

#[test]
fn agent_branch_name_for_empty_slug_omits_trailing_hyphen() {
    assert_eq!(agent_branch_name(42, "日本語"), "agent/42");
    assert_eq!(agent_branch_name(7, "  !!!  "), "agent/7");
}
