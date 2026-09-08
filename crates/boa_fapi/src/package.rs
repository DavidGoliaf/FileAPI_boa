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
use encoding_rs::CoderResult;

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
/// `push` feeds one chunk and returns all decoded output, while `finish`
/// flushes with `last = true`. Malformed sequences decode with replacement,
/// never as an exception. Split multibyte sequences stay buffered inside the
/// decoder, never emitted as U+FFFD early. A leading BOM selects the effective
/// encoding once (Encoding Standard BOM handling): UTF-8/UTF-16LE/UTF-16BE
/// sniffing wins over any fallback.
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

    /// Feeds one chunk and returns all decoded output.
    pub(crate) fn push(
        &mut self,
        encoding: &TextEncoding,
        chunk: &[u8],
    ) -> Result<String, FileApiError> {
        if self.decoder.is_none() {
            self.decoder = Some(encoding.encoding.new_decoder());
        }
        let Some(decoder) = self.decoder.as_mut() else {
            return Err(FileApiError::Internal);
        };
        decode_all(decoder, chunk, false)
    }

    /// Flushes the decoder at EOF (`last = true`).
    pub(crate) fn finish(&mut self, encoding: &TextEncoding) -> Result<String, FileApiError> {
        if self.finished {
            return Ok(String::new());
        }
        self.finished = true;
        if self.decoder.is_none() {
            self.decoder = Some(encoding.encoding.new_decoder());
        }
        let Some(decoder) = self.decoder.as_mut() else {
            return Err(FileApiError::Internal);
        };
        decode_all(decoder, b"", true)
    }
}

/// Drives one `encoding_rs` call sequence until every input byte and every
/// pending output byte has been consumed. `decode_to_string` never reallocates
/// its receiver; `OutputFull` therefore requires explicit fallible growth.
fn decode_all(
    decoder: &mut encoding_rs::Decoder,
    input: &[u8],
    last: bool,
) -> Result<String, FileApiError> {
    let initial = input
        .len()
        .checked_mul(4)
        .and_then(|size| size.checked_add(8))
        .unwrap_or(8);
    let mut output = String::new();
    output
        .try_reserve(initial)
        .map_err(|_| FileApiError::ResourceLimit(ResourceLimitKind::MaterializeBytes))?;
    let mut offset = 0_usize;
    loop {
        let old_len = output.len();
        let (result, read, _) = decoder.decode_to_string(&input[offset..], &mut output, last);
        let new_offset = offset.checked_add(read).ok_or(FileApiError::ResourceLimit(
            ResourceLimitKind::MaterializeBytes,
        ))?;
        if new_offset > input.len() {
            return Err(FileApiError::Internal);
        }
        offset = new_offset;
        match result {
            CoderResult::InputEmpty if offset == input.len() => return Ok(output),
            CoderResult::OutputFull => {
                let previous_capacity = output.capacity();
                let additional = previous_capacity.max(8);
                output.try_reserve(additional).map_err(|_| {
                    FileApiError::ResourceLimit(ResourceLimitKind::MaterializeBytes)
                })?;
                if output.capacity() <= previous_capacity && output.len() == old_len {
                    return Err(FileApiError::ResourceLimit(
                        ResourceLimitKind::MaterializeBytes,
                    ));
                }
            }
            CoderResult::InputEmpty => {
                // `last = false` may leave a decoder-internal partial
                // sequence. It is intentionally retained until a later push.
                if offset == input.len() {
                    return Ok(output);
                }
            }
        }
    }
}

impl Default for IncrementalDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns true for the ASCII whitespace code points used by the Encoding
/// Standard and MIME parser. Rust's Unicode-aware `trim` is intentionally not
/// used: NBSP and other Unicode whitespace are label data, not outer space.
fn is_ascii_whitespace(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | b'\x0C' | b'\r' | b' ')
}

fn trim_ascii_whitespace(value: &str) -> &str {
    let bytes = value.as_bytes();
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && is_ascii_whitespace(bytes[start]) {
        start += 1;
    }
    while end > start && is_ascii_whitespace(bytes[end - 1]) {
        end -= 1;
    }
    // MIME types and encoding labels are ASCII at every successful call site.
    &value[start..end]
}

