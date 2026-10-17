use bytes::Buf;
use http_body_util::BodyExt;
use std::pin::{Pin, pin};
use std::ptr;
use std::task::{Context, Poll, Waker};

use crate::error::WireError;

/// Upper bound on speculative body pre-allocation driven by `Body::size_hint`.
///
/// The hint comes from user-supplied bodies and must not be trusted blindly.
const MAX_BODY_PREALLOC: usize = 16 * 1024 * 1024;

/// Unchecked cursor writer into a pre-allocated `Vec<u8>`.
///
/// # Safety invariant
///
/// Callers must ensure that the total number of bytes written via [`put`](Self::put)
/// and [`put_byte`](Self::put_byte) never exceeds the reserved spare capacity of the
/// underlying `Vec<u8>`.
pub(crate) struct FastBufWriter {
    ptr: *mut u8,
}

impl FastBufWriter {
    /// Creates a new writer starting at the beginning of `buf`'s allocation.
    #[inline(always)]
    pub(crate) fn new(buf: &mut Vec<u8>) -> Self {
        Self {
            ptr: buf.as_mut_ptr(),
        }
    }

    /// Copies `src` to the current cursor position and advances the cursor.
    ///
    /// # Safety
    ///
    /// There must be at least `src.len()` bytes of remaining capacity in the buffer.
    #[inline(always)]
    pub(crate) unsafe fn put(&mut self, src: &[u8]) {
        let len = src.len();
        unsafe {
            ptr::copy_nonoverlapping(src.as_ptr(), self.ptr, len);
            self.ptr = self.ptr.add(len);
        }
    }

    /// Writes a single byte at the current cursor position and advances the cursor.
    ///
    /// # Safety
    ///
    /// There must be at least 1 byte of remaining capacity in the buffer.
    #[inline(always)]
    pub(crate) unsafe fn put_byte(&mut self, b: u8) {
        unsafe {
            *self.ptr = b;
            self.ptr = self.ptr.add(1);
        }
    }

    /// Writes the 2-byte sequence `\r\n` at the current cursor position.
    ///
    /// # Safety
    ///
    /// There must be at least 2 bytes of remaining capacity in the buffer.
    #[inline(always)]
    pub(crate) unsafe fn put_crlf(&mut self) {
        unsafe {
            ptr::copy_nonoverlapping(b"\r\n".as_ptr(), self.ptr, 2);
            self.ptr = self.ptr.add(2);
        }
    }
}

/// Returns the number of body bytes worth reserving up front.
#[inline(always)]
pub(crate) fn body_prealloc<B: BodyExt>(body: &B) -> usize {
    if body.is_end_stream() {
        return 0;
    }
    usize::try_from(body.size_hint().lower())
        .unwrap_or(usize::MAX)
        .min(MAX_BODY_PREALLOC)
}

/// Exact serialized length of all header fields (`name: value\r\n` each).
#[inline(always)]
pub(crate) fn headers_wire_len(headers: &http::HeaderMap) -> usize {
    headers
        .iter()
        .map(|(name, value)| name.as_str().len() + value.len() + 4)
        .sum()
}

/// Appends every data frame of `body` to `buf`, ignoring trailers.
///
/// Fast path: if `body.is_end_stream()` is true (e.g. `Empty`), returns immediately.
/// Otherwise, the body is polled once with a no-op waker. In-memory bodies
/// (`Full`, `Empty`, ...) are always ready, so no executor machinery is needed.
/// If the body ever returns `Pending`, we fall back to `futures::executor::block_on`.
#[inline(always)]
pub(crate) fn drain_body<B>(body: B, buf: &mut Vec<u8>) -> Result<(), WireError>
where
    B: BodyExt,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    if body.is_end_stream() {
        return Ok(());
    }
    let mut body = pin!(body);
    let mut cx = Context::from_waker(Waker::noop());
    match poll_all(body.as_mut(), &mut cx, buf) {
        Poll::Ready(res) => res,
        Poll::Pending => drain_body_blocking(body, buf),
    }
}

#[cold]
#[inline(never)]
fn drain_body_blocking<B>(mut body: Pin<&mut B>, buf: &mut Vec<u8>) -> Result<(), WireError>
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
    buf: &mut Vec<u8>,
) -> Poll<Result<(), WireError>>
where
    B: BodyExt,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    loop {
        match body.as_mut().poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Ok(mut data) = frame.into_data() {
                    while data.has_remaining() {
                        let chunk = data.chunk();
                        buf.extend_from_slice(chunk);
                        let n = chunk.len();
                        data.advance(n);
                    }
                }
            }
            Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(WireError::Collection(e.into()))),
            Poll::Ready(None) => return Poll::Ready(Ok(())),
            Poll::Pending => return Poll::Pending,
        }
    }
}

