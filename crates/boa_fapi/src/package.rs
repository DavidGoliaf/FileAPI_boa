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

fn is_http_whitespace(value: char) -> bool {
    matches!(
        value,
        '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '
    )
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

fn is_mime_token_char(value: char) -> bool {
    value.is_ascii() && is_mime_token_byte(value as u8)
}

fn is_http_quoted_string_char(value: char) -> bool {
    value == '\u{0009}'
        || ('\u{0020}'..='\u{007E}').contains(&value)
        || ('\u{0080}'..='\u{00FF}').contains(&value)
}

/// Parses a Blob MIME type and returns its first `charset` parameter.
///
/// This is the local MIME parser used by the packaging-data algorithm. It
/// validates the type/subtype record, then follows the permissive MIME parser
/// algorithm for parameters: malformed individual parameters are skipped,
/// quoted values may contain semicolons, and text after a closing quote is
/// ignored until the next separator. Duplicate parameter names follow the
/// ordered map's first-parameter-wins rule. Only a malformed type/subtype
/// record prevents a charset from being exposed.
pub(crate) fn mime_charset(media_type: &str) -> Option<String> {
    let input: Vec<char> = media_type.chars().collect();
    let mut start = 0_usize;
    let mut end = input.len();
    while start < end && is_http_whitespace(input[start]) {
        start += 1;
    }
    while end > start && is_http_whitespace(input[end - 1]) {
        end -= 1;
    }

    let mut position = start;
    let type_start = position;
    while position < end && input[position] != '/' {
        position += 1;
    }
    if position == type_start
        || position == end
        || !input[type_start..position]
            .iter()
            .copied()
            .all(is_mime_token_char)
    {
        return None;
    }

    position += 1;
    let subtype_start = position;
    while position < end && input[position] != ';' {
        position += 1;
    }
    let mut subtype_end = position;
    while subtype_end > subtype_start && is_http_whitespace(input[subtype_end - 1]) {
        subtype_end -= 1;
    }
    if subtype_end == subtype_start
        || !input[subtype_start..subtype_end]
            .iter()
            .copied()
            .all(is_mime_token_char)
    {
        return None;
    }

    let mut charset = None;
    while position < end {
        // The subtype loop leaves position at the next parameter separator.
        if input[position] != ';' {
            break;
        }
        position += 1;
        while position < end && is_http_whitespace(input[position]) {
            position += 1;
        }

        let name_start = position;
        while position < end && input[position] != ';' && input[position] != '=' {
            position += 1;
        }
        let name = &input[name_start..position];
        if position < end && input[position] == ';' {
            // A parameter without '=' is malformed, but does not invalidate
            // the MIME record or prevent later parameters from being parsed.
            continue;
        }
        if position == end {
            break;
        }
        position += 1; // Skip '='.
        if position == end {
            break;
        }

        let mut value = String::new();
        if input[position] == '"' {
            position += 1;
            while position < end {
                match input[position] {
                    '"' => {
                        position += 1;
                        break;
                    }
                    '\\' => {
                        position += 1;
                        if position == end {
                            value.push('\\');
                            break;
                        }
                        value.push(input[position]);
                        position += 1;
                    }
                    value_char => {
                        // Fetch's HTTP quoted-string collector appends every
                        // non-quote/non-backslash code point, including ';',
                        // and lets EOF terminate the collection naturally.
                        value.push(value_char);
                        position += 1;
                    }
                }
            }
            // WHATWG ignores text between the closing quote and the next
            // semicolon, e.g. `charset="windows-1252"junk`.
            while position < end && input[position] != ';' {
                position += 1;
            }
        } else {
            let value_start = position;
            while position < end && input[position] != ';' {
                position += 1;
            }
            let mut value_end = position;
            while value_end > value_start && is_http_whitespace(input[value_end - 1]) {
                value_end -= 1;
            }
            if value_end == value_start {
                continue;
            }
            value.extend(input[value_start..value_end].iter().copied());
        }

        let name_valid = !name.is_empty()
            && name.iter().copied().all(is_mime_token_char)
            && value.chars().all(is_http_quoted_string_char);
        let is_charset = name
            .iter()
            .copied()
            .collect::<String>()
            .eq_ignore_ascii_case("charset");
        if name_valid && is_charset && charset.is_none() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_decoder_sniffs_bom_when_split_after_first_or_second_byte() {
        let encoding = TextEncoding {
            encoding: encoding_rs::UTF_8,
        };
        for (first, second) in [
            (&[0xEF][..], &[0xBB, 0xBF, 0x42][..]),
            (&[0xEF, 0xBB][..], &[0xBF, 0x42][..]),
        ] {
            let mut decoder = IncrementalDecoder::new();
            let result = (|| {
                let mut output = decoder.push(&encoding, first)?;
                output.push_str(&decoder.push(&encoding, second)?);
                output.push_str(&decoder.finish(&encoding)?);
                Ok::<_, FileApiError>(output)
            })();
            assert!(
                matches!(&result, Ok(output) if output == "B"),
                "unexpected decoder result: {result:?}"
            );
        }
    }

    #[test]
    fn mime_parser_skips_malformed_parameters_and_accepts_eof_quote() {
        let expected = Some(String::from("windows-1252"));
        assert_eq!(mime_charset("text/plain;charset =windows-1252"), None);
        assert_eq!(
            mime_charset("text/plain;foo=\"a;b\";charset=windows-1252"),
            expected
        );
        assert_eq!(mime_charset("text/plain;charset=\"windows-1252"), expected);
        assert_eq!(
            mime_charset("text/plain;foo;charset=windows-1252"),
            expected
        );
        assert_eq!(
            mime_charset("text/plain;foo=\"x\"junk;charset=windows-1252"),
            expected
        );
    }
}
