//! `bellows policy-image-gen` — print the canonical opencode install
//! snippet so operators eyeballing the policy image can see the pinned
//! version + sha256 without parsing the Dockerfile. AC13 of issue #120.
//!
//! AC14's Dockerfile test verifies that the rendered snippet appears
//! verbatim in `policy-image/Dockerfile`, so this binary doubles as
//! the source-of-truth printer when an operator runs:
//!
//!     cargo run --bin policy-image-gen
//!
//! while drafting a Dockerfile bump.

fn main() {
    print!("{}", bellows::policy_image_gen::opencode_install_snippet());
}
