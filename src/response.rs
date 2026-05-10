//! HTTP response encoding and decoding.
//!
//! This module handles the serialization of `http::Response` objects into wire-format bytes
//! and the parsing of raw bytes to determine response boundaries.
//!
//! # Response Encoding
//!
//! The [`WireEncode`] trait is implemented for [`http::Response`],
//! allowing you to serialize responses to bytes via direct serialization
//! (no async runtime, no HTTP pipeline, no `Clone` constraint on the body).
//!
//! ```rust
//! use http_wire::WireEncode;
//! use http::Response;
//! use http_body_util::Full;
//! use bytes::Bytes;
//!
//! let response = Response::new(Full::new(Bytes::from("Hello")));
//! let wire_bytes = response.encode().unwrap();
//! ```
//!
//! # Response Decoding
//!
//! Use [`FullResponse`] to decode HTTP responses from raw bytes.

use bytes::{Bytes, BytesMut};

#[doc(no_inline)]
pub use httparse::{Header, Response};

use crate::error::WireError;
use crate::util::{chunked_body_len, is_chunked_slice, version_to_str};
use crate::{WireDecode, WireEncode};

impl<B> WireEncode for http::Response<B>
where
    B: http_body_util::BodyExt,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    fn encode(self) -> Result<Bytes, WireError> {
        let version = self.version();
        if version != http::Version::HTTP_11 && version != http::Version::HTTP_10 {
            return Err(WireError::UnsupportedVersion);
        }

        let (parts, body) = self.into_parts();
        let status = parts.status;
        let reason = status.canonical_reason().unwrap_or("Unknown");

        // Collect body bytes synchronously.
        // Works for any body type whose future completes without an async I/O driver
        // (e.g. `Full<Bytes>`, `Empty<Bytes>`).
        let body_bytes = futures::executor::block_on(body.collect())
            .map_err(|e| WireError::Collection(e.into()))?
            .to_bytes();

        // Pre-allocate a rough estimate: 48 bytes per header covers name + value
        // in the typical case; 16 bytes of fixed overhead absorbs the status line,
        // separators and the blank line. Both are intentionally coarse — the point
        // is to avoid the most common reallocations, not to be exact.
        let mut buf = BytesMut::with_capacity(
            parts.headers.len() * 48 + body_bytes.len() + 16,
        );

        // Status line: HTTP-version SP status-code SP reason-phrase CRLF
        buf.extend_from_slice(version_to_str(version).as_bytes());
        buf.extend_from_slice(b" ");
        buf.extend_from_slice(status.as_str().as_bytes());
        buf.extend_from_slice(b" ");
        buf.extend_from_slice(reason.as_bytes());
        buf.extend_from_slice(b"\r\n");

        // Header fields
        for (name, value) in &parts.headers {
            buf.extend_from_slice(name.as_str().as_bytes());
            buf.extend_from_slice(b": ");
            buf.extend_from_slice(value.as_bytes());
            buf.extend_from_slice(b"\r\n");
        }

        // Header/body separator
        buf.extend_from_slice(b"\r\n");

        // Body
        buf.extend_from_slice(&body_bytes);

        Ok(buf.freeze())
    }
}

/// Decoder for extracting HTTP response status code and message length.
///
/// Returns `(StatusCode, usize)` containing the status code and total length in bytes
/// of a complete HTTP response (headers + body), or `None` if incomplete or malformed.
///
/// Correctly handles status codes without bodies (1xx, 204, 304), `Content-Length`,
/// and `Transfer-Encoding: chunked`.
///
/// # Example
///
/// ```rust
/// use http_wire::WireDecode;
/// use http_wire::response::FullResponse;
///
/// let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
/// let mut headers = [httparse::EMPTY_HEADER; 16];
/// let (full_response, length) = FullResponse::decode(raw, &mut headers).unwrap();
/// assert_eq!(full_response.head.code, Some(200));
/// assert_eq!(length, raw.len());
/// ```
pub struct FullResponse<'headers, 'buf> {
    /// The parsed HTTP response headers and status line.
    ///
    /// Contains the HTTP version, status code, reason phrase, and headers.
    /// Use `head.code` to access the status code, `head.reason` for the reason phrase,
    /// and `head.headers` to iterate over the headers.
    pub head: httparse::Response<'headers, 'buf>,
    /// The response body as a byte slice.
    ///
    /// This is a reference into the original buffer passed to [`parse`](Self::parse)
    /// or [`decode`](WireDecode::decode). It contains the complete body content
    /// after decoding any transfer encodings (chunked or content-length).
    pub body: &'buf [u8],
}

