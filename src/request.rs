//! HTTP request encoding and decoding.
//!
//! This module handles the serialization of `http::Request` objects into wire-format bytes
//! and the parsing of raw bytes to determine request boundaries.
//!
//! # Request Encoding
//!
//! The [`WireEncode`] and [`WireEncodeAsync`] traits are implemented for [`http::Request`],
//! allowing you to serialize requests to bytes.
//!

use bytes::Bytes;
use http_body_util::Empty;
use hyper_util::rt::TokioIo;
use tokio::io::duplex;
use tokio::sync::oneshot;

pub use httparse::{Header, Request};

use crate::error::WireError;
use crate::util::{is_chunked_slice, parse_chunked_body, parse_usize};
use crate::wire::WireCapture;
use crate::{WireDecode, WireEncode, WireEncodeAsync};
use std::mem::MaybeUninit;

// Implementation of WireEncode for Request
impl<B> WireEncode for http::Request<B>
where
    B: http_body_util::BodyExt + Send + Sync + 'static,
    B::Data: Send + Sync + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    fn encode(self) -> Result<Bytes, WireError> {
        // Create a minimal single-threaded runtime
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| WireError::Connection(Box::new(e)))?;

        // Block on the async encode method
        rt.block_on(self.encode_async())
    }
}

impl<B> WireEncodeAsync for http::Request<B>
where
    B::Data: Send + Sync + 'static,
    B: http_body_util::BodyExt + Send + Sync + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    #[inline]
    async fn encode_async(self) -> Result<Bytes, WireError> {
        use hyper::service::service_fn;
        use std::convert::Infallible;

        // Check HTTP version - only HTTP/1.1 and HTTP/1.0 are supported
        let version = self.version();
        if version != http::Version::HTTP_11 && version != http::Version::HTTP_10 {
            return Err(WireError::UnsupportedVersion);
        }

        let (client, server) = duplex(8192);
        let capture_client = WireCapture::new(client);
        let captured_ref = capture_client.captured.clone();

        let (tx, rx) = oneshot::channel::<Result<(), WireError>>();

        // Spawn a mock server that will accept the connection and read the request
        let server_handle = tokio::spawn(async move {
            let tx = std::sync::Mutex::new(Some(tx));
            let service = service_fn(move |_req: http::Request<hyper::body::Incoming>| {
                // Signal that the request has been received
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(Ok(()));
                }
                async move {
                    // Return a minimal response
                    Ok::<_, Infallible>(http::Response::new(Empty::<Bytes>::new()))
                }
            });

            hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(server), service)
                .await
        });

        // Send the request through the client side and capture what's written
        let client_handle = tokio::spawn(async move {
            let client_connection = hyper::client::conn::http1::Builder::new()
                .handshake(TokioIo::new(capture_client))
                .await;

            match client_connection {
                Ok((mut sender, connection)) => {
                    // Spawn the connection driver
                    tokio::spawn(connection);

                    // Send the request
                    sender
                        .send_request(self)
                        .await
                        .map(|_| ())
                        .map_err(|e| WireError::Connection(Box::new(e)))
                }
                Err(e) => Err(WireError::Connection(Box::new(e))),
            }
        });

        // Wait for the server to receive the request
        rx.await.map_err(|_| WireError::Sync)??;

        // Cleanup
        client_handle.abort();
        server_handle.abort();

        let result = captured_ref.lock().clone();
        Ok(Bytes::from(result))
    }
}

/// Decoder for determining HTTP request message length.
///
/// Returns the total length in bytes of a complete HTTP request (headers + body),
/// or `None` if the request is incomplete or malformed.
///
/// Supports `Content-Length`, `Transfer-Encoding: chunked`, and body-less requests.
///
pub struct FullRequest<'headers, 'buf> {
    /// The parsed HTTP request headers and request line.
    ///
    /// Contains the HTTP method, request path, HTTP version, and headers.
    /// Use `head.method` to access the method, `head.path` for the request path,
    /// and `head.headers` to iterate over the headers.
    pub head: httparse::Request<'headers, 'buf>,
    /// The request body as a byte slice.
    ///
    /// This is a reference into the original buffer passed to [`parse`](Self::parse)
    /// or [`decode`](WireDecode::decode). It contains the complete body content
    /// after decoding any transfer encodings (chunked or content-length).
    pub body: &'buf [u8],
}

