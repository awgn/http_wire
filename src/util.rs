use bytes::{BufMut, BytesMut};
use http_body_util::BodyExt;
use std::pin::{Pin, pin};
use std::task::{Context, Poll, Waker};

use crate::error::WireError;

/// Upper bound on speculative body pre-allocation driven by `Body::size_hint`.
///
/// The hint comes from user-supplied bodies and must not be trusted blindly.
const MAX_BODY_PREALLOC: usize = 16 * 1024 * 1024;

/// Returns the number of body bytes worth reserving up front.
#[inline]
pub(crate) fn body_prealloc<B: BodyExt>(body: &B) -> usize {
    usize::try_from(body.size_hint().lower())
        .unwrap_or(usize::MAX)
        .min(MAX_BODY_PREALLOC)
}

/// Exact serialized length of all header fields (`name: value\r\n` each).
#[inline]
pub(crate) fn headers_wire_len(headers: &http::HeaderMap) -> usize {
    headers
        .iter()
        .map(|(name, value)| name.as_str().len() + value.len() + 4)
        .sum()
}

/// Appends every data frame of `body` to `buf`, ignoring trailers.
///
/// Fast path: the body is polled once with a no-op waker. In-memory bodies
/// (`Full`, `Empty`, ...) are always ready, so no executor machinery is needed.
/// If the body ever returns `Pending`, we fall back to `futures::executor::block_on`.
#[inline]
pub(crate) fn drain_body<B>(body: B, buf: &mut BytesMut) -> Result<(), WireError>
where
    B: BodyExt,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let mut body = pin!(body);
    let mut cx = Context::from_waker(Waker::noop());
    match poll_all(body.as_mut(), &mut cx, buf) {
        Poll::Ready(res) => res,
        Poll::Pending => drain_body_blocking(body, buf),
    }
}

#[cold]
#[inline(never)]
fn drain_body_blocking<B>(mut body: Pin<&mut B>, buf: &mut BytesMut) -> Result<(), WireError>
where
    B: BodyExt,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    futures::executor::block_on(std::future::poll_fn(|cx| {
        poll_all(body.as_mut(), cx, buf)
    }))
}

#[inline(always)]
fn poll_all<B>(
    mut body: Pin<&mut B>,
    cx: &mut Context<'_>,
    buf: &mut BytesMut,
) -> Poll<Result<(), WireError>>
where
    B: BodyExt,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    loop {
        match body.as_mut().poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    buf.put(data);
                }
            }
            Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(WireError::Collection(e.into()))),
            Poll::Ready(None) => return Poll::Ready(Ok(())),
            Poll::Pending => return Poll::Pending,
        }
    }
}

/// Determines the body slice of a message whose head (`headers_len` bytes) was already parsed.
///
/// Looks at `Content-Length` / `Transfer-Encoding` and returns the body as a sub-slice of `buf`.
#[inline]
pub(crate) fn frame_body<'buf>(
    headers: &[httparse::Header<'_>],
    buf: &'buf [u8],
    headers_len: usize,
) -> Result<&'buf [u8], WireError> {
    let mut content_len: Option<usize> = None;
    let mut is_chunked = false;

    for header in headers {
        let name = header.name.as_bytes();
        if name.len() == 14 && name.eq_ignore_ascii_case(b"Content-Length") {
            content_len = parse_dec_usize(header.value);
        } else if name.len() == 17 && name.eq_ignore_ascii_case(b"Transfer-Encoding") {
            is_chunked = is_chunked_slice(header.value);
        }
    }

    let rest = &buf[headers_len..];
    if is_chunked {
        let body_len = chunked_body_len(rest).ok_or(WireError::InvalidChunkedBody)?;
        Ok(&rest[..body_len])
    } else {
        // If content-length is missing, length is 0
        let body_len = content_len.unwrap_or(0);
        rest.get(..body_len)
            .ok_or(WireError::IncompleteBody(body_len.saturating_sub(rest.len())))
    }
}

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

