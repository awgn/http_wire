/// Converts an `http::Version` to its wire-format string.
///
/// # Panics
///
/// Panics if called with a version other than HTTP/1.0 or HTTP/1.1.
/// The caller is responsible for rejecting unsupported versions before calling this.
#[inline(always)]
pub(crate) fn version_to_str(version: http::Version) -> &'static str {
    match version {
        http::Version::HTTP_10 => "HTTP/1.0",
        http::Version::HTTP_11 => "HTTP/1.1",
        _ => unreachable!("unsupported HTTP version: only HTTP/1.0 and HTTP/1.1 are supported"),
    }
}



/// Returns the total length of the chunked body (including the final `0\r\n\r\n`).
///
/// Returns `None` if the data is incomplete or malformed.
#[inline]
pub(crate) fn chunked_body_len(buf: &[u8]) -> Option<usize> {
    let mut pos = 0;

    loop {
        // Find the CRLF that terminates the chunk-size line.
        let crlf = buf[pos..].windows(2).position(|w| w == b"\r\n")?;

        // The bytes before the CRLF are the chunk-size plus optional extensions.
        // Split on ';' to discard extensions, then parse as hex.
        let size_field = buf[pos..pos + crlf].splitn(2, |&b| b == b';').next()?;
        let chunk_size = usize::from_str_radix(
            std::str::from_utf8(size_field.trim_ascii()).ok()?,
            16,
        )
        .ok()?;

        // Advance past the chunk-size line (including its CRLF).
        pos += crlf + 2;

        if chunk_size == 0 {
            // Terminal chunk: find the \r\n\r\n that closes the trailer section.
            // Searching from pos-2 (the CRLF of the "0\r\n" line) means the pattern
            // matches immediately when there are no trailers, unifying both cases.
            return buf[pos - 2..]
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|k| pos - 2 + k + 4);
        }

        // Advance past chunk data and its trailing CRLF.
        pos = pos.checked_add(chunk_size)?.checked_add(2)?;
        if pos > buf.len() {
            return None;
        }
    }
}

/// Returns `true` if the `Transfer-Encoding` header value indicates chunked transfer.
///
/// Handles comma-separated lists (e.g. `gzip, chunked`) by checking only the
/// **last** coding, as required by RFC 9112 §7.1. The comparison is
/// case-insensitive and tolerates surrounding whitespace on each token.

#[inline]
pub(crate) fn is_chunked_slice(buf: &[u8]) -> bool {
    let last = buf.rsplit(|&b| b == b',').next().unwrap_or(buf);
    last.trim_ascii().eq_ignore_ascii_case(b"chunked")
}


#[cfg(test)]
mod tests {
    use super::*;

    // ── Happy path ────────────────────────────────────────────────────────────

    #[test]
    fn single_chunk() {
        // "5\r\nhello\r\n0\r\n\r\n"
        let buf = b"5\r\nhello\r\n0\r\n\r\n";
        assert_eq!(chunked_body_len(buf), Some(buf.len()));
    }

    #[test]
    fn multiple_chunks() {
        // Two data chunks followed by the terminal chunk.
        let buf = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(chunked_body_len(buf), Some(buf.len()));
    }

    #[test]
    fn chunk_size_uppercase_hex() {
        // Chunk size written in uppercase hex: A = 10.
        let buf = b"A\r\n0123456789\r\n0\r\n\r\n";
        assert_eq!(chunked_body_len(buf), Some(buf.len()));
    }

    #[test]
    fn chunk_size_mixed_case_hex() {
        // 0x1a = 26; use vec! so the count is correct by construction.
        let mut full = b"1a\r\n".to_vec();
        full.extend_from_slice(&vec![b'x'; 0x1a]);
        full.extend_from_slice(b"\r\n0\r\n\r\n");
        assert_eq!(chunked_body_len(&full), Some(full.len()));
    }

    #[test]
    fn terminal_chunk_only() {
        // Degenerate but valid: body with no data at all.
        let buf = b"0\r\n\r\n";
        assert_eq!(chunked_body_len(buf), Some(buf.len()));
    }

    #[test]
    fn extra_data_after_terminal_chunk_not_included() {
        // Bytes after the terminal \r\n\r\n must not be counted.
        let buf = b"5\r\nhello\r\n0\r\n\r\nGARBAGE";
        assert_eq!(chunked_body_len(buf), Some(b"5\r\nhello\r\n0\r\n\r\n".len()));
    }

    // ── Chunk extensions (after ';') ──────────────────────────────────────────