impl<'headers, 'buf> FullRequest<'headers, 'buf> {
    /// Core parsing logic shared between [`parse`](Self::parse) and [`parse_uninit`](Self::parse_uninit).
    ///
    /// This method processes the body of an HTTP request after headers have been parsed.
    /// It examines `Content-Length` and `Transfer-Encoding` headers to determine how
    /// many bytes constitute the complete message.
    ///
    /// # Arguments
    ///
    /// * `buf` - The buffer containing the raw HTTP message bytes
    /// * `headers_len` - The length of the headers section in bytes (including the `\r\n\r\n` terminator)
    ///
    /// # Returns
    ///
    /// Returns `Ok(total_len)` where `total_len` is the complete message length (headers + body).
    ///
    /// # Errors
    ///
    /// Returns [`WireError::InvalidChunkedBody`] if chunked encoding is malformed,
    /// or [`WireError::IncompleteBody`] if the body is shorter than specified by `Content-Length`.
    fn parse_core(&mut self, buf: &'buf [u8], headers_len: usize) -> Result<usize, WireError> {
        let mut content_len: Option<usize> = None;
        let mut is_chunked = false;

        // Scan headers for Content-Length or Transfer-Encoding
        for header in self.head.headers.iter() {
            let name = header.name.as_bytes();
            if name.len() == 14 && name.eq_ignore_ascii_case(b"Content-Length") {
                content_len = parse_usize(header.value);
            } else if name.len() == 17 && name.eq_ignore_ascii_case(b"Transfer-Encoding") {
                is_chunked = is_chunked_slice(header.value);
            }
        }

        // Calculate body length
        if is_chunked {
            let body_len =
                parse_chunked_body(&buf[headers_len..]).ok_or(WireError::InvalidChunkedBody)?;
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

    /// Parse an HTTP request using initialized headers storage.
    ///
    /// This method parses the HTTP request from the provided buffer, using
    /// pre-initialized header storage. It is compatible with
    /// [`httparse::Request::parse`](httparse::Request::parse).
    ///
    /// For a version that avoids initializing headers (performance optimization),
    /// see [`parse_uninit`](Self::parse_uninit).
    ///
    /// # Arguments
    ///
    /// * `buf` - The buffer containing the raw HTTP message bytes
    ///
    /// # Returns
    ///
    /// Returns `Ok(total_len)` where `total_len` is the complete message length (headers + body).
    ///
    /// # Errors
    ///
    /// Returns [`WireError::PartialHead`] if headers are incomplete,
    /// [`WireError::HttparseError`] if header parsing fails,
    /// or errors from [`parse_core`](Self::parse_core) for body-related issues.
    pub fn parse(&mut self, buf: &'buf [u8]) -> Result<usize, WireError> {
        match self.head.parse(buf) {
            Ok(httparse::Status::Complete(headers_len)) => self.parse_core(buf, headers_len),
            Ok(httparse::Status::Partial) => Err(WireError::PartialHead),
            Err(err) => Err(err.into()),
        }
    }

    /// Parse an HTTP request using uninitialized headers storage (performance optimization).
    ///
    /// This method avoids the overhead of initializing the headers array before parsing
    /// by using [`httparse::Request::parse_with_uninit_headers`](httparse::Request::parse_with_uninit_headers).
    /// It is particularly useful when parsing many requests with the same headers buffer.
    ///
    /// For the standard version using initialized headers, see [`parse`](Self::parse).
    ///
    /// # Arguments
    ///
    /// * `buf` - The buffer containing the raw HTTP message bytes
    /// * `headers` - A mutable slice of uninitialized `Header` structs to store parsed headers
    ///
    /// # Returns
    ///
    /// Returns `Ok(total_len)` where `total_len` is the complete message length (headers + body).
    ///
    /// # Errors
    ///
    /// Returns [`WireError::PartialHead`] if headers are incomplete,
    /// [`WireError::HttparseError`] if header parsing fails,
    /// or errors from [`parse_core`](Self::parse_core) for body-related issues.
    pub fn parse_uninit(
        &mut self,
        buf: &'buf [u8],
        headers: &'headers mut [MaybeUninit<Header<'buf>>],
    ) -> Result<usize, WireError> {
        match self.head.parse_with_uninit_headers(buf, headers) {
            Ok(httparse::Status::Complete(headers_len)) => self.parse_core(buf, headers_len),
            Ok(httparse::Status::Partial) => Err(WireError::PartialHead),
            Err(err) => Err(err.into()),
        }
    }
}

impl<'headers, 'buf> WireDecode<'headers, 'buf> for FullRequest<'headers, 'buf> {
    fn decode(
        buf: &'buf [u8],
        headers: &'headers mut [Header<'buf>],
    ) -> Result<(Self, usize), WireError> {
        let mut full_request = FullRequest {
            head: httparse::Request::new(headers),
            body: &[],
        };

        let total = full_request.parse(buf)?;
        Ok((full_request, total))
    }