impl<'headers, 'buf> FullResponse<'headers, 'buf> {
    /// Parse an HTTP response from raw bytes.
    ///
    /// This method parses the HTTP response from the provided buffer, extracting
    /// the status code, headers, and body. It correctly handles responses with
    /// no body (1xx, 204, 304), `Content-Length` specified bodies, and
    /// `Transfer-Encoding: chunked` bodies.
    ///
    /// # Arguments
    ///
    /// * `buf` - The buffer containing the raw HTTP response bytes
    ///
    /// # Returns
    ///
    /// Returns `Ok(total_len)` where `total_len` is the complete message length (headers + body).
    ///
    /// # Errors
    ///
    /// Returns [`WireError::PartialHead`] if headers are incomplete,
    /// [`WireError::HttparseError`] if header parsing fails,
    /// [`WireError::InvalidChunkedBody`] if chunked encoding is malformed,
    /// or [`WireError::IncompleteBody`] if the body is shorter than specified by `Content-Length`.
    pub fn parse(&mut self, buf: &'buf [u8]) -> Result<usize, WireError> {
        match self.head.parse(buf) {
            Ok(httparse::Status::Complete(headers_len)) => {
                let code = self.head.code.unwrap_or(200);

                // Fast path for responses that never have a body (1xx, 204, 304)
                if code == 204 || code == 304 || (100..200).contains(&code) {
                    self.body = &[];
                    return Ok(headers_len);
                }

                let mut content_len: Option<usize> = None;
                let mut is_chunked = false;

                // Scan headers for Content-Length or Transfer-Encoding
                for header in self.head.headers.iter() {
                    let name = header.name.as_bytes();
                    if name.len() == 14 && name.eq_ignore_ascii_case(b"Content-Length") {
                        content_len = std::str::from_utf8(header.value)
                            .ok()
                            .and_then(|s| s.parse().ok());
                    } else if name.len() == 17 && name.eq_ignore_ascii_case(b"Transfer-Encoding") {
                        is_chunked = is_chunked_slice(header.value);
                    }
                }

                // Calculate body length
                if is_chunked {
                    let body_len = chunked_body_len(&buf[headers_len..])
                        .ok_or(WireError::InvalidChunkedBody)?;
                    self.body = &buf[headers_len..headers_len + body_len];
                    Ok(headers_len + body_len)
                } else {
                    // If content-length is missing, length is 0
                    let body_len = content_len.unwrap_or(0);
                    let total = headers_len + body_len;
                    if buf.len() >= total {
                        self.body = &buf[headers_len..total];
                        Ok(total)
                    } else {
                        Err(WireError::IncompleteBody(total - buf.len()))
                    }
                }
            }
            Ok(httparse::Status::Partial) => Err(WireError::PartialHead),
            Err(err) => Err(err.into()),
        }
    }
}