    #[test]
    fn chunk_extension_is_ignored() {
        // Extensions after ';' must be silently discarded.
        let buf = b"5;name=value\r\nhello\r\n0\r\n\r\n";
        assert_eq!(chunked_body_len(buf), Some(buf.len()));
    }

    #[test]
    fn terminal_chunk_with_extension() {
        let buf = b"5\r\nhello\r\n0;last\r\n\r\n";
        assert_eq!(chunked_body_len(buf), Some(buf.len()));
    }

    // ── Trailers ──────────────────────────────────────────────────────────────

    #[test]
    fn single_trailer() {
        let buf = b"5\r\nhello\r\n0\r\nTrailer-Header: value\r\n\r\n";
        assert_eq!(chunked_body_len(buf), Some(buf.len()));
    }

    #[test]
    fn multiple_trailers() {
        let buf = b"5\r\nhello\r\n0\r\nX-A: 1\r\nX-B: 2\r\n\r\n";
        assert_eq!(chunked_body_len(buf), Some(buf.len()));
    }

    // ── Incomplete data → must return None ───────────────────────────────────

    #[test]
    fn incomplete_chunk_data() {
        // Content-Length says 5 but only 3 bytes of data are present.
        let buf = b"5\r\nhel";
        assert_eq!(chunked_body_len(buf), None);
    }

    #[test]
    fn incomplete_missing_terminal_chunk() {
        // Data chunk is complete but there is no terminal "0\r\n\r\n".
        let buf = b"5\r\nhello\r\n";
        assert_eq!(chunked_body_len(buf), None);
    }

    #[test]
    fn incomplete_terminal_chunk_missing_final_crlf() {
        // "0\r\n" present but the closing "\r\n" is missing.
        let buf = b"5\r\nhello\r\n0\r\n";
        assert_eq!(chunked_body_len(buf), None);
    }

    #[test]
    fn incomplete_trailer_no_closing_crlf() {
        // Trailer line present but the final empty CRLF is missing.
        let buf = b"5\r\nhello\r\n0\r\nTrailer: val\r\n";
        assert_eq!(chunked_body_len(buf), None);
    }

    #[test]
    fn empty_buffer() {
        assert_eq!(chunked_body_len(b""), None);
    }

    // ── Malformed input → must return None ───────────────────────────────────

    #[test]
    fn invalid_hex_in_chunk_size() {
        // 'z' is not a valid hex digit.
        let buf = b"z\r\nhello\r\n0\r\n\r\n";
        assert_eq!(chunked_body_len(buf), None);
    }

    #[test]
    fn empty_chunk_size_field() {
        // A bare "\r\n" with no hex digit must be rejected — original code
        // silently treated this as chunk_size=0 (false terminal chunk).
        let buf = b"\r\nhello\r\n0\r\n\r\n";
        assert_eq!(chunked_body_len(buf), None);
    }

    #[test]
    fn chunk_size_line_lf_only_rejected() {
        // Bare LF without preceding CR must be rejected.
        let buf = b"5\nhello\n0\n\n";
        assert_eq!(chunked_body_len(buf), None);
    }

    #[test]
    fn chunk_size_with_space_in_middle_rejected() {
        // "1 2" must not be parsed as 0x12 = 18; it is invalid hex.
        // Original code silently skipped the space and returned a wrong length.
        let buf = b"1 2\r\n";
        let mut full = buf.to_vec();
        full.extend_from_slice(&[b'x'; 18]); // 18 bytes (the wrong answer)
        full.extend_from_slice(b"\r\n0\r\n\r\n");
        assert_eq!(chunked_body_len(&full), None);
    }

    // ── Overflow safety ───────────────────────────────────────────────────────

    #[test]
    fn chunk_size_overflow_rejected() {
        // A hex value that overflows usize must return None, not a wrapped
        // small value that would be accepted as a valid length.
        // Original code used wrapping_shl, so this could silently return Some.
        let buf = b"ffffffffffffffffffffffff\r\n";
        assert_eq!(chunked_body_len(buf), None);
    }

    #[test]
    fn chunk_size_exactly_usize_max_rejected() {
        // usize::MAX in hex on a 64-bit target is "ffffffffffffffff".
        // Even if it parses, pos.checked_add(chunk_size).checked_add(2)
        // must overflow and return None.
        let hex = format!("{:x}\r\n", usize::MAX);
        assert_eq!(chunked_body_len(hex.as_bytes()), None);
    }
}
