//! The single Web IDL conversion layer for all M2 bindings.
//!
//! Every constructor argument, dictionary member and method argument used by
//! the Blob/File/FileList bindings is converted here. Binding modules never
//! duplicate coercion logic.

use std::sync::Arc;

use boa_engine::object::JsObject;
use boa_engine::object::builtins::{JsArrayBuffer, JsDataView, JsSharedArrayBuffer, JsTypedArray};
use boa_engine::property::PropertyKey;
use boa_engine::{Context, JsResult, JsSymbol, JsValue, js_string};
use boa_fapi_core::blob::{BlobData, BlobSegment};
use boa_fapi_core::endings::{NativeLineEnding, convert_line_endings_to_native};
use boa_fapi_core::limits::FileApiLimits;
use boa_fapi_core::source::ByteSource;
use boa_fapi_core::source::memory::MemorySource;
use bytes::Bytes;

use crate::blob::BlobNative;
use crate::error::{js_from_core, range_error, type_error};
use crate::file::FileNative;

/// The platform's native line ending for `endings: "native"`.
///
/// Windows uses CRLF; every other supported platform uses LF.
#[must_use]
pub(crate) fn platform_native_ending() -> NativeLineEnding {
    if cfg!(windows) {
        NativeLineEnding::Crlf
    } else {
        NativeLineEnding::Lf
    }
}

/// `|f| mod 2^64` for a finite, integer-valued `f` (sign ignored).
///
/// The value is decomposed into mantissa and exponent so that magnitudes far
/// beyond `u64` wrap exactly like the Web IDL integer conversion algorithm;
/// no saturating `as` cast is involved.
fn magnitude_mod_u64(f: f64) -> u64 {
    debug_assert!(f.is_finite() && f == f.trunc() && f >= 0.0);
    if f == 0.0 {
        return 0;
    }
    let bits = f.to_bits();
    let biased_exponent = ((bits >> 52) & 0x7FF) as i64;
    let mantissa = bits & 0x000F_FFFF_FFFF_FFFF;
    if biased_exponent == 0 {
        // Subnormal values are smaller than 1, hence cannot be integer-valued
        // other than zero, which was handled above.
        return 0;
    }
    let exponent = biased_exponent - 1023;
    if exponent < 0 {
        return 0;
    }
    if exponent >= 116 {
        // The value is an exact multiple of 2^64.
        return 0;
    }
    // value = (2^52 | mantissa) * 2^(exponent - 52); the integer factor fits
    // in 53 bits, so any shift that drops bits below 2^64 performs `mod 2^64`.
    let integer = mantissa | (1u64 << 52);
    if exponent >= 52 {
        integer << (exponent - 52)
    } else {
        integer >> (52 - exponent)
    }
}

/// `trunc(x) mod 2^64` as a `u64` bit pattern, per Web IDL integer conversion.
fn f64_to_u64_wrap(x: f64) -> u64 {
    let truncated = x.trunc();
    if truncated == 0.0 {
        return 0;
    }
    let magnitude = magnitude_mod_u64(truncated.abs());
    if truncated < 0.0 {
        magnitude.wrapping_neg()
    } else {
        magnitude
    }
}

/// Converts `x` to an IDL `long long` value (plain conversion, no clamping).
fn f64_to_long_long(x: f64) -> i64 {
    if x.is_nan() {
        return 0;
    }
    if x == f64::INFINITY {
        return i64::MAX;
    }
    if x == f64::NEG_INFINITY {
        return i64::MIN;
    }
    // The value is already reduced into [0, 2^64); the bit pattern is the
    // two's complement representation the Web IDL algorithm mandates.
    f64_to_u64_wrap(x) as i64
}

/// Converts `x` to an IDL `unsigned long` value.
fn f64_to_unsigned_long(x: f64) -> u32 {
    if x.is_nan() {
        return 0;
    }
    if x == f64::INFINITY {
        return u32::MAX;
    }
    if x == f64::NEG_INFINITY {
        return 0;
    }
    // The low 32 bits of `mod 2^64` are exactly `mod 2^32`.
    f64_to_u64_wrap(x) as u32
}

