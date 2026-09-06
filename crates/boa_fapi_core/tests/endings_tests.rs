//! Tests for line ending conversion.

use boa_fapi_core::endings::{NativeLineEnding, convert_line_endings_to_native};
use proptest::prelude::*;

// ──────────────────────────────────────────────
// Unit tests for Lf target
// ──────────────────────────────────────────────

#[test]
fn lf_empty_string() {
    assert_eq!(convert_line_endings_to_native("", NativeLineEnding::Lf), "");
}

#[test]
fn lf_no_endings() {
    assert_eq!(
        convert_line_endings_to_native("hello world", NativeLineEnding::Lf),
        "hello world"
    );
}

#[test]
fn lf_bare_cr() {
    assert_eq!(
        convert_line_endings_to_native("a\rb", NativeLineEnding::Lf),
        "a\nb"
    );
}

#[test]
fn lf_bare_lf() {
    assert_eq!(
        convert_line_endings_to_native("a\nb", NativeLineEnding::Lf),
        "a\nb"
    );
}

#[test]
fn lf_crlf() {
    assert_eq!(
        convert_line_endings_to_native("a\r\nb", NativeLineEnding::Lf),
        "a\nb"
    );
}

#[test]
fn lf_cr_cr() {
    assert_eq!(
        convert_line_endings_to_native("a\rb", NativeLineEnding::Lf),
        "a\nb"
    );
    assert_eq!(
        convert_line_endings_to_native("a\r\rb", NativeLineEnding::Lf),
        "a\n\nb"
    );
}

#[test]
fn lf_lf_lf() {
    assert_eq!(
        convert_line_endings_to_native("a\n\nb", NativeLineEnding::Lf),
        "a\n\nb"
    );
}

#[test]
fn lf_crlf_cr() {
    assert_eq!(
        convert_line_endings_to_native("a\r\n\rb", NativeLineEnding::Lf),
        "a\n\nb"
    );
}

#[test]
fn lf_starts_with_cr() {
    assert_eq!(
        convert_line_endings_to_native("\rhello", NativeLineEnding::Lf),
        "\nhello"
    );
}

#[test]
fn lf_ends_with_cr() {
    assert_eq!(
        convert_line_endings_to_native("hello\r", NativeLineEnding::Lf),
        "hello\n"
    );
}

#[test]
fn lf_mixed_unicode() {
    assert_eq!(
        convert_line_endings_to_native("привет\r\nмир", NativeLineEnding::Lf),
        "привет\nмир"
    );
}

// ──────────────────────────────────────────────
// Unit tests for Crlf target
// ──────────────────────────────────────────────

#[test]
fn crlf_empty_string() {
    assert_eq!(
        convert_line_endings_to_native("", NativeLineEnding::Crlf),
        ""
    );
}

#[test]
fn crlf_no_endings() {
    assert_eq!(
        convert_line_endings_to_native("hello world", NativeLineEnding::Crlf),
        "hello world"
    );
}

#[test]
fn crlf_bare_cr() {
    assert_eq!(
        convert_line_endings_to_native("a\rb", NativeLineEnding::Crlf),
        "a\r\nb"
    );
}

#[test]
fn crlf_bare_lf() {
    assert_eq!(
        convert_line_endings_to_native("a\nb", NativeLineEnding::Crlf),
        "a\r\nb"
    );
}

#[test]
fn crlf_crlf() {
    assert_eq!(
        convert_line_endings_to_native("a\r\nb", NativeLineEnding::Crlf),
        "a\r\nb"
    );
}

#[test]
fn crlf_cr_cr() {
    assert_eq!(
        convert_line_endings_to_native("a\r\rb", NativeLineEnding::Crlf),
        "a\r\n\r\nb"
    );
}

#[test]
fn crlf_lf_lf() {
    assert_eq!(
        convert_line_endings_to_native("a\n\nb", NativeLineEnding::Crlf),
        "a\r\n\r\nb"
    );
}

#[test]
fn crlf_crlf_cr() {
    assert_eq!(
        convert_line_endings_to_native("a\r\n\rb", NativeLineEnding::Crlf),
        "a\r\n\r\nb"
    );
}

#[test]
fn crlf_starts_with_lf() {
    assert_eq!(
        convert_line_endings_to_native("\nhello", NativeLineEnding::Crlf),
        "\r\nhello"
    );
}

#[test]
fn crlf_ends_with_lf() {
    assert_eq!(
        convert_line_endings_to_native("hello\n", NativeLineEnding::Crlf),
        "hello\r\n"
    );
}

#[test]
fn crlf_mixed_unicode() {
    assert_eq!(
        convert_line_endings_to_native("привет\r\nмир", NativeLineEnding::Crlf),
        "привет\r\nмир"
    );
}

// ──────────────────────────────────────────────
// Property tests
// ──────────────────────────────────────────────

proptest! {
    #[test]
    fn no_standalone_cr_in_lf_result(input in ".*") {
        let result = convert_line_endings_to_native(&input, NativeLineEnding::Lf);
        // No CR that is not part of CRLF (but since target is Lf, there should be no CR at all)
        for (i, b) in result.bytes().enumerate() {
            prop_assert!(b != b'\r', "standalone CR at position {} in Lf result", i);
        }
    }

    #[test]
    fn no_standalone_cr_in_crlf_result(input in ".*") {
        let result = convert_line_endings_to_native(&input, NativeLineEnding::Crlf);
        // Every CR must be followed by LF
        let bytes = result.as_bytes();
        for i in 0..bytes.len() {
            if bytes[i] == b'\r'
                && (i + 1 >= bytes.len() || bytes[i + 1] != b'\n')
            {
                prop_assert!(false, "standalone CR at position {} in Crlf result", i);
            }
        }
    }

    #[test]
    fn line_ending_count_preserved(input in ".*") {
        // Count logical line endings in input: CR, LF, CRLF (but CRLF counts as one)
        let input_count = count_line_endings(&input);

        let lf_result = convert_line_endings_to_native(&input, NativeLineEnding::Lf);
        let lf_count = count_line_endings(&lf_result);
        prop_assert_eq!(input_count, lf_count, "Lf: input count {} != result count {}", input_count, lf_count);

        let crlf_result = convert_line_endings_to_native(&input, NativeLineEnding::Crlf);
        let crlf_count = count_line_endings(&crlf_result);
        prop_assert_eq!(input_count, crlf_count, "Crlf: input count {} != result count {}", input_count, crlf_count);
    }
}

fn count_line_endings(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                count += 1;
                i += 1;
                if i < bytes.len() && bytes[i] == b'\n' {
                    i += 1;
                }
            }
            b'\n' => {
                count += 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    count
}
