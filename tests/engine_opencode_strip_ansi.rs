//! AC3 of issue #120: `policy::strip_ansi` removes CSI escape sequences
//! from stderr-shaped input before signature-matching runs. Opencode
//! emits ANSI-coloured JSON-ish stderr by default; the
//! signature-matchers operate on stripped text so colourisation does
//! not produce false negatives.

use bellows::policy::strip_ansi;

#[test]
fn strip_ansi_removes_bare_reset_sequence() {
    assert_eq!(strip_ansi("hello\x1b[0mworld"), "helloworld");
}

#[test]
fn strip_ansi_removes_colour_sequence_around_substring() {
    assert_eq!(strip_ansi("\x1b[31mERR\x1b[0m: nope"), "ERR: nope");
}

#[test]
fn strip_ansi_removes_bracketed_csi_mid_string() {
    // A mid-string CSI sequence with multiple parameters.
    let s = "a\x1b[1;32mb\x1b[0mc";
    assert_eq!(strip_ansi(s), "abc");
}

#[test]
fn strip_ansi_no_op_on_plain_text() {
    assert_eq!(strip_ansi("no escapes here"), "no escapes here");
}