/// Converts `x` to an IDL `[Clamp] long long` value.
///
/// Rounding is to the nearest integer with ties going to the even integer,
/// followed by clamping into the `i64` range.
fn f64_to_clamped_long_long(x: f64) -> i64 {
    if x.is_nan() {
        return 0;
    }
    if x == f64::INFINITY {
        return i64::MAX;
    }
    if x == f64::NEG_INFINITY {
        return i64::MIN;
    }
    let rounded = x.round_ties_even();
    if rounded >= 9_223_372_036_854_775_808.0 {
        // 2^63 is not representable as `i64`; the bound applies.
        return i64::MAX;
    }
    if rounded < -9_223_372_036_854_775_808.0 {
        return i64::MIN;
    }
    // Bounded to [-2^63, 2^63) and integer-valued, so the conversion is exact.
    rounded as i64
}

/// Web IDL `[Clamp] long long` conversion.
pub(crate) fn clamped_long_long(value: &JsValue, context: &mut Context) -> JsResult<i64> {
    let number = value.to_number(context)?;
    Ok(f64_to_clamped_long_long(number))
}

/// Web IDL `long long` conversion.
pub(crate) fn long_long(value: &JsValue, context: &mut Context) -> JsResult<i64> {
    let number = value.to_number(context)?;
    Ok(f64_to_long_long(number))
}

/// Web IDL `unsigned long` conversion.
pub(crate) fn unsigned_long(value: &JsValue, context: &mut Context) -> JsResult<u32> {
    let number = value.to_number(context)?;
    Ok(f64_to_unsigned_long(number))
}

/// Optional `[Clamp] long long` argument: `undefined` means absent.
pub(crate) fn optional_clamped_long_long(
    value: &JsValue,
    context: &mut Context,
) -> JsResult<Option<i64>> {
    if value.is_undefined() {
        return Ok(None);
    }
    Ok(Some(clamped_long_long(value, context)?))
}

/// Web IDL `USVString` conversion: lone surrogates become U+FFFD.
pub(crate) fn usv_string(value: &JsValue, context: &mut Context) -> JsResult<String> {
    let string = value.to_string(context)?;
    Ok(string.to_std_string_lossy())
}

/// Web IDL `DOMString` conversion to a Rust string.
///
/// Boa represents lone surrogates faithfully; they cannot exist in a Rust
/// `String`, so they are replaced with U+FFFD. Every DOMString consumed by
/// the File API bindings (MIME types, line ending selectors) treats such
/// data as non-ASCII, so this replacement preserves observable semantics.
pub(crate) fn dom_string(value: &JsValue, context: &mut Context) -> JsResult<String> {
    let string = value.to_string(context)?;
    Ok(string.to_std_string_lossy())
}

/// `endings` member: only `transparent` and `native` are valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EndingMode {
    /// Keep line endings as written.
    Transparent,
    /// Normalize line endings to the platform target.
    Native,
}

/// Parses the `endings` DOMString member.
fn parse_ending_mode(value: &JsValue, context: &mut Context) -> JsResult<EndingMode> {
    let string = value.to_string(context)?;
    if string == js_string!("native") {
        return Ok(EndingMode::Native);
    }
    if string == js_string!("transparent") {
        return Ok(EndingMode::Transparent);
    }
    Err(type_error(
        "the provided value is not of type BlobEnding (\"transparent\" or \"native\")",
    ))
}

/// Accumulates blob parts while enforcing the M1 resource limits.
///
/// The accumulator owns a single [`BlobData`] and never touches raw
/// segments: shared `Blob`/`File` parts are re-linked through the core
/// `push_shared` primitive, copied parts through `from_segments`.
pub(crate) struct PartsCollector {
    blob: BlobData,
    parts: usize,
    limits: FileApiLimits,
}

impl PartsCollector {
    pub(crate) fn new(limits: FileApiLimits) -> Self {
        Self {
            blob: BlobData::empty(""),
            parts: 0,
            limits,
        }
    }

    fn push_copied(&mut self, bytes: Bytes) -> JsResult<()> {
        if bytes.is_empty() {
            if self.parts >= self.limits.max_parts {
                return Err(range_error("too many blob parts"));
            }
            self.parts += 1;
            return Ok(());
        }
        let source: Arc<dyn ByteSource> = Arc::new(MemorySource::new(bytes));
        let len = source.len();
        let staged = BlobData::from_segments(
            vec![BlobSegment {
                source,
                offset: 0,
                len,
            }],
            "",
            &self.limits,
        )
        .map_err(|error| match error {
            boa_fapi_core::file_api_error::FileApiError::ResourceLimit(_) => {
                range_error("blob size exceeds the configured limit")
            }
            other => js_from_core(other),
        })?;
        self.push_shared(&Arc::new(staged))
    }

