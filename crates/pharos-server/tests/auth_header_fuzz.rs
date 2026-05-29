//! V17 invariant: `parse_auth_header` / `extract_token` never panic.
//! Adversarial input must always yield `None` or a partially-parsed
//! `AuthHeader`, never a 500 with the raw value in the panic log.
//!
//! Why proptest: the parser branches on byte boundaries (quote
//! pairing, comma split, `=` split). Off-by-ones on UTF-8
//! multi-byte chars, missing quotes, repeated commas, etc. all
//! historically panic split-on-char parsers.
//!
//! These are property tests, not unit tests — they generate
//! thousands of inputs per run and only need ONE panic to fail.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_server::api::jellyfin::auth_extractor::{auth_header_from_request, parse_auth_header};
use proptest::prelude::*;

// P47 — `PROPTEST_CASES` env override. Local `just test` runs at
// 32 cases (default); CI + `just test-thorough` runs at the full
// 512. Shrink iters scale with case count so a regression still
// shrinks to a minimal repro at full size but doesn't waste local
// time on tiny case loads.
fn cfg() -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(32);
    ProptestConfig {
        cases,
        max_shrink_iters: cases.saturating_mul(4).min(256),
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(cfg())]

    /// Pure parser fuzz. Any UTF-8 string, no prefix constraint.
    #[test]
    fn parse_auth_header_never_panics_on_arbitrary_input(
        s in proptest::string::string_regex(".{0,256}").unwrap()
    ) {
        let _ = parse_auth_header(&s);
    }

    /// Hand-tuned: inputs that START with the magic prefix to exercise
    /// the inner k="v" loop branches. Most random strings get rejected
    /// at the prefix; this one always passes prefix and hits the
    /// split-on-comma / split-on-`=` path that's most likely to panic.
    #[test]
    fn parse_auth_header_never_panics_with_mediabrowser_prefix(
        kvs in proptest::collection::vec(
            (
                proptest::string::string_regex("[A-Za-z0-9_-]{0,16}").unwrap(),
                proptest::string::string_regex(".{0,32}").unwrap(),
            ),
            0..8,
        )
    ) {
        let mut s = "MediaBrowser ".to_string();
        for (i, (k, v)) in kvs.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(k);
            s.push('=');
            s.push('"');
            s.push_str(v);
            s.push('"');
        }
        let _ = parse_auth_header(&s);
    }

    /// Same shape but with the quotes deliberately mismatched —
    /// stress-tests the trim_matches('"') path.
    #[test]
    fn parse_auth_header_handles_mismatched_quotes(
        kvs in proptest::collection::vec(
            (
                proptest::string::string_regex("[A-Za-z0-9]{0,8}").unwrap(),
                proptest::string::string_regex("[A-Za-z0-9\"=,]{0,32}").unwrap(),
            ),
            1..6,
        )
    ) {
        let mut s = "MediaBrowser ".to_string();
        for (i, (k, v)) in kvs.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(k);
            s.push('=');
            s.push_str(v);
        }
        let _ = parse_auth_header(&s);
    }

    /// auth_header_from_request via TestRequest — round-trips through
    /// actix's header parsing too (rejects bytes that aren't valid HTTP
    /// header values, so we restrict to printable ASCII).
    #[test]
    fn auth_header_from_request_never_panics(
        v1 in proptest::string::string_regex("[\\x20-\\x7e]{0,128}").unwrap(),
        v2 in proptest::string::string_regex("[\\x20-\\x7e]{0,128}").unwrap(),
    ) {
        let req = actix_web::test::TestRequest::default()
            .insert_header(("X-Emby-Authorization", v1.as_str()))
            .insert_header(("Authorization", v2.as_str()))
            .to_http_request();
        let _ = auth_header_from_request(&req);
    }
}
