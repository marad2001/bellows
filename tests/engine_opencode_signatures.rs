//! AC4 + AC5 of issue #120: opencode-specific stderr signatures for
//! rate-limit (HTTP 429) and auth-error (HTTP 401) cases. Both are
//! composite matches (substring AND substring) so a bare numeric code
//! anywhere else in the agent's webfetched content does not produce
//! false positives. The union helpers
//! (`is_rate_limit_signature` / `is_auth_error_signature`) must
//! include opencode now too.

use bellows::policy::{
    is_auth_error_signature, is_opencode_auth_error_signature,
    is_opencode_rate_limit_signature, is_rate_limit_signature,
};

// -----------------------------------------------------------------
// AC4: composite AI_APICallError + statusCode 429.
// -----------------------------------------------------------------

#[test]
fn opencode_rate_limit_signature_matches_composite_429() {
    let stderr = r#"
{"name":"AI_APICallError","message":"...","statusCode":429,"...":"..."}
"#;
    assert!(
        is_opencode_rate_limit_signature(stderr),
        "composite AI_APICallError + statusCode 429 must match: {stderr:?}",
    );
    // The union signature also matches now.
    assert!(
        is_rate_limit_signature(stderr),
        "is_rate_limit_signature union must include opencode 429",
    );
}

#[test]
fn opencode_rate_limit_signature_requires_both_substrings() {
    // Only AI_APICallError — must NOT match (could be any API call).
    let only_call_error = r#"{"name":"AI_APICallError","statusCode":500}"#;
    assert!(
        !is_opencode_rate_limit_signature(only_call_error),
        "AI_APICallError without 429 must NOT match: {only_call_error:?}",
    );
    // Only 429 — must NOT match (could be unrelated HTTP content).
    let only_429 = r#"some unrelated content "statusCode":429 mentioned"#;
    assert!(
        !is_opencode_rate_limit_signature(only_429),
        "bare statusCode 429 without AI_APICallError must NOT match: {only_429:?}",
    );
}

// -----------------------------------------------------------------
// AC5: composite AI_APICallError + statusCode 401.
// -----------------------------------------------------------------

#[test]
fn opencode_auth_error_signature_matches_composite_401() {
    let stderr = r#"{"name":"AI_APICallError","statusCode":401,"message":"..."}"#;
    assert!(
        is_opencode_auth_error_signature(stderr),
        "composite AI_APICallError + 401 must match: {stderr:?}",
    );
    assert!(
        is_auth_error_signature(stderr),
        "union must include opencode 401",
    );
}

#[test]
fn opencode_auth_error_signature_requires_both_substrings() {
    let only_call_error = r#"{"name":"AI_APICallError","statusCode":500}"#;
    assert!(!is_opencode_auth_error_signature(only_call_error));
    let only_401 = r#"unrelated content "statusCode":401 elsewhere"#;
    assert!(!is_opencode_auth_error_signature(only_401));
}

// -----------------------------------------------------------------
// Both opencode signatures must run on ANSI-stripped input so coloured
// opencode stderr still matches.
// -----------------------------------------------------------------

#[test]
fn opencode_signatures_strip_ansi_before_matching() {
    let coloured = "\x1b[31m{\"name\":\"AI_APICallError\",\"statusCode\":429}\x1b[0m";
    assert!(
        is_opencode_rate_limit_signature(coloured),
        "coloured opencode stderr must still match the 429 signature: {coloured:?}",
    );
    let coloured_401 = "\x1b[31m{\"name\":\"AI_APICallError\",\"statusCode\":401}\x1b[0m";
    assert!(
        is_opencode_auth_error_signature(coloured_401),
        "coloured opencode stderr must still match the 401 signature: {coloured_401:?}",
    );
}