fn is_mime_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// Parses a Blob MIME type and returns its first `charset` parameter.
///
/// This is the local MIME parser used by the packaging-data algorithm. It
/// validates type/subtype tokens, parameter names, separators and quoted
/// values before exposing a charset. Duplicate parameter names follow the
/// MIME parser's first-parameter-wins rule. A malformed MIME type or charset
/// parameter returns `None`, so the caller continues with UTF-8 fallback.
pub(crate) fn mime_charset(media_type: &str) -> Option<String> {
    let bytes = media_type.as_bytes();
    if bytes
        .iter()
        .any(|byte| !(*byte).is_ascii() || !(0x20..=0x7E).contains(byte))
    {
        return None;
    }
    let mut cursor = 0_usize;
    while cursor < bytes.len() && is_ascii_whitespace(bytes[cursor]) {
        cursor += 1;
    }
    let type_start = cursor;
    while cursor < bytes.len() && bytes[cursor] != b'/' {
        cursor += 1;
    }
    if cursor == type_start
        || cursor == bytes.len()
        || !bytes[type_start..cursor]
            .iter()
            .all(|byte| is_mime_token_byte(*byte))
    {
        return None;
    }
    cursor += 1;
    let subtype_start = cursor;
    while cursor < bytes.len() && bytes[cursor] != b';' && !is_ascii_whitespace(bytes[cursor]) {
        cursor += 1;
    }
    if cursor == subtype_start
        || !bytes[subtype_start..cursor]
            .iter()
            .all(|byte| is_mime_token_byte(*byte))
    {
        return None;
    }
    while cursor < bytes.len() && is_ascii_whitespace(bytes[cursor]) {
        cursor += 1;
    }
    let mut charset = None;
    while cursor < bytes.len() {
        if bytes[cursor] != b';' {
            return None;
        }
        cursor += 1;
        while cursor < bytes.len() && is_ascii_whitespace(bytes[cursor]) {
            cursor += 1;
        }
        let name_start = cursor;
        while cursor < bytes.len() && bytes[cursor] != b'=' && bytes[cursor] != b';' {
            cursor += 1;
        }
        let name = trim_ascii_whitespace(&media_type[name_start..cursor]);
        if name.is_empty()
            || !name.bytes().all(is_mime_token_byte)
            || cursor == bytes.len()
            || bytes[cursor] != b'='
        {
            return None;
        }
        cursor += 1;
        while cursor < bytes.len() && is_ascii_whitespace(bytes[cursor]) {
            cursor += 1;
        }
        let value = if cursor < bytes.len() && bytes[cursor] == b'"' {
            cursor += 1;
            let mut value = String::new();
            let mut closed = false;
            while cursor < bytes.len() {
                match bytes[cursor] {
                    b'"' => {
                        cursor += 1;
                        closed = true;
                        break;
                    }
                    b'\\' if cursor + 1 < bytes.len() => {
                        cursor += 1;
                        value.push(bytes[cursor] as char);
                        cursor += 1;
                    }
                    byte if (0x20..=0x7E).contains(&byte) && byte != b';' => {
                        value.push(byte as char);
                        cursor += 1;
                    }
                    _ => return None,
                }
            }
            if !closed {
                return None;
            }
            while cursor < bytes.len() && is_ascii_whitespace(bytes[cursor]) {
                cursor += 1;
            }
            value
        } else {
            let value_start = cursor;
            while cursor < bytes.len() && bytes[cursor] != b';' {
                cursor += 1;
            }
            trim_ascii_whitespace(&media_type[value_start..cursor]).to_owned()
        };
        if name.eq_ignore_ascii_case("charset") && charset.is_none() {
            if value.is_empty() {
                return None;
            }
            charset = Some(value);
        }
    }
    charset
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
/// Exact `get an encoding` semantics (`Encoding::for_label`): labels that map
/// to the `replacement` encoding resolve to it instead of falling through.
/// `None` is never returned: there is no `EncodingError` path for labels.
pub(crate) fn resolve_text_encoding(label: Option<&str>, media_type: &str) -> TextEncoding {
    if let Some(label) = label
        && let Some(encoding) =
            encoding_rs::Encoding::for_label(trim_ascii_whitespace(label).as_bytes())
    {
        return TextEncoding { encoding };
    }
    if let Some(charset) = mime_charset(media_type)
        && let Some(encoding) =
            encoding_rs::Encoding::for_label(trim_ascii_whitespace(&charset).as_bytes())
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
pub(crate) fn decode_text(encoding: &TextEncoding, bytes: &[u8]) -> Result<String, FileApiError> {
    let mut decoder = IncrementalDecoder::new();
    let mut out = decoder.push(encoding, bytes)?;
    let tail = decoder.finish(encoding)?;
    append_decoded_text(&mut out, &tail)?;
    Ok(out)
}

/// Appends decoded UTF-8 with fallible capacity growth at the JS boundary.
pub(crate) fn append_decoded_text(
    destination: &mut String,
    piece: &str,
) -> Result<(), FileApiError> {
    destination
        .try_reserve(piece.len())
        .map_err(|_| FileApiError::ResourceLimit(ResourceLimitKind::MaterializeBytes))?;
    destination.push_str(piece);
    Ok(())
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