    fn push_shared(&mut self, data: &Arc<BlobData>) -> JsResult<()> {
        self.blob
            .push_shared(data, "", &self.limits, &mut self.parts)
            .map_err(|error| match error {
                boa_fapi_core::file_api_error::FileApiError::ResourceLimit(
                    boa_fapi_core::error::ResourceLimitKind::BlobParts,
                ) => range_error("too many blob parts"),
                boa_fapi_core::file_api_error::FileApiError::ResourceLimit(_) => {
                    range_error("blob size exceeds the configured limit")
                }
                other => js_from_core(other),
            })
    }

    /// Builds the final [`BlobData`], normalizing the media type.
    ///
    /// The accumulated blob always carries the empty media type, so changing
    /// it is a metadata-only relabel: `concat_shared` with an empty blob
    /// re-links the same sources under the same limits without copying.
    pub(crate) fn into_blob_data(self, media_type: &str) -> JsResult<BlobData> {
        BlobData::empty("")
            .concat_shared(&self.blob, media_type, &self.limits)
            .map_err(js_from_core)
    }
}

/// Copies the bytes of an `ArrayBuffer`; a detached buffer copies as empty.
fn array_buffer_bytes(buffer: &JsArrayBuffer) -> Bytes {
    match buffer.data() {
        Some(data) => Bytes::copy_from_slice(&data),
        None => Bytes::new(),
    }
}

/// Copies the visible byte range of a view (`DataView` or any `TypedArray`).
///
/// A detached backing buffer yields an empty byte sequence, per the Web IDL
/// "get a copy of the bytes held by the buffer source" algorithm.
fn view_bytes(buffer: &JsValue, byte_offset: usize, byte_length: usize) -> JsResult<Bytes> {
    let Some(object) = buffer.as_object() else {
        return Err(type_error("the viewed buffer is not an object"));
    };
    let range = byte_offset
        .checked_add(byte_length)
        .ok_or_else(|| type_error("the view range overflows"))?;

    if let Ok(shared) = JsSharedArrayBuffer::from_object(object.clone()) {
        let all = Bytes::from(shared.to_vec());
        if range > all.len() {
            return Ok(Bytes::new());
        }
        return Ok(all.slice(byte_offset..range));
    }
    if let Ok(array) = JsArrayBuffer::from_object(object) {
        let Some(data) = array.data() else {
            // Detached buffer: copy the empty byte sequence.
            return Ok(Bytes::new());
        };
        if range > data.len() {
            return Err(type_error("the view is out of bounds of its buffer"));
        }
        return Ok(Bytes::copy_from_slice(&data[byte_offset..range]));
    }
    Err(type_error(
        "the viewed buffer is neither an ArrayBuffer nor a SharedArrayBuffer",
    ))
}

/// One union-converted `BlobPart`, before options-dependent processing.
///
/// Conversion-time snapshot per the rework order contract: `BufferSource`
/// bytes are copied immediately, `USVString` is materialized immediately,
/// and `Blob`/`File` keeps its immutable backing. `endings`, MIME
/// normalization and final blob-size accounting are *not* applied here:
/// options are not converted yet.
pub(crate) enum ConvertedBlobPart {
    /// Copied byte sequence (BufferSource visible range, empty parts).
    Bytes(Bytes),
    /// Shared immutable backing of a branded `Blob`/`File`.
    Shared(Arc<BlobData>),
    /// USVString conversion result, before `endings` processing.
    Text(String),
}