/// Fast SWAR case-insensitive comparison against `"content-length"` (14 bytes).
///
/// The caller must guarantee `name.len() == 14`.
#[inline(always)]
fn eq_content_length_14(name: &[u8]) -> bool {
    debug_assert_eq!(name.len(), 14);
    // SAFETY: `name.len() == 14`, so reading 8 bytes at offset 0 and 8 bytes at offset 6 is valid.
    unsafe {
        let w0 = ptr::read_unaligned(name.as_ptr() as *const u64) | 0x2020_2020_2020_2020;
        let w1 = ptr::read_unaligned(name.as_ptr().add(6) as *const u64) | 0x2020_2020_2020_2020;
        let t0 = u64::from_ne_bytes(*b"content-");
        let t1 = u64::from_ne_bytes(*b"t-length");
        (w0 ^ t0) | (w1 ^ t1) == 0
    }
}

/// Fast SWAR case-insensitive comparison against `"transfer-encoding"` (17 bytes).
///
/// The caller must guarantee `name.len() == 17`.
#[inline(always)]
fn eq_transfer_encoding_17(name: &[u8]) -> bool {
    debug_assert_eq!(name.len(), 17);
    // SAFETY: `name.len() == 17`, so reading 8 bytes at offset 0, 8 bytes at offset 8,
    // and 1 byte at offset 16 is valid.
    unsafe {
        let w0 = ptr::read_unaligned(name.as_ptr() as *const u64) | 0x2020_2020_2020_2020;
        let w1 = ptr::read_unaligned(name.as_ptr().add(8) as *const u64) | 0x2020_2020_2020_2020;
        let b16 = (*name.get_unchecked(16)) | 0x20;
        let t0 = u64::from_ne_bytes(*b"transfer");
        let t1 = u64::from_ne_bytes(*b"-encodin");
        ((w0 ^ t0) | (w1 ^ t1) == 0) & (b16 == b'g')
    }
}

/// Determines the body slice of a message whose head (`headers_len` bytes) was already parsed.
///
/// Looks at `Content-Length` / `Transfer-Encoding` and returns the body as a sub-slice of `buf`.
#[inline(always)]
pub(crate) fn frame_body<'buf>(
    headers: &[httparse::Header<'_>],
    buf: &'buf [u8],
    headers_len: usize,
) -> Result<&'buf [u8], WireError> {
    let mut content_len: Option<usize> = None;
    let mut is_chunked = false;

    for header in headers {
        let name = header.name.as_bytes();
        match name.len() {
            14 if !is_chunked && eq_content_length_14(name) => {
                content_len = parse_dec_usize(header.value);
            }
            17 if eq_transfer_encoding_17(name) => {
                is_chunked = is_chunked_slice(header.value);
            }
            _ => {}
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
        let digit = hex_nibble(b);
        if digit > 15 {
            return None;
        }
        val = val.checked_mul(16)?.checked_add(digit as usize)?;
    }

    Some(val)
}

