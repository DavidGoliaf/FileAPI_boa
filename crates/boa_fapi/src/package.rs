//! Shared memory-backed result packaging for the asynchronous `FileReader`
//! (M4-A) and the synchronous `FileReaderSync` (M4-B).
//!
//! This module is the single home of the four representations: exact bytes,
//! binary string, incremental `encoding_rs` text decoding, and checked
//! data-URL packaging. Both readers package through these helpers, so the
//! sync and async semantics cannot diverge. Nothing here touches Boa jobs,
//! events, or quotas: callers own scheduling and state.

use boa_fapi_core::error::ResourceLimitKind;
use boa_fapi_core::file_api_error::FileApiError;

/// Supported text encodings for `readAsText`: the Encoding Standard label
/// resolved through the fixed `encoding_rs` dependency.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TextEncoding {
    /// The resolved Encoding Standard encoding.
    pub(crate) encoding: &'static encoding_rs::Encoding,
    /// Whether the input starts with that encoding's BOM (UTF-8 only in
    /// this shim: `encoding_rs` strips the BOM when sniffing is enabled).
    pub(crate) strip_utf8_bom: bool,
}

/// Incremental decoder state for `readAsText`.
///
/// Wraps an `encoding_rs::Decoder`; `push` feeds one chunk and returns the
/// decoded prefix, `finish` flushes with `last = true`. Malformed sequences
/// decode with replacement, never as an exception. Split multibyte
/// sequences stay buffered inside the decoder, never emitted as U+FFFD
/// early. A leading UTF-8 BOM is stripped once (Encoding Standard BOM
/// handling) when the operation uses UTF-8 decoding.
pub(crate) struct IncrementalDecoder {
    /// The underlying `encoding_rs` decoder.
    decoder: Option<encoding_rs::Decoder>,
    /// Whether the decoder already finished.
    finished: bool,
    /// Whether the leading UTF-8 BOM was already consumed.
    bom_consumed: bool,
}

impl IncrementalDecoder {
    /// Creates a fresh decoder (no input consumed yet).
    pub(crate) fn new() -> Self {
        Self {
            decoder: None,
            finished: false,
            bom_consumed: false,
        }
    }

    /// Feeds one chunk and returns the decoded prefix.
    pub(crate) fn push(&mut self, encoding: &TextEncoding, chunk: &[u8]) -> String {
        if self.decoder.is_none() {
            self.decoder = Some(encoding.encoding.new_decoder_without_bom_handling());
        }
        let Some(decoder) = self.decoder.as_mut() else {
            return String::new();
        };
        // `decode_to_string` with `last = false`: split multibyte sequences
        // stay buffered inside the decoder, never emitted as U+FFFD early.
        // The output `String` must have spare capacity: `decode_to_string`
        // treats capacity as the output limit and never reallocates.
        let mut out = String::with_capacity(chunk.len().saturating_add(8));
        let (_, _, _) = decoder.decode_to_string(chunk, &mut out, false);
        strip_leading_bom_once(encoding, &mut self.bom_consumed, &mut out);
        out
    }

    /// Flushes the decoder at EOF (`last = true`).
    pub(crate) fn finish(&mut self, encoding: &TextEncoding) -> String {
        if self.finished {
            return String::new();
        }
        self.finished = true;
        if self.decoder.is_none() {
            self.decoder = Some(encoding.encoding.new_decoder_without_bom_handling());
        }
        let Some(decoder) = self.decoder.as_mut() else {
            return String::new();
        };
        let mut out = String::with_capacity(8);
        let (_, _, _) = decoder.decode_to_string(b"", &mut out, true);
        strip_leading_bom_once(encoding, &mut self.bom_consumed, &mut out);
        out
    }
}

impl Default for IncrementalDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Strips one leading U+FEFF once per UTF-8 operation (Encoding Standard
/// BOM handling for `readAsText`). Non-UTF-8 encodings keep the character.
fn strip_leading_bom_once(encoding: &TextEncoding, consumed: &mut bool, out: &mut String) {
    if *consumed || !encoding.strip_utf8_bom {
        return;
    }
    *consumed = true;
    if out.starts_with('\u{FEFF}') {
        out.drain(..'\u{FEFF}'.len_utf8());
    }
}

/// Resolves an encoding label.
///
/// Returns `None` for an unknown or unsupported label: the caller fails the
/// operation with `EncodingError` and no partial result. `None` (absent
/// label) defaults to UTF-8.
pub(crate) fn resolve_label(label: Option<&str>) -> Option<TextEncoding> {
    let Some(label) = label else {
        return Some(TextEncoding {
            encoding: encoding_rs::UTF_8,
            strip_utf8_bom: true,
        });
    };
    if label.trim().is_empty() {
        return Some(TextEncoding {
            encoding: encoding_rs::UTF_8,
            strip_utf8_bom: true,
        });
    }
    // `for_label_no_replacement` maps unknown labels and the `replacement`
    // encoding itself to `None`: both terminate with `EncodingError`.
    encoding_rs::Encoding::for_label_no_replacement(label.trim().as_bytes()).map(|encoding| {
        TextEncoding {
            encoding,
            strip_utf8_bom: encoding == encoding_rs::UTF_8,
        }
    })
}

/// Decodes a complete byte input with exactly the incremental semantics:
///
/// one `push` of the whole input followed by `finish`. Malformed sequences
/// decode with replacement; a single leading UTF-8 BOM is stripped.
pub(crate) fn decode_text(encoding: &TextEncoding, bytes: &[u8]) -> String {
    let mut decoder = IncrementalDecoder::new();
    let mut out = decoder.push(encoding, bytes);
    out.push_str(&decoder.finish(encoding));
    out
}

/// Packages bytes as a binary string: one code unit `U+0000..U+00FF` per
/// input byte; embedded NULs are preserved.
pub(crate) fn package_binary_string(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| char::from(*byte)).collect()
}

/// Computes the exact data-URL output length
/// (`data:<type>;base64,<payload>`) with checked arithmetic.
///
/// Base64 expands 3 bytes to 4 characters. Returns `None` when the
/// computation overflows; callers compare against `max_data_url_output`.
pub(crate) fn data_url_len(media_type: &str, byte_len: u64) -> Option<u64> {
    let payload_len = byte_len.checked_add(2)?.checked_div(3)?.checked_mul(4)?;
    let prefix_len = u64::try_from(media_type.len().saturating_add("data:;base64,".len())).ok()?;
    payload_len.checked_add(prefix_len)
}

/// Packages bytes as `data:<type>;base64,<payload>` (`data:;base64,` for an
/// empty media type) with standard base64 (no whitespace or line breaks).
///
/// Re-checks the exact output length against the ceiling before
/// allocating the payload; exceeding it is `ResourceLimit(DataUrlOutput)`.
pub(crate) fn package_data_url(
    media_type: &str,
    bytes: &[u8],
    limit: u64,
) -> Result<String, FileApiError> {
    let payload = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
    let prefix = if media_type.is_empty() {
        String::from("data:;base64,")
    } else {
        format!("data:{media_type};base64,")
    };
    let total_len = prefix.len().saturating_add(payload.len());
    if total_len as u64 > limit {
        return Err(FileApiError::ResourceLimit(
            ResourceLimitKind::DataUrlOutput,
        ));
    }
    let mut out = prefix;
    out.push_str(&payload);
    Ok(out)
}