/// Converts one `BlobPart` union value into its snapshot form.
///
/// Member order `(BufferSource or Blob or USVString)`: real `BufferSource`
/// copies the visible range now; branded `Blob`/`File` keeps the shared
/// backing; every other value takes the USVString `ToString` path now. A
/// forged Blob/File-shaped object fails the brand and falls to `ToString`;
/// a throwing `toString` propagates as the abrupt completion. Part-count and a
/// checked lower-bound byte-size pressure are enforced here, before the
/// converted sequence can grow beyond the configured ceiling. The final exact
/// accounting remains in the processing step.
fn convert_part(
    value: &JsValue,
    parts: &mut usize,
    lower_bound: &mut u64,
    limits: &FileApiLimits,
    context: &mut Context,
) -> JsResult<ConvertedBlobPart> {
    if *parts >= limits.max_parts {
        return Err(range_error("too many blob parts"));
    }
    if let Some(object) = value.as_object() {
        // Union member order: BufferSource, then Blob, then USVString.
        // The counter increments only for an accepted part: throwing
        // BufferSource accessors and throwing `toString` leave it
        // unchanged (the whole conversion fails anyway), and a detached
        // buffer's empty sequence still counts as one part.
        if let Ok(buffer) = JsArrayBuffer::from_object(object.clone()) {
            account_lower_bound(buffer.byte_length() as u64, lower_bound, limits)?;
            let bytes = array_buffer_bytes(&buffer);
            *parts += 1;
            return Ok(ConvertedBlobPart::Bytes(bytes));
        }
        if let Ok(shared) = JsSharedArrayBuffer::from_object(object.clone()) {
            account_lower_bound(shared.byte_length() as u64, lower_bound, limits)?;
            let bytes = Bytes::from(shared.to_vec());
            *parts += 1;
            return Ok(ConvertedBlobPart::Bytes(bytes));
        }
        if let Ok(typed) = JsTypedArray::from_object(object.clone()) {
            let offset = typed.byte_offset(context)?;
            let length = typed.byte_length(context)?;
            account_lower_bound(length as u64, lower_bound, limits)?;
            let buffer = typed.buffer(context)?;
            let bytes = view_bytes(&buffer, offset, length)?;
            *parts += 1;
            return Ok(ConvertedBlobPart::Bytes(bytes));
        }
        if let Ok(view) = JsDataView::from_object(object.clone()) {
            let offset = view.byte_offset(context)?;
            let length = view.byte_length(context)?;
            let buffer = view.buffer(context)?;
            let offset = usize::try_from(offset)
                .map_err(|_| type_error("the view offset exceeds the addressable range"))?;
            let length = usize::try_from(length)
                .map_err(|_| type_error("the view length exceeds the addressable range"))?;
            account_lower_bound(length as u64, lower_bound, limits)?;
            let bytes = view_bytes(&buffer, offset, length)?;
            *parts += 1;
            return Ok(ConvertedBlobPart::Bytes(bytes));
        }
        if let Some(native) = object.downcast_ref::<BlobNative>() {
            account_lower_bound(native.blob_data().size(), lower_bound, limits)?;
            *parts += 1;
            return Ok(ConvertedBlobPart::Shared(native.blob_data().clone()));
        }
        if let Some(native) = object.downcast_ref::<FileNative>() {
            account_lower_bound(native.blob_data().size(), lower_bound, limits)?;
            *parts += 1;
            return Ok(ConvertedBlobPart::Shared(native.blob_data().clone()));
        }
        // Any other object (plain, forged Blob/File shape, proxy, boxed
        // String): USVString fallback via observable `ToString`; a
        // throwing `toString` propagates as the abrupt completion.
        drop(object);
    }
    // Non-object values (numbers, booleans, null, Symbol, BigInt,
    // primitive strings): USVString `ToString`; Symbol throws its own
    // `TypeError` unchanged, everything else stringifies.
    let text = usv_string(value, context)?;
    let transparent_len = u64::try_from(text.len())
        .map_err(|_| range_error("blob part size exceeds the configured limit"))?;
    let native_len = line_ending_byte_len(&text)?;
    account_lower_bound(transparent_len.min(native_len), lower_bound, limits)?;
    *parts += 1;
    Ok(ConvertedBlobPart::Text(text))
}

/// Adds one phase-1 lower bound without applying `endings` or allocating a
/// normalized text copy. The bound is deliberately conservative: native line
/// ending conversion can only select the shorter transparent/native result.
fn account_lower_bound(
    part_size: u64,
    lower_bound: &mut u64,
    limits: &FileApiLimits,
) -> JsResult<()> {
    *lower_bound = lower_bound
        .checked_add(part_size)
        .ok_or_else(|| range_error("blob size exceeds the configured limit"))?;
    if *lower_bound > limits.max_blob_size {
        return Err(range_error("blob size exceeds the configured limit"));
    }
    Ok(())
}

/// Computes the UTF-8 byte length after native line-ending normalization in a
/// single pass over an already materialized USVString. No second full String is
/// created during phase-1 preflight.
fn line_ending_byte_len(text: &str) -> JsResult<u64> {
    let target_len: u64 = match platform_native_ending() {
        NativeLineEnding::Lf => 1,
        NativeLineEnding::Crlf => 2,
    };
    let mut length = 0_u64;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let add = match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                target_len
            }
            '\n' => target_len,
            other => u64::try_from(other.len_utf8())
                .map_err(|_| range_error("blob part size exceeds the configured limit"))?,
        };
        length = length
            .checked_add(add)
            .ok_or_else(|| range_error("blob part size exceeds the configured limit"))?;
    }
    Ok(length)
}