/// Parses the chunk-size field from a chunk header line.
///
/// Discards optional chunk extensions (starting with `;`), strips surrounding ASCII whitespace,
/// and parses the hexadecimal value directly from ASCII bytes without UTF-8 validation.
///
/// Returns `None` if the field is empty, contains invalid hex digits, or overflows `usize`.
#[inline]
pub(crate) fn parse_chunk_size(mut slice: &[u8]) -> Option<usize> {
    // Discard chunk extensions (anything from ';' onwards)
    if let Some(semi_pos) = slice.iter().position(|&b| b == b';') {
        slice = &slice[..semi_pos];
    }

    slice = slice.trim_ascii();
    if slice.is_empty() {
        return None;
    }

    let mut val: usize = 0;
    for &b in slice {
        let digit = match b {
            b'0'..=b'9' => (b - b'0') as usize,
            b'a'..=b'f' => (b - b'a' + 10) as usize,
            b'A'..=b'F' => (b - b'A' + 10) as usize,
            _ => return None,
        };
        val = val.checked_mul(16)?.checked_add(digit)?;
    }

    Some(val)
}

/// Parses a decimal `usize` from a byte slice (e.g. for `Content-Length`).
///
/// Strips surrounding ASCII whitespace and parses decimal digits directly from ASCII bytes
/// without UTF-8 validation.
///
/// Returns `None` if the slice is empty, contains non-digit bytes, or overflows `usize`.
#[inline]
pub(crate) fn parse_dec_usize(slice: &[u8]) -> Option<usize> {
    let slice = slice.trim_ascii();
    if slice.is_empty() {
        return None;
    }

    let mut val: usize = 0;
    for &b in slice {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val.checked_mul(10)?.checked_add((b - b'0') as usize)?;
    }

    Some(val)
}

/// Finds the index of the first occurrence of `\r\n` in `slice`.
///
/// Uses SIMD single-byte search for `\r` (via `memchr`)
/// followed by checking whether the next byte is `\n`.
#[inline]
pub(crate) fn find_crlf(slice: &[u8]) -> Option<usize> {
    let mut offset = 0;
    while let Some(rel) = memchr::memchr(b'\r', &slice[offset..]) {
        let idx = offset + rel;
        if slice.get(idx + 1) == Some(&b'\n') {
            return Some(idx);
        }
        offset = idx + 1;
    }
    None
}

/// Finds the index of the first occurrence of `\r\n\r\n` in `slice`.
///
/// Uses SIMD multi-byte substring search (via `memchr::memmem::find`).
#[inline]
pub(crate) fn find_crlf_crlf(slice: &[u8]) -> Option<usize> {
    memchr::memmem::find(slice, b"\r\n\r\n")
}

/// Returns the total length of the chunked body (including the final `0\r\n\r\n`).
///
/// Returns `None` if the data is incomplete or malformed.
#[inline]
pub(crate) fn chunked_body_len(buf: &[u8]) -> Option<usize> {
    let mut pos = 0;

    loop {
        let rest = &buf[pos..];

        // Fast path: `<hex digits>\r\n` (no extensions, no whitespace) is parsed in a
        // single pass. Anything else takes the general path below.
        let (chunk_size, line_len) = match chunk_line_fast(rest) {
            Some(v) => v,
            None => {
                // Find the CRLF that terminates the chunk-size line.
                let crlf = find_crlf(rest)?;
                // The bytes before the CRLF are the chunk-size plus optional extensions.
                (parse_chunk_size(&rest[..crlf])?, crlf + 2)
            }
        };

        // Advance past the chunk-size line (including its CRLF).
        pos += line_len;

        if chunk_size == 0 {
            // No trailers: the blank line immediately follows the "0\r\n" line.
            if buf[pos..].starts_with(b"\r\n") {
                return Some(pos + 2);
            }
            // Trailers present: find the \r\n\r\n that closes the trailer section.
            // Searching from pos-2 (the CRLF of the "0\r\n" line) keeps the same
            // semantics as the general case.
            return find_crlf_crlf(&buf[pos - 2..]).map(|k| pos - 2 + k + 4);
        }

        // Advance past chunk data and its trailing CRLF.
        pos = pos.checked_add(chunk_size)?.checked_add(2)?;
        if pos > buf.len() {
            return None;
        }
    }
}

