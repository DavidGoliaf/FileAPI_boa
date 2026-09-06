//! MIME type normalization for `Blob.type`.

/// Normalizes a MIME type string per the File API specification.
///
/// Algorithm:
/// 1. If any Unicode scalar value is outside U+0020..U+007E, return the empty string.
/// 2. Otherwise return the string with ASCII uppercase letters lowercased (A-Z → a-z),
///    leaving all other characters unchanged.
///
/// The input is not trimmed, not parsed as MIME, and no slash/parameters validation is performed.
pub fn normalize_blob_type(input: &str) -> String {
    for ch in input.chars() {
        if !('\u{0020}'..='\u{007E}').contains(&ch) {
            return String::new();
        }
    }
    input.to_ascii_lowercase()
}