/// Parses a decimal `usize` from a byte slice (e.g. for `Content-Length`).
///
/// Strips surrounding ASCII whitespace and parses decimal digits directly from ASCII bytes
/// without UTF-8 validation.
///
/// Returns `None` if the slice is empty, contains non-digit bytes, or overflows `usize`.
#[inline(always)]
pub(crate) fn parse_dec_usize(slice: &[u8]) -> Option<usize> {
    let slice = slice.trim_ascii();
    let len = slice.len();
    if len == 0 {
        return None;
    }

    // Fast path for up to 19 digits on 64-bit targets: cannot overflow `u64`/`usize`.
    #[cfg(target_pointer_width = "64")]
    if len <= 19 {
        let mut val: usize = 0;
        for &b in slice {
            let d = b.wrapping_sub(b'0');
            if d > 9 {
                return None;
            }
            val = val * 10 + d as usize;
        }
        return Some(val);
    }

    let mut val: usize = 0;
    for &b in slice {
        let d = b.wrapping_sub(b'0');
        if d > 9 {
            return None;
        }
        val = val.checked_mul(10)?.checked_add(d as usize)?;
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
fn hex_nibble(b: u8) -> u8 {
    let d = b.wrapping_sub(b'0');
    if d <= 9 {
        return d;
    }
    let a = (b | 0x20).wrapping_sub(b'a');
    if a <= 5 {
        return a + 10;
    }
    0xFF
}

#[inline(always)]
fn chunk_line_fast(buf: &[u8]) -> Option<(usize, usize)> {
    let mut val: usize = 0;
    let max_digits = (usize::BITS as usize) / 4;
    for (i, &b) in buf.iter().enumerate() {
        let digit = hex_nibble(b);
        if digit <= 15 {
            if i >= max_digits {
                return None;
            }
            val = (val << 4) | (digit as usize);
        } else if b == b'\r' && i > 0 && buf.get(i + 1) == Some(&b'\n') {
            return Some((val, i + 2));
        } else {
            return None;
        }
    }
    None
}

/// Returns `true` if the `Transfer-Encoding` header value indicates chunked transfer.
///
/// Handles comma-separated lists (e.g. `gzip, chunked`) by checking only the
/// **last** coding, as required by RFC 9112 §7.1. The comparison is
/// case-insensitive and tolerates surrounding whitespace on each token.
#[inline(always)]
pub(crate) fn is_chunked_slice(buf: &[u8]) -> bool {
    // Fast path for the overwhelmingly common exact literal `b"chunked"` (7 bytes).
    if buf.len() == 7 {
        return buf.eq_ignore_ascii_case(b"chunked");
    }
    let last = match memchr::memrchr(b',', buf) {
        Some(i) => &buf[i + 1..],
        None => buf,
    };
    last.trim_ascii().eq_ignore_ascii_case(b"chunked")
}

/// Appends a single HTTP header field (`name: value\r\n`) to `w`.
///
/// # Safety
///
/// `w` must have at least `name.len() + value.len() + 4` bytes of remaining capacity.
#[inline(always)]
pub(crate) unsafe fn write_header_unchecked(w: &mut FastBufWriter, name: &[u8], value: &[u8]) {
    unsafe {
        w.put(name);
        ptr::copy_nonoverlapping(b": ".as_ptr(), w.ptr, 2);
        w.ptr = w.ptr.add(2);
        w.put(value);
        w.put_crlf();
    }
}

/// Appends all HTTP headers in `headers` to `w`.
///
/// # Safety
///
/// `w` must have at least `headers_wire_len(headers)` bytes of remaining capacity.
#[inline(always)]
pub(crate) unsafe fn write_headers_unchecked(w: &mut FastBufWriter, headers: &http::HeaderMap) {
    for (name, value) in headers {
        unsafe {
            write_header_unchecked(w, name.as_str().as_bytes(), value.as_bytes());
        }
    }
}

/// Appends the request-target for `uri` directly to `w` without heap allocations.
///
/// # Safety
///
/// `w` must have at least `uri_len(uri)` bytes of remaining capacity.
#[inline(always)]
pub(crate) unsafe fn write_uri_unchecked(w: &mut FastBufWriter, uri: &http::Uri) {
    unsafe {
        if let Some(scheme) = uri.scheme_str() {
            w.put(scheme.as_bytes());
            w.put_byte(b':');
            if let Some(auth) = uri.authority() {
                ptr::copy_nonoverlapping(b"//".as_ptr(), w.ptr, 2);
                w.ptr = w.ptr.add(2);
                w.put(auth.as_str().as_bytes());
            }
            if let Some(pq) = uri.path_and_query() {
                w.put(pq.as_str().as_bytes());
            }
        } else {
            let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
            w.put(path.as_bytes());
        }
    }
}

/// Returns the byte length of the serialized request-target for `uri`.
#[inline(always)]
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
    use bytes::BytesMut;

    /// Helper for tests that previously used `extend_header`.
    fn extend_header(buf: &mut BytesMut, name: &[u8], value: &[u8]) {
        let mut v = Vec::with_capacity(name.len() + value.len() + 4);
        let mut w = FastBufWriter::new(&mut v);
        unsafe {
            write_header_unchecked(&mut w, name, value);
            v.set_len(name.len() + value.len() + 4);
        }
        buf.extend_from_slice(&v);
    }

    /// Helper for tests that previously used `extend_headers`.
    fn extend_headers(buf: &mut BytesMut, headers: &http::HeaderMap) {
        let len = headers_wire_len(headers);
        let mut v = Vec::with_capacity(len);
        let mut w = FastBufWriter::new(&mut v);
        unsafe {
            write_headers_unchecked(&mut w, headers);
            v.set_len(len);
        }
        buf.extend_from_slice(&v);
    }

    /// Helper for tests that previously used `extend_uri`.
    fn extend_uri(buf: &mut BytesMut, uri: &http::Uri) {
        let len = uri_len(uri);
        let mut v = Vec::with_capacity(len);
        let mut w = FastBufWriter::new(&mut v);
        unsafe {
            write_uri_unchecked(&mut w, uri);
            v.set_len(len);
        }
        buf.extend_from_slice(&v);
    }

    // ── Happy path ───────────────────────────────────────────────────────────

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

    // ── Chunk extensions (after ';') ─────────────────────────────────────────

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

    // ── Trailers ─────────────────────────────────────────────────────────────

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

    // ── Overflow safety ──────────────────────────────────────────────────────

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

    // ── parse_dec_usize direct unit tests ────────────────────────────────────

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

    // ── find_crlf / find_crlf_crlf direct unit tests ─────────────────────────

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