impl<'headers, 'buf> WireDecode<'headers, 'buf> for FullResponse<'headers, 'buf> {
    fn decode(
        buf: &'buf [u8],
        headers: &'headers mut [Header<'buf>],
    ) -> Result<(Self, usize), WireError> {
        let mut full_response = FullResponse {
            head: httparse::Response::new(headers),
            body: &[],
        };

        let total = full_response.parse(buf)?;
        Ok((full_response, total))
    }

    // decode_uninit is not implemented for FullResponse because httparse::Response
    // does not provide parse_with_uninit_headers method.
    // The default implementation will panic with an appropriate message.
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http_body_util::{Empty, Full};

    // ── Encoding ────────────────────────────────────────────────────────────

    #[test]
    fn test_response_encode_200() {
        let response = http::Response::builder()
            .status(200)
            .header("Content-Type", "text/plain")
            .body(Full::new(Bytes::from("Hello")))
            .unwrap();

        let bytes = response.encode().unwrap();
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(output.contains("content-type: text/plain\r\n"));
        assert!(output.contains("\r\n\r\n"));
        assert!(output.ends_with("Hello"));
    }

    #[test]
    fn test_response_encode_404() {
        let response = http::Response::builder()
            .status(404)
            .body(Full::new(Bytes::from("Not Found")))
            .unwrap();

        let bytes = response.encode().unwrap();
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert!(output.ends_with("Not Found"));
    }

    #[test]
    fn test_response_encode_no_body() {
        let response = http::Response::builder()
            .status(204)
            .header("Server", "http_wire")
            .body(Empty::<Bytes>::new())
            .unwrap();

        let bytes = response.encode().unwrap();
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.starts_with("HTTP/1.1 204 No Content\r\n"));
        assert!(output.contains("server: http_wire\r\n"));
        // Body must be empty after the separator
        let parts: Vec<&str> = output.splitn(2, "\r\n\r\n").collect();
        assert_eq!(parts.len(), 2);
        assert!(parts[1].is_empty());
    }

    #[test]
    fn test_response_encode_http10() {
        let response = http::Response::builder()
            .status(200)
            .version(http::Version::HTTP_10)
            .body(Empty::<Bytes>::new())
            .unwrap();

        let bytes = response.encode().unwrap();
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.starts_with("HTTP/1.0 200 OK\r\n"));
    }

    #[test]
    fn test_response_encode_http2_rejected() {
        let response = http::Response::builder()
            .status(200)
            .version(http::Version::HTTP_2)
            .body(Full::new(Bytes::from("Hello")))
            .unwrap();

        let result = response.encode();
        assert!(matches!(result, Err(WireError::UnsupportedVersion)));
    }

    #[test]
    fn test_response_encode_header_body_separator() {
        let body = "Hello World";
        let response = http::Response::builder()
            .status(200)
            .header("Content-Type", "text/plain")
            .body(Full::new(Bytes::from(body)))
            .unwrap();

        let bytes = response.encode().unwrap();
        let output = String::from_utf8_lossy(&bytes);

        // Headers and body must be separated by exactly \r\n\r\n
        assert!(output.contains("\r\n\r\n"));
        let parts: Vec<&str> = output.splitn(2, "\r\n\r\n").collect();
        assert_eq!(parts.len(), 2, "response must have a headers section and a body section");
        assert_eq!(parts[1], body, "body must appear verbatim after the separator");
    }

    #[test]
    fn test_response_encode_multiple_headers() {
        let response = http::Response::builder()
            .status(200)
            .header("Content-Type", "application/json")
            .header("X-Request-Id", "abc-123")
            .header("Cache-Control", "no-cache")
            .body(Empty::<Bytes>::new())
            .unwrap();

        let bytes = response.encode().unwrap();
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.contains("content-type: application/json\r\n"));
        assert!(output.contains("x-request-id: abc-123\r\n"));
        assert!(output.contains("cache-control: no-cache\r\n"));
    }

    // ── Decoding ─────────────────────────────────────────────────────────────

    #[test]
    fn test_decode_response_no_body() {
        let raw = b"HTTP/1.1 204 No Content\r\nServer: test\r\n\r\n";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullResponse::decode(raw, &mut headers);
        assert!(result.is_ok());
        let (response, len) = result.unwrap();
        assert_eq!(response.head.code, Some(204));
        assert_eq!(len, raw.len());
        assert_eq!(response.body.len(), 0);
    }

    #[test]
    fn test_decode_response_with_content_length() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullResponse::decode(raw, &mut headers);
        assert!(result.is_ok());
        let (response, len) = result.unwrap();
        assert_eq!(response.head.code, Some(200));
        assert_eq!(len, raw.len());
        assert_eq!(response.body, b"hello");
    }

    #[test]
    fn test_decode_response_incomplete_body() {
        // Content-Length says 10, but body is only 5 bytes
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhello";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullResponse::decode(raw, &mut headers);
        assert!(matches!(result, Err(WireError::IncompleteBody(_))));
    }

    #[test]
    fn test_decode_response_incomplete_headers() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullResponse::decode(raw, &mut headers);
        assert!(matches!(result, Err(WireError::PartialHead)));
    }

    #[test]
    fn test_decode_response_chunked_encoding() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullResponse::decode(raw, &mut headers);
        assert!(result.is_ok());
        let (response, len) = result.unwrap();
        assert_eq!(response.head.code, Some(200));
        assert_eq!(len, raw.len());
    }

    #[test]
    fn test_decode_response_chunked_multiple_chunks() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullResponse::decode(raw, &mut headers);
        assert!(result.is_ok());
        let (response, len) = result.unwrap();
        assert_eq!(response.head.code, Some(200));
        assert_eq!(len, raw.len());
    }

    #[test]
    fn test_decode_response_chunked_incomplete() {
        // Missing final 0\r\n\r\n
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullResponse::decode(raw, &mut headers);
        assert!(matches!(result, Err(WireError::InvalidChunkedBody)));
    }

    #[test]
    fn test_decode_response_extra_data_after() {
        // Buffer has extra data after the response — decode should return correct length
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let mut raw = response.to_vec();
        raw.extend_from_slice(b"extra garbage data");
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let (_, len) = FullResponse::decode(&raw, &mut headers).unwrap();
        assert_eq!(len, response.len());
    }

    #[test]
    fn test_decode_response_chunked_case_insensitive() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: CHUNKED\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullResponse::decode(raw, &mut headers);
        assert!(result.is_ok());
        let (response, len) = result.unwrap();
        assert_eq!(response.head.code, Some(200));
        assert_eq!(len, raw.len());
    }

    #[test]
    fn test_decode_response_304_no_body() {
        // 304 responses never have a body
        let raw = b"HTTP/1.1 304 Not Modified\r\nETag: \"abc\"\r\n\r\n";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullResponse::decode(raw, &mut headers);
        assert!(result.is_ok());
        let (response, len) = result.unwrap();
        assert_eq!(response.head.code, Some(304));
        assert_eq!(len, raw.len());
        assert_eq!(response.body.len(), 0);
    }

    #[test]
    fn test_decode_response_1xx_no_body() {
        // 1xx responses never have a body
        let raw = b"HTTP/1.1 100 Continue\r\n\r\n";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullResponse::decode(raw, &mut headers);
        assert!(result.is_ok());
        let (response, len) = result.unwrap();
        assert_eq!(response.head.code, Some(100));
        assert_eq!(len, raw.len());
        assert_eq!(response.body.len(), 0);
    }

    #[test]
    fn test_decode_response_fields_access() {
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 11\r\n\r\nHello World";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let (response, total_len) = FullResponse::decode(raw, &mut headers).unwrap();

        assert_eq!(response.head.code, Some(200));
        assert_eq!(response.head.reason, Some("OK"));
        assert_eq!(response.head.version, Some(1));

        assert_eq!(response.head.headers.len(), 2);
        assert_eq!(response.head.headers[0].name, "Content-Type");
        assert_eq!(response.head.headers[0].value, b"text/plain");
        assert_eq!(response.head.headers[1].name, "Content-Length");
        assert_eq!(response.head.headers[1].value, b"11");

        assert_eq!(response.body, b"Hello World");
        assert_eq!(total_len, raw.len());
    }

    #[test]
    #[should_panic(
        expected = "decode_uninit is not available for this type due to missing parse_with_uninit_headers method"
    )]
    fn test_decode_response_uninit_panics() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let mut headers = [const { std::mem::MaybeUninit::uninit() }; 16];
        let _result = FullResponse::decode_uninit(raw, &mut headers);
    }
}