/// Applies `endings` and accumulates converted parts into `BlobData`.
///
/// Processing step after all arguments are converted: text parts are
/// line-ending processed, then every part is appended under the final
/// blob-size accounting. A failure leaves `collector` unchanged for the
/// failed part (no observable partial blob).
pub(crate) fn process_converted(
    converted: Vec<ConvertedBlobPart>,
    endings: EndingMode,
    collector: &mut PartsCollector,
) -> JsResult<()> {
    for part in converted {
        match part {
            ConvertedBlobPart::Bytes(bytes) => collector.push_copied(bytes)?,
            ConvertedBlobPart::Shared(data) => collector.push_shared(&data)?,
            ConvertedBlobPart::Text(text) => {
                let prepared = match endings {
                    EndingMode::Transparent => text,
                    EndingMode::Native => {
                        convert_line_endings_to_native(&text, platform_native_ending())
                    }
                };
                collector.push_copied(Bytes::from(prepared))?;
            }
        }
    }
    Ok(())
}

/// `GetMethod(V, @@iterator)`: one normative read, missing/null/undefined
/// means "not iterable", non-callable throws `TypeError`.
fn get_iterator_method(value: &JsValue, context: &mut Context) -> JsResult<Option<JsObject>> {
    // Primitive strings are non-objects: argument conversion fails
    // before the iterator protocol (a boxed String still iterates).
    if value.as_string().is_some() && value.as_object().is_none() {
        return Ok(None);
    }
    let key = PropertyKey::from(JsSymbol::iterator());
    let method = value.to_object(context)?.get(key, context)?;
    if method.is_null_or_undefined() {
        return Ok(None);
    }
    method
        .as_callable()
        .map(Some)
        .ok_or_else(|| type_error("the provided value is not iterable: @@iterator is not callable"))
}

/// Web IDL `sequence<BlobPart>` conversion shared by `Blob` and `File`.
///
/// Normative order: `undefined` (Blob only) is empty; `GetMethod(V,
/// @@iterator)` once; absent/non-callable throws `TypeError`; boxed
/// `String` and `TypedArray`-as-sequence iterate through their own
/// `@@iterator`; a primitive string is non-object and fails per argument
/// conversion. Iteration runs `next` → `done` → `value` left to right;
/// abrupt completion propagates unchanged with *no* iterator closing:
/// the `sequence<T>` creation steps convert `IteratorStepValue` results
/// without calling `iterator.return()` on failure.
///
/// The sequence converter performs only typed conversion: BufferSource
/// bytes are copied at element-conversion time, USVString is
/// materialized at element-conversion time, `Blob`/`File` backings are
/// retained, and the part count and conservative byte-size lower bound are
/// bounded before unbounded accumulation. It applies no `endings`, no MIME normalization and no
/// final blob-size accounting — options are not converted yet. Callers
/// run [`process_converted`] after the remaining arguments are
/// converted.
pub(crate) fn convert_sequence(
    value: &JsValue,
    required: bool,
    limits: &FileApiLimits,
    context: &mut Context,
) -> JsResult<Vec<ConvertedBlobPart>> {
    if value.is_undefined() {
        if required {
            return Err(type_error(
                "the provided value cannot be converted to a sequence<BlobPart>",
            ));
        }
        return Ok(Vec::new());
    }
    let Some(method) = get_iterator_method(value, context)? else {
        return Err(type_error(
            "the provided value cannot be converted to a sequence<BlobPart>",
        ));
    };
    let iterator_value = method.call(value, &[], context)?;
    let Some(iterator) = iterator_value.as_object() else {
        return Err(type_error("the iterator result is not an object"));
    };
    let next_value = iterator.get(js_string!("next"), context)?;
    let Some(next) = next_value.as_callable() else {
        return Err(type_error("the iterator next method is not callable"));
    };
    // Part-count and lower-bound size pressure are enforced per element inside
    // `convert_part`, before unbounded accumulation. An infinite iterator
    // therefore ends deterministically in a quota error. No `return()` runs
    // on any abrupt completion: errors propagate unchanged.
    let mut parts = 0_usize;
    let mut lower_bound = 0_u64;
    let mut converted = Vec::new();
    loop {
        let result_value = next.call(&iterator.clone().into(), &[], context)?;
        let Some(result_object) = result_value.as_object() else {
            return Err(type_error("the iterator result is not an object"));
        };
        let done_value = result_object.get(js_string!("done"), context)?;
        if done_value.to_boolean() {
            return Ok(converted);
        }
        let element = result_object.get(js_string!("value"), context)?;
        let part = convert_part(&element, &mut parts, &mut lower_bound, limits, context)?;
        converted
            .try_reserve(1)
            .map_err(|_| range_error("too many blob parts"))?;
        converted.push(part);
    }
}

