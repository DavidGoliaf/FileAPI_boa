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
}

/// Incremental decoder state for `readAsText`.
///
/// Wraps an `encoding_rs::Decoder` created with BOM sniffing enabled;
/// `push` feeds one chunk and returns the decoded prefix, `finish`
/// flushes with `last = true`. Malformed sequences decode with
/// replacement, never as an exception. Split multibyte sequences stay
/// buffered inside the decoder, never emitted as U+FFFD early. A leading
/// BOM selects the effective encoding once (Encoding Standard BOM
/// handling): UTF-8/UTF-16LE/UTF-16BE sniffing wins over any fallback.
pub(crate) struct IncrementalDecoder {
    /// The underlying `encoding_rs` decoder (BOM sniffing enabled).
    decoder: Option<encoding_rs::Decoder>,
    /// Whether the decoder already finished.
    finished: bool,
}

impl IncrementalDecoder {
    /// Creates a fresh decoder (no input consumed yet).
    pub(crate) fn new() -> Self {
        Self {
            decoder: None,
            finished: false,
        }
    }

    /// Feeds one chunk and returns the decoded prefix.
    pub(crate) fn push(&mut self, encoding: &TextEncoding, chunk: &[u8]) -> String {
        if self.decoder.is_none() {
            self.decoder = Some(encoding.encoding.new_decoder());
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
        out
    }

    /// Flushes the decoder at EOF (`last = true`).
    pub(crate) fn finish(&mut self, encoding: &TextEncoding) -> String {
        if self.finished {
            return String::new();
        }
        self.finished = true;
        if self.decoder.is_none() {
            self.decoder = Some(encoding.encoding.new_decoder());
        }
        let Some(decoder) = self.decoder.as_mut() else {
            return String::new();
        };
        let mut out = String::with_capacity(8);
        let (_, _, _) = decoder.decode_to_string(b"", &mut out, true);
        out
    }
}

impl Default for IncrementalDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Extracts a `charset` parameter from a Blob MIME type per the W3C
/// packaging-data steps: split on `;`, take the first `charset=`
/// parameter, strip quotes/whitespace. Returns `None` when absent.
pub(crate) fn mime_charset(media_type: &str) -> Option<&str> {
    for param in media_type.split(';').skip(1) {
        let param = param.trim();
        let Some((name, value)) = param.split_once('=') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("charset") {
            continue;
        }
        let value = value.trim().trim_matches('"').trim();
        if value.is_empty() {
            return None;
        }
        return Some(value);
    }
    None
}

/// Shared `readAsText` encoding selection (async `FileReader` and sync
/// `FileReaderSync` call this one function, so the strings cannot
/// diverge), per File API "packaging data / Text" over Encoding Standard
/// "get an encoding" and "Decode":
///
/// 1. an explicit label selects the fallback encoding via `get an
///    encoding`; an unknown label is *failure*, not an exception — it
///    falls through to the MIME step, never `EncodingError`;
/// 2. else the Blob MIME `charset` parameter via `get an encoding`;
/// 3. else UTF-8;
/// 4. `Decode` BOM-sniffs and may replace any fallback with UTF-8,
///    UTF-16LE or UTF-16BE;
/// 5. malformed sequences decode to U+FFFD.
///
/// Exact `get an encoding` semantics (`Encoding::for_label`): labels
/// that map to the `replacement` encoding resolve to it (decoding then
/// yields U+FFFD per byte) instead of falling through. `None` is never
/// returned: there is no `EncodingError` path for labels.
pub(crate) fn resolve_text_encoding(label: Option<&str>, media_type: &str) -> TextEncoding {
    if let Some(label) = label
        && !label.trim().is_empty()
        && let Some(encoding) = encoding_rs::Encoding::for_label(label.trim().as_bytes())
    {
        return TextEncoding { encoding };
    }
    if let Some(charset) = mime_charset(media_type)
        && let Some(encoding) = encoding_rs::Encoding::for_label(charset.trim().as_bytes())
    {
        return TextEncoding { encoding };
    }
    TextEncoding {
        encoding: encoding_rs::UTF_8,
    }
}

/// Decodes a complete byte input with exactly the incremental semantics:
///
/// one `push` of the whole input followed by `finish`. Malformed sequences
/// decode with replacement; the BOM sniff selects the effective encoding.
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
