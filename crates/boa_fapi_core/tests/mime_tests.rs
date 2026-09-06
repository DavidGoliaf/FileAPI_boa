//! Tests for MIME type normalization.

use boa_fapi_core::mime::normalize_blob_type;
use proptest::prelude::*;

// ──────────────────────────────────────────────
// Unit tests
// ──────────────────────────────────────────────

#[test]
fn empty_type() {
    assert_eq!(normalize_blob_type(""), "");
}

#[test]
fn ascii_lowercase() {
    assert_eq!(normalize_blob_type("text/plain"), "text/plain");
}

#[test]
fn mixed_case_type() {
    assert_eq!(normalize_blob_type("TEXT/PLAIN"), "text/plain");
}

#[test]
fn mixed_case_mixed() {
    assert_eq!(normalize_blob_type("Text/HTML"), "text/html");
}

#[test]
fn spaces_preserved() {
    assert_eq!(normalize_blob_type(" TEXT/PLAIN "), " text/plain ");
}

#[test]
fn leading_trailing_spaces() {
    assert_eq!(normalize_blob_type("  text/plain  "), "  text/plain  ");
}

#[test]
fn del_gives_empty() {
    assert_eq!(normalize_blob_type("text\x7Fplain"), "");
}

#[test]
fn lf_gives_empty() {
    assert_eq!(normalize_blob_type("text\nplain"), "");
}

#[test]
fn cr_gives_empty() {
    assert_eq!(normalize_blob_type("text\rplain"), "");
}

#[test]
fn nul_gives_empty() {
    assert_eq!(normalize_blob_type("text\0plain"), "");
}

#[test]
fn non_ascii_latin_gives_empty() {
    assert_eq!(normalize_blob_type("text/\u{00E9}"), "");
}

#[test]
fn emoji_gives_empty() {
    assert_eq!(normalize_blob_type("text/\u{1F600}"), "");
}

#[test]
fn tab_gives_empty() {
    // Tab (U+0009) is outside U+0020..U+007E → empty result
    assert_eq!(normalize_blob_type("text\tplain"), "");
}

#[test]
fn printable_ascii_all_preserved() {
    // All printable ASCII: U+0020 to U+007E
    let input: String = (0x20u8..=0x7E).map(|b| b as char).collect();
    let expected = input.to_ascii_lowercase();
    assert_eq!(normalize_blob_type(&input), expected);
}

// ──────────────────────────────────────────────
// Property tests
// ──────────────────────────────────────────────

proptest! {
    #[test]
    fn result_is_empty_or_ascii_printable_lowercase(input in ".*") {
        let result = normalize_blob_type(&input);
        if !result.is_empty() {
            // Must contain only U+0020..U+007E
            for ch in result.chars() {
                let in_range = ('\u{0020}'..='\u{007E}').contains(&ch);
                prop_assert!(in_range, "char {:?} outside printable ASCII range", ch);
            }
            // Must not contain ASCII uppercase
            for ch in result.chars() {
                prop_assert!(!ch.is_ascii_uppercase());
            }
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn non_ascii_input_always_empty(input in proptest::string::string_regex("[\u{0080}-\u{10FFFF}].*").unwrap()) {
        prop_assert_eq!(normalize_blob_type(&input), "");
    }
}