    fn decode_uninit(
        buf: &'buf [u8],
        headers: &'headers mut [MaybeUninit<Header<'buf>>],
    ) -> Result<(Self, usize), WireError> {
        let mut full_request = FullRequest {
            head: httparse::Request::new(&mut []),
            body: &[],
        };

        let total = full_request.parse_uninit(buf, headers)?;
        Ok((full_request, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{Empty, Full};

    #[test]
    fn test_request_sync_no_body() {
        let request = http::Request::builder()
            .method("GET")
            .uri("/api/test")
            .header("Host", "example.com")
            .body(Empty::<Bytes>::new())
            .unwrap();

        let bytes = request.encode().unwrap();
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.contains("GET /api/test HTTP/1.1"));
        assert!(output.contains("host: example.com"));
    }

    #[test]
    fn test_request_sync_with_body() {
        let body = r#"{"test":"data"}"#;
        let request = http::Request::builder()
            .method("POST")
            .uri("/api/submit")
            .header("Host", "example.com")
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(body)))
            .unwrap();

        let bytes = request.encode().unwrap();
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.contains("POST /api/submit HTTP/1.1"));
        assert!(output.contains(body));
    }

    #[test]
    fn test_request_sync_http2_rejected() {
        let request = http::Request::builder()
            .method("GET")
            .uri("/")
            .version(http::Version::HTTP_2)
            .body(Empty::<Bytes>::new())
            .unwrap();

        let result = request.encode();
        assert!(matches!(result, Err(WireError::UnsupportedVersion)));
    }

    #[tokio::test]
    async fn test_request_to_wire() {
        let request = http::Request::builder()
            .method("GET")
            .uri("/api/test")
            .header("Host", "example.com")
            .body(Empty::<Bytes>::new())
            .unwrap();

        let bytes = request.encode_async().await.unwrap();
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.contains("GET /api/test HTTP/1.1"));
        assert!(output.contains("host: example.com"));
    }

    #[tokio::test]
    async fn test_request_with_body_to_wire() {
        let body = r#"{"test":"data"}"#;
        let request = http::Request::builder()
            .method("POST")
            .uri("/api/submit")
            .header("Host", "example.com")
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(body)))
            .unwrap();

        let bytes = request.encode_async().await.unwrap();
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.contains("POST /api/submit HTTP/1.1"));
        assert!(output.contains(body));
    }

    #[tokio::test]
    async fn test_http2_request_rejected() {
        let request = http::Request::builder()
            .method("GET")
            .uri("/")
            .version(http::Version::HTTP_2)
            .body(Empty::<Bytes>::new())
            .unwrap();

        let result = request.encode_async().await;
        assert!(matches!(result, Err(WireError::UnsupportedVersion)));
    }

    #[test]
    fn test_decode_request_no_body() {
        let raw = b"GET /api/users HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullRequest::decode(raw, &mut headers);
        assert!(result.is_ok());
    }

    #[test]
    fn test_decode_request_with_content_length() {
        let raw = b"POST /api/users HTTP/1.1\r\nHost: example.com\r\nContent-Length: 14\r\n\r\n{\"name\":\"foo\"}";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullRequest::decode(raw, &mut headers);
        assert!(result.is_ok());
    }

    #[test]
    fn test_decode_request_incomplete_body() {
        // Content-Length says 13, but body is only 5 bytes
        let raw =
            b"POST /api/users HTTP/1.1\r\nHost: example.com\r\nContent-Length: 13\r\n\r\nhello";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullRequest::decode(raw, &mut headers);
        assert!(matches!(result, Err(WireError::IncompleteBody(_))));
    }

    #[test]
    fn test_decode_request_incomplete_headers() {
        let raw = b"POST /api/users HTTP/1.1\r\nHost: example.com\r\n";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullRequest::decode(raw, &mut headers);
        assert!(matches!(result, Err(WireError::PartialHead)));
    }

    #[test]
    fn test_decode_request_chunked_encoding() {
        let raw = b"POST /api/data HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullRequest::decode(raw, &mut headers);
        assert!(result.is_ok());
    }

    #[test]
    fn test_decode_request_chunked_multiple_chunks() {
        let raw = b"POST /api/data HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullRequest::decode(raw, &mut headers);
        assert!(result.is_ok());
    }

    #[test]
    fn test_decode_request_chunked_incomplete() {
        // Missing final 0\r\n\r\n
        let raw = b"POST /api/data HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullRequest::decode(raw, &mut headers);
        assert!(matches!(result, Err(WireError::InvalidChunkedBody)));
    }

    #[test]
    fn test_decode_request_extra_data_after() {
        // Buffer has extra data after the request - should return correct length
        let request = b"GET /api/users HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let mut raw = request.to_vec();
        raw.extend_from_slice(b"extra garbage data");
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullRequest::decode(&raw, &mut headers);
        assert!(result.is_ok());
    }

    #[test]
    fn test_decode_request_chunked_case_insensitive() {
        let raw = b"POST /api/data HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: CHUNKED\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullRequest::decode(raw, &mut headers);
        assert!(result.is_ok());
    }

    #[test]
    fn test_decode_request_uninit_no_body() {
        let raw = b"GET /api/users HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let mut headers = [const { MaybeUninit::uninit() }; 16];
        let result = FullRequest::decode_uninit(raw, &mut headers);
        assert!(result.is_ok());
    }

    #[test]
    fn test_decode_request_uninit_with_body() {
        let raw = b"POST /api/users HTTP/1.1\r\nHost: example.com\r\nContent-Length: 14\r\n\r\n{\"name\":\"foo\"}";
        let mut headers = [const { MaybeUninit::uninit() }; 16];
        let result = FullRequest::decode_uninit(raw, &mut headers);
        assert!(result.is_ok());
        let (req, _) = result.unwrap();
        assert_eq!(req.body, b"{\"name\":\"foo\"}");
    }
}