/// Returns the constructor argument at `index`, or `undefined` when absent.
pub(crate) fn arg(args: &[JsValue], index: usize) -> JsValue {
    args.get(index).cloned().unwrap_or_default()
}

/// The `type` and `endings` members shared by `BlobPropertyBag` and `FilePropertyBag`.
pub(crate) struct BlobOptions {
    /// Normalized MIME type (empty string when absent or invalid).
    pub(crate) media_type: String,
    /// Line ending handling for USVString parts.
    pub(crate) endings: EndingMode,
}

impl BlobOptions {
    /// Parses a `BlobPropertyBag`-compatible options value.
    ///
    /// `null` and `undefined` produce the defaults; member getters propagate
    /// exceptions; inherited members are converted before own members and
    /// each dictionary's members in alphabetical order (`endings`, `type`).
    pub(crate) fn parse(value: &JsValue, context: &mut Context) -> JsResult<Self> {
        let defaults = Self {
            media_type: String::new(),
            endings: EndingMode::Transparent,
        };
        let Some(object) = value.as_object() else {
            if value.is_null_or_undefined() {
                return Ok(defaults);
            }
            return Err(type_error("the provided options value is not an object"));
        };

        // BlobPropertyBag members, alphabetical: endings, then type.
        let endings = match object.get(js_string!("endings"), context)? {
            v if v.is_undefined() => EndingMode::Transparent,
            v => parse_ending_mode(&v, context)?,
        };
        let media_type = match object.get(js_string!("type"), context)? {
            v if v.is_undefined() => String::new(),
            v => {
                let raw = dom_string(&v, context)?;
                boa_fapi_core::mime::normalize_blob_type(&raw)
            }
        };

        Ok(Self {
            media_type,
            endings,
        })
    }
}

/// `FilePropertyBag`: [`BlobOptions`] plus the `lastModified` member.
pub(crate) struct FileOptions {
    /// Shared Blob members.
    pub(crate) blob: BlobOptions,
    /// `Some(value)` when `lastModified` was supplied; `None` when absent.
    pub(crate) last_modified: Option<i64>,
}

