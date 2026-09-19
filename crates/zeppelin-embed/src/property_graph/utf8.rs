//! Shared allocation-free UTF-8 validation with bounded caller checkpoints.
/// Distinguishes malformed bytes from the caller's original control error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Utf8CheckError<E> {
    /// Malformed, overlong, surrogate, out-of-range or truncated UTF-8.
    Invalid,
    /// Original cancellation, deadline, close or work failure.
    Control(E),
}
/// Validates at most 64 KiB between checkpoints without allocating or owning
/// input. A partial codepoint at a window boundary is checked in the next
/// window. The returned string borrows only the unchanged input bytes.
/// Empty input needs no validation window. Callers retain their own final
/// operation checkpoint; this helper creates no control or memory account.
pub fn checked_utf8<E>(
    bytes: &[u8],
    mut checkpoint: impl FnMut() -> Result<(), E>,
) -> Result<&str, Utf8CheckError<E>> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        checkpoint().map_err(Utf8CheckError::Control)?;
        let length = remaining.len().min(64 * 1024);
        let part = remaining.get(..length).ok_or(Utf8CheckError::Invalid)?;
        let consumed = match std::str::from_utf8(part) {
            Ok(_) => length,
            Err(error) if error.error_len().is_none() && length < remaining.len() => {
                error.valid_up_to()
            }
            Err(_) => return Err(Utf8CheckError::Invalid),
        };
        if consumed == 0 {
            return Err(Utf8CheckError::Invalid);
        }
        remaining = remaining.get(consumed..).ok_or(Utf8CheckError::Invalid)?;
    }
    // SAFETY: every byte was validated in complete UTF-8 spans above. A partial
    // code point at a chunk boundary remained in `remaining` for the next check.
    // The immutable borrowed bytes cannot change between validation and cast.
    Ok(unsafe { std::str::from_utf8_unchecked(bytes) })
}
