//! Line ending conversion for `Blob.prototype.slice()` with `endings = "native"`.

/// Target line ending style.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeLineEnding {
    /// Unix-style line feed.
    Lf,
    /// Windows-style carriage return + line feed.
    Crlf,
}

/// Converts all line endings in the input to the target style.
///
/// Each bare CR, bare LF, and CRLF sequence is normalized to exactly one target ending.
/// All other code points (including non-ASCII) are preserved unchanged.
pub fn convert_line_endings_to_native(input: &str, target: NativeLineEnding) -> String {
    let target = match target {
        NativeLineEnding::Lf => "\n",
        NativeLineEnding::Crlf => "\r\n",
    };

    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                result.push_str(target);
                // Skip LF immediately following CR (CRLF → single target)
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            '\n' => {
                result.push_str(target);
            }
            other => {
                result.push(other);
            }
        }
    }

    result
}