impl FileOptions {
    /// Parses a `FilePropertyBag` value.
    ///
    /// Inherited members convert first (alphabetical), then `lastModified`
    /// with the ordinary `long long` conversion.
    pub(crate) fn parse(value: &JsValue, context: &mut Context) -> JsResult<Self> {
        let blob = BlobOptions::parse(value, context)?;
        let last_modified = match value.as_object() {
            Some(object) => match object.get(js_string!("lastModified"), context)? {
                v if v.is_undefined() => None,
                v => Some(long_long(&v, context)?),
            },
            // `null` produced the default dictionary already.
            None => None,
        };
        Ok(Self {
            blob,
            last_modified,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ──────────────────────────────────────────────
    // [Clamp] long long: nearest, ties-to-even, then clamp
    // ──────────────────────────────────────────────

    #[test]
    fn clamp_special_values() {
        assert_eq!(f64_to_clamped_long_long(f64::NAN), 0);
        assert_eq!(f64_to_clamped_long_long(f64::INFINITY), i64::MAX);
        assert_eq!(f64_to_clamped_long_long(f64::NEG_INFINITY), i64::MIN);
    }

    #[test]
    fn clamp_rounds_ties_to_even() {
        assert_eq!(f64_to_clamped_long_long(0.0), 0);
        assert_eq!(f64_to_clamped_long_long(-0.0), 0);
        assert_eq!(f64_to_clamped_long_long(0.4), 0);
        assert_eq!(f64_to_clamped_long_long(0.5), 0);
        assert_eq!(f64_to_clamped_long_long(0.6), 1);
        assert_eq!(f64_to_clamped_long_long(1.5), 2);
        assert_eq!(f64_to_clamped_long_long(2.5), 2);
        assert_eq!(f64_to_clamped_long_long(3.5), 4);
        assert_eq!(f64_to_clamped_long_long(-0.5), 0);
        assert_eq!(f64_to_clamped_long_long(-1.5), -2);
        assert_eq!(f64_to_clamped_long_long(-2.5), -2);
    }

    #[test]
    fn clamp_bounds_on_huge_magnitude() {
        assert_eq!(f64_to_clamped_long_long(1e300), i64::MAX);
        assert_eq!(f64_to_clamped_long_long(-1e300), i64::MIN);
        assert_eq!(
            f64_to_clamped_long_long(9_223_372_036_854_775_808.0),
            i64::MAX
        );
        assert_eq!(f64_to_clamped_long_long(1e21), i64::MAX);
    }

    // ──────────────────────────────────────────────
    // Ordinary long long: truncation + modulo 2^64
    // ──────────────────────────────────────────────

    #[test]
    fn long_long_special_values() {
        assert_eq!(f64_to_long_long(f64::NAN), 0);
        assert_eq!(f64_to_long_long(f64::INFINITY), i64::MAX);
        assert_eq!(f64_to_long_long(f64::NEG_INFINITY), i64::MIN);
    }

    #[test]
    fn long_long_truncates_toward_zero() {
        assert_eq!(f64_to_long_long(2.9), 2);
        assert_eq!(f64_to_long_long(-2.9), -2);
        assert_eq!(f64_to_long_long(0.999), 0);
        assert_eq!(f64_to_long_long(-0.999), 0);
        assert_eq!(f64_to_long_long(123.0), 123);
        assert_eq!(f64_to_long_long(-123.0), -123);
    }

    #[test]
    fn long_long_wraps_modulo_2pow64() {
        // 2^63 wraps to i64::MIN.
        assert_eq!(f64_to_long_long(9_223_372_036_854_775_808.0), i64::MIN);
        // 1.5 * 2^63 wraps to a negative value.
        assert_eq!(
            f64_to_long_long(13_835_058_055_282_163_712.0),
            -4_611_686_018_427_387_904
        );
        // 2^64 wraps to 0.
        assert_eq!(f64_to_long_long(18_446_744_073_709_551_616.0), 0);
        // 2^64 + 2^12 wraps to 2^12.
        assert_eq!(f64_to_long_long(18_446_744_073_709_555_712.0), 4_096);
        // -1 wraps to -1.
        assert_eq!(f64_to_long_long(-1.0), -1);
        // Huge magnitudes that are multiples of 2^64 wrap to 0 (3 * 2^64).
        assert_eq!(f64_to_long_long(55_340_232_221_128_654_848.0), 0);
    }

    // ──────────────────────────────────────────────
    // Ordinary unsigned long: truncation + modulo 2^32
    // ──────────────────────────────────────────────

    #[test]
    fn unsigned_long_special_values() {
        assert_eq!(f64_to_unsigned_long(f64::NAN), 0);
        assert_eq!(f64_to_unsigned_long(f64::INFINITY), u32::MAX);
        assert_eq!(f64_to_unsigned_long(f64::NEG_INFINITY), 0);
    }

    #[test]
    fn unsigned_long_wraps_modulo_2pow32() {
        assert_eq!(f64_to_unsigned_long(-1.0), u32::MAX);
        assert_eq!(f64_to_unsigned_long(4_294_967_296.0), 0);
        assert_eq!(f64_to_unsigned_long(4_294_967_297.0), 1);
        assert_eq!(f64_to_unsigned_long(-4_294_967_295.0), 1);
        assert_eq!(f64_to_unsigned_long(2.9), 2);
        assert_eq!(f64_to_unsigned_long(1e10), 1_410_065_408);
        assert_eq!(f64_to_unsigned_long(4_294_967_304.0), 8);
        assert_eq!(f64_to_unsigned_long(8_589_934_592.0), 0);
    }

    // ──────────────────────────────────────────────
    // Ending mode platform target
    // ──────────────────────────────────────────────

    #[test]
    fn platform_ending_matches_host() {
        let expected = if cfg!(windows) {
            NativeLineEnding::Crlf
        } else {
            NativeLineEnding::Lf
        };
        assert_eq!(platform_native_ending(), expected);
    }
}