/// Single-pass parser for the common chunk-size line shape `<hex digits>\r\n`.
///
/// Returns `(chunk_size, line_len_including_crlf)`, or `None` when the line has any
/// other shape (extensions, whitespace, overflow, missing CRLF, ...), in which case
/// the caller must use the general path to get identical semantics.
#[inline(always)]
fn chunk_line_fast(buf: &[u8]) -> Option<(usize, usize)> {
    let mut val: usize = 0;
    for (i, &b) in buf.iter().enumerate() {
        let digit = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            b'\r' if i > 0 && buf.get(i + 1) == Some(&b'\n') => return Some((val, i + 2)),
            _ => return None,
        };
        val = val.checked_mul(16)?.checked_add(digit as usize)?;
    }
    None
}

/// Returns `true` if the `Transfer-Encoding` header value indicates chunked transfer.
///
/// Handles comma-separated lists (e.g. `gzip, chunked`) by checking only the
/// **last** coding, as required by RFC 9112 §7.1. The comparison is
/// case-insensitive and tolerates surrounding whitespace on each token.
#[inline]
pub(crate) fn is_chunked_slice(buf: &[u8]) -> bool {
    let last = match memchr::memrchr(b',', buf) {
        Some(i) => &buf[i + 1..],
        None => buf,
    };
    last.trim_ascii().eq_ignore_ascii_case(b"chunked")
}

/// Appends a single HTTP header field (`name: value\r\n`) to `buf`.
#[inline]
pub(crate) fn extend_header(buf: &mut BytesMut, name: &[u8], value: &[u8]) {
    buf.extend_from_slice(name);
    buf.extend_from_slice(b": ");
    buf.extend_from_slice(value);
    buf.extend_from_slice(b"\r\n");
}

/// Appends all HTTP headers in `headers` to `buf`.
#[inline]
pub(crate) fn extend_headers(buf: &mut BytesMut, headers: &http::HeaderMap) {
    for (name, value) in headers {
        extend_header(buf, name.as_str().as_bytes(), value.as_bytes());
    }
}

/// Appends the request-target for `uri` directly to `buf` without heap allocations.
///
/// Handles:
/// - absolute-form when a scheme is present (e.g. proxy requests: `http://example.com/path?query`)
/// - origin-form otherwise (standard `/path?query`, defaulting to `/`)
#[inline]
pub(crate) fn extend_uri(buf: &mut BytesMut, uri: &http::Uri) {
    if let Some(scheme) = uri.scheme_str() {
        buf.extend_from_slice(scheme.as_bytes());
        buf.extend_from_slice(b":");
        if let Some(auth) = uri.authority() {
            buf.extend_from_slice(b"//");
            buf.extend_from_slice(auth.as_str().as_bytes());
        }
        if let Some(pq) = uri.path_and_query() {
            buf.extend_from_slice(pq.as_str().as_bytes());
        }
    } else {
        let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
        buf.extend_from_slice(path.as_bytes());
    }
}

