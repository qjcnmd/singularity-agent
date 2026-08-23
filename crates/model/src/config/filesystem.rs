//! Filesystem trust-boundary seam for provider configuration.
//!
//! Path-bounded reads keep provider config and auth files within the
//! configuration module's existing helpers.

/// Failure modes of bounded text reads: the content exceeded the configured
/// byte bound, or the read itself failed before a complete value was produced.
pub(crate) enum BoundedTextError {
    TooLarge,
    Read,
}

pub(crate) fn read_bounded_text_from_file(
    file: &mut std::fs::File,
    max_bytes: usize,
) -> Result<String, BoundedTextError> {
    use std::io::Read;
    let max_bytes_u64 = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    let metadata_len = file.metadata().map_err(|_| BoundedTextError::Read)?.len();
    if metadata_len > max_bytes_u64 {
        return Err(BoundedTextError::TooLarge);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata_len).unwrap_or(max_bytes));
    file.take(max_bytes_u64.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| BoundedTextError::Read)?;
    if bytes.len() > max_bytes {
        return Err(BoundedTextError::TooLarge);
    }
    String::from_utf8(bytes).map_err(|_| BoundedTextError::Read)
}