/// Returns the byte length of the serialized request-target for `uri`.
#[inline]
pub(crate) fn uri_len(uri: &http::Uri) -> usize {
    if let Some(scheme) = uri.scheme_str() {
        let mut len = scheme.len() + 1; // "scheme:"
        if let Some(auth) = uri.authority() {
            len += 2 + auth.as_str().len(); // "//authority"
        }
        if let Some(pq) = uri.path_and_query() {
            len += pq.as_str().len();
        }
        len
    } else {
        uri.path_and_query().map(|pq| pq.as_str().len()).unwrap_or(1)
    }
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

    // ── Incomplete data → must return None ────────────────────────────────────

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

    // ── Malformed input → must return None ────────────────────────────────────

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

    // ── parse_chunk_size direct unit tests ───────────────────────────────────

    #[test]
    fn test_parse_chunk_size_direct() {
        assert_eq!(parse_chunk_size(b"0"), Some(0));
        assert_eq!(parse_chunk_size(b"1a"), Some(26));
        assert_eq!(parse_chunk_size(b"1A"), Some(26));
        assert_eq!(parse_chunk_size(b" 20 \t"), Some(32));
        assert_eq!(parse_chunk_size(b"10;foo=bar"), Some(16));
        assert_eq!(parse_chunk_size(b"10;"), Some(16));
        assert_eq!(parse_chunk_size(b""), None);
        assert_eq!(parse_chunk_size(b"   "), None);
        assert_eq!(parse_chunk_size(b";ext"), None);
        assert_eq!(parse_chunk_size(b"1g"), None);
        assert_eq!(parse_chunk_size(b"+10"), None);
        assert_eq!(parse_chunk_size(b"-10"), None);
        assert_eq!(parse_chunk_size(b"ffffffffffffffffffffffff"), None);
    }

    // ── parse_dec_usize direct unit tests ─────────────────────────────────────

    #[test]
    fn test_parse_dec_usize_direct() {
        assert_eq!(parse_dec_usize(b"0"), Some(0));
        assert_eq!(parse_dec_usize(b"42"), Some(42));
        assert_eq!(parse_dec_usize(b"  100 \t"), Some(100));
        assert_eq!(parse_dec_usize(b""), None);
        assert_eq!(parse_dec_usize(b"   "), None);
        assert_eq!(parse_dec_usize(b"12a3"), None);
        assert_eq!(parse_dec_usize(b"+10"), None);
        assert_eq!(parse_dec_usize(b"-10"), None);
        assert_eq!(parse_dec_usize(b"9999999999999999999999999999999999999999"), None);
    }

    // ── find_crlf / find_crlf_crlf direct unit tests ──────────────────────────

    #[test]
    fn test_find_crlf_direct() {
        assert_eq!(find_crlf(b"abc\r\ndef"), Some(3));
        assert_eq!(find_crlf(b"\r\ndef"), Some(0));
        assert_eq!(find_crlf(b"\r\r\n"), Some(1));
        assert_eq!(find_crlf(b"abc\rdef"), None);
        assert_eq!(find_crlf(b"abc\ndef"), None);
        assert_eq!(find_crlf(b""), None);
    }

    #[test]
    fn test_find_crlf_crlf_direct() {
        assert_eq!(find_crlf_crlf(b"header\r\n\r\nbody"), Some(6));
        assert_eq!(find_crlf_crlf(b"\r\n\r\nbody"), Some(0));
        assert_eq!(find_crlf_crlf(b"\r\n\rbody"), None);
        assert_eq!(find_crlf_crlf(b"header\r\nbody\r\n"), None);
        assert_eq!(find_crlf_crlf(b""), None);
    }

    // ── extend_header / extend_headers direct unit tests ─────────────────────

    #[test]
    fn test_extend_header_direct() {
        let mut buf = BytesMut::new();
        extend_header(&mut buf, b"Content-Type", b"text/plain");
        assert_eq!(&buf[..], b"Content-Type: text/plain\r\n");
    }

    #[test]
    fn test_extend_headers_direct() {
        let mut map = http::HeaderMap::new();
        map.insert("Host", "example.com".parse().unwrap());
        map.insert("Accept", "*/*".parse().unwrap());
        let mut buf = BytesMut::new();
        extend_headers(&mut buf, &map);
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(s.contains("host: example.com\r\n"));
        assert!(s.contains("accept: */*\r\n"));
    }

    // ── extend_uri / uri_len direct unit tests ───────────────────────────────

    #[test]
    fn test_extend_uri_absolute() {
        let uri: http::Uri = "http://example.com:8080/path?q=1".parse().unwrap();
        let mut buf = BytesMut::new();
        extend_uri(&mut buf, &uri);
        assert_eq!(&buf[..], b"http://example.com:8080/path?q=1");
        assert_eq!(buf.len(), uri_len(&uri));
        assert_eq!(&buf[..], uri.to_string().as_bytes());
    }

    #[test]
    fn test_extend_uri_origin() {
        let uri: http::Uri = "/path?q=1".parse().unwrap();
        let mut buf = BytesMut::new();
        extend_uri(&mut buf, &uri);
        assert_eq!(&buf[..], b"/path?q=1");
        assert_eq!(buf.len(), uri_len(&uri));
    }

    #[test]
    fn test_extend_uri_empty_defaults_to_slash() {
        let uri = http::Uri::default();
        let mut buf = BytesMut::new();
        extend_uri(&mut buf, &uri);
        assert_eq!(&buf[..], b"/");
        assert_eq!(buf.len(), uri_len(&uri));
    }

    #[test]
    fn test_extend_uri_scheme_and_authority_canonicalized() {
        let uri: http::Uri = "http://example.com".parse().unwrap();
        let mut buf = BytesMut::new();
        extend_uri(&mut buf, &uri);
        assert_eq!(&buf[..], b"http://example.com/");
        assert_eq!(buf.len(), uri_len(&uri));
        assert_eq!(&buf[..], uri.to_string().as_bytes());
    }

    // ---- chunked fast path must agree with the general path ----

    #[test]
    fn chunk_line_fast_accepts_only_plain_hex_crlf() {
        assert_eq!(chunk_line_fast(b"5\r\nhello"), Some((5, 3)));
        assert_eq!(chunk_line_fast(b"aF\r\n"), Some((0xaf, 4)));
        assert_eq!(chunk_line_fast(b"0\r\n\r\n"), Some((0, 3)));
        // Shapes that must be delegated to the general path.
        assert_eq!(chunk_line_fast(b"\r\n"), None); // empty size
        assert_eq!(chunk_line_fast(b"5;ext\r\n"), None); // extension
        assert_eq!(chunk_line_fast(b" 5\r\n"), None); // whitespace
        assert_eq!(chunk_line_fast(b"5 \r\n"), None);
        assert_eq!(chunk_line_fast(b"5\n"), None); // bare LF
        assert_eq!(chunk_line_fast(b"5\r"), None); // truncated
        assert_eq!(chunk_line_fast(b"5\rX\r\n"), None); // lone CR
        assert_eq!(chunk_line_fast(b"g\r\n"), None); // invalid hex
        assert_eq!(chunk_line_fast(b"ffffffffffffffffff\r\n"), None); // overflow
        assert_eq!(chunk_line_fast(b""), None);
    }

    #[test]
    fn chunked_terminal_with_and_without_trailers() {
        // No trailers: fast terminal check.
        assert_eq!(chunked_body_len(b"0\r\n\r\n"), Some(5));
        assert_eq!(chunked_body_len(b"0\r\n\r\nEXTRA"), Some(5));
        // Trailer present: general search.
        assert_eq!(chunked_body_len(b"0\r\nX: 1\r\n\r\n"), Some(11));
        // Truncated right after the terminal chunk line.
        assert_eq!(chunked_body_len(b"0\r\n"), None);
        assert_eq!(chunked_body_len(b"0\r\n\r"), None);
    }

    #[test]
    fn chunked_mixed_fast_and_slow_lines() {
        // Fast line, extension line, whitespace line, then terminal.
        let data = b"3\r\nabc\r\n3;x=y\r\ndef\r\n 3 \r\nghi\r\n0\r\n\r\n";
        assert_eq!(chunked_body_len(data), Some(data.len()));
    }

    #[test]
    fn chunked_huge_size_does_not_panic() {
        assert_eq!(chunked_body_len(b"ffffffffffffffff\r\nabc"), None);
        assert_eq!(chunked_body_len(b"7fffffffffffffff\r\nabc"), None);
    }

    /// Original implementation of `chunked_body_len`, kept as an oracle.
    fn chunked_body_len_reference(buf: &[u8]) -> Option<usize> {
        let mut pos = 0;
        loop {
            let crlf = find_crlf(&buf[pos..])?;
            let chunk_size = parse_chunk_size(&buf[pos..pos + crlf])?;
            pos += crlf + 2;
            if chunk_size == 0 {
                return find_crlf_crlf(&buf[pos - 2..]).map(|k| pos - 2 + k + 4);
            }
            pos = pos.checked_add(chunk_size)?.checked_add(2)?;
            if pos > buf.len() {
                return None;
            }
        }
    }

    #[test]
    fn chunked_body_len_matches_reference_on_random_input() {
        const TOKENS: &[&[u8]] = &[
            b"0", b"1", b"5", b"a", b"F", b"ff", b"g", b";", b"=", b" ", b"\t", b"\r\n", b"\r",
            b"\n", b"abc", b"hello", b"X: 1", b"ffffffffffffffff", b"10000000000000000",
        ];
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let mut ok_count = 0usize;
        for _ in 0..1_000_000 {
            let n = 1 + (next() % 14) as usize;
            let mut buf = Vec::new();
            for _ in 0..n {
                buf.extend_from_slice(TOKENS[(next() % TOKENS.len() as u64) as usize]);
            }
            let new = chunked_body_len(&buf);
            assert_eq!(new, chunked_body_len_reference(&buf), "input: {buf:?}");
            ok_count += new.is_some() as usize;
        }

        // Structured, mostly-valid streams, so the success path is exercised too.
        for _ in 0..200_000 {
            let mut buf = Vec::new();
            for _ in 0..(next() % 6) {
                let len = (next() % 20) as usize;
                match next() % 4 {
                    0 => buf.extend_from_slice(format!("{len:x}\r\n").as_bytes()),
                    1 => buf.extend_from_slice(format!("{len:X}\r\n").as_bytes()),
                    2 => buf.extend_from_slice(format!("{len:x};ext=1\r\n").as_bytes()),
                    _ => buf.extend_from_slice(format!(" {len:x} \r\n").as_bytes()),
                }
                buf.extend(std::iter::repeat_n(b'z', len));
                buf.extend_from_slice(b"\r\n");
            }
            buf.extend_from_slice(match next() % 4 {
                0 => b"0\r\n\r\n".as_slice(),
                1 => b"0\r\nX: 1\r\n\r\n",
                2 => b"0;e\r\n\r\n",
                _ => b"0\r\nX: 1\r\n", // incomplete trailer
            });
            // Also try every truncation of the stream.
            for cut in 0..=buf.len() {
                let new = chunked_body_len(&buf[..cut]);
                assert_eq!(new, chunked_body_len_reference(&buf[..cut]), "input: {:?}", &buf[..cut]);
                ok_count += new.is_some() as usize;
            }
        }
        assert!(ok_count > 100_000, "success path under-exercised: {ok_count}");
    }

    // ---- frame_body ----

    #[test]
    fn frame_body_content_length_and_incomplete() {
        let h = [httparse::Header { name: "Content-Length", value: b"5" }];
        let buf = b"HEAD hello";
        assert_eq!(frame_body(&h, buf, 5).unwrap(), b"hello");
        match frame_body(&h, b"HEAD hel", 5) {
            Err(WireError::IncompleteBody(2)) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn frame_body_absurd_content_length_does_not_overflow() {
        let h = [httparse::Header { name: "content-length", value: b"18446744073709551615" }];
        assert!(matches!(
            frame_body(&h, b"HEAD", 4),
            Err(WireError::IncompleteBody(_))
        ));
    }

    #[test]
    fn is_chunked_slice_last_coding_only() {
        assert!(is_chunked_slice(b"chunked"));
        assert!(is_chunked_slice(b"gzip, chunked"));
        assert!(is_chunked_slice(b" CHUNKED "));
        assert!(!is_chunked_slice(b"chunked, gzip"));
        assert!(!is_chunked_slice(b""));
        assert!(!is_chunked_slice(b"gzip,"));
    }
}
