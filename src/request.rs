//! HTTP request encoding and decoding.
//!
//! This module provides:
//! - [`WireEncode`] implementation for `http::Request<B>` - encodes requests to wire format
//! - [`RequestLength`] - parses raw bytes to determine complete request length
//!
//! Both `Content-Length` and `Transfer-Encoding: chunked` are fully supported.

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Empty;
use hyper_util::rt::TokioIo;
use tokio::io::duplex;
use tokio::sync::oneshot;

use crate::error::WireError;
use crate::util::{is_chunked_slice, parse_chunked_body, parse_usize};
use crate::wire::WireCapture;
use crate::{ WireDecode, WireEncode};

impl<B> WireEncode for http::Request<B>
where
    B::Data: Send + Sync + 'static,
    B: http_body_util::BodyExt + Send + Sync + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    #[inline]
    async fn encode(self) -> Result<Bytes, WireError> {
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
            let service = service_fn(move |_req: Request<hyper::body::Incoming>| {
                // Signal that the request has been received
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(Ok(()));
                }
                async move {
                    // Return a minimal response
                    Ok::<_, Infallible>(Response::new(Empty::<Bytes>::new()))
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
/// Supports `Content-Length`, `Transfer-Encoding: chunked`, and bodyless requests.
///
/// # Example
///
/// ```rust
/// use http_wire::WireDecode;
/// use http_wire::request::RequestLength;
///
/// let raw = b"POST /api HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\n\r\nhello";
/// let length = RequestLength::decode(raw).unwrap();
/// assert_eq!(length, raw.len());
/// ```
pub struct RequestLength;

impl WireDecode for RequestLength {
    type Output = usize;

    fn decode(buf: &[u8]) -> Option<Self::Output> {
        // Keep headers on stack.
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut req = httparse::Request::new(&mut headers);

        match req.parse(buf) {
            Ok(httparse::Status::Complete(headers_len)) => {
                let mut content_len: Option<usize> = None;
                let mut is_chunked = false;

                // Scan headers for Content-Length and Transfer-Encoding
                for header in req.headers.iter() {
                    let name = header.name.as_bytes();
                    if name.len() == 14 && name.eq_ignore_ascii_case(b"Content-Length") {
                        content_len = parse_usize(header.value);
                    } else if name.len() == 17 && name.eq_ignore_ascii_case(b"Transfer-Encoding") {
                        is_chunked = is_chunked_slice(header.value);
                    }
                }

                // Calculate body length
                if is_chunked {
                    let body_len = parse_chunked_body(&buf[headers_len..])?;
                    Some(headers_len + body_len)
                } else {
                    // If content-length is missing, length is 0
                    let len = content_len.unwrap_or(0);
                    let total = headers_len + len;
                    if buf.len() >= total {
                        Some(total)
                    } else {
                        None
                    }
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;

    #[tokio::test]
    async fn test_request_to_wire() {
        let request = Request::builder()
            .method("GET")
            .uri("/api/test")
            .header("Host", "example.com")
            .body(Empty::<Bytes>::new())
            .unwrap();

        let bytes = request.encode().await.unwrap();
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.contains("GET /api/test HTTP/1.1"));
        assert!(output.contains("host: example.com"));
    }

    #[tokio::test]
    async fn test_request_with_body_to_wire() {
        let body = r#"{"test":"data"}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/api/submit")
            .header("Host", "example.com")
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(body)))
            .unwrap();

        let bytes = request.encode().await.unwrap();
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.contains("POST /api/submit HTTP/1.1"));
        assert!(output.contains(body));
    }

    #[tokio::test]
    async fn test_http2_request_rejected() {
        let request = Request::builder()
            .method("GET")
            .uri("/")
            .version(http::Version::HTTP_2)
            .body(Empty::<Bytes>::new())
            .unwrap();

        let result = request.encode().await;
        assert!(matches!(result, Err(WireError::UnsupportedVersion)));
    }

    #[test]
    fn test_decode_request_no_body() {
        let raw = b"GET /api/users HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let result = RequestLength::decode(raw);
        assert_eq!(result, Some(raw.len()));
    }

    #[test]
    fn test_decode_request_with_content_length() {
        let raw = b"POST /api/users HTTP/1.1\r\nHost: example.com\r\nContent-Length: 14\r\n\r\n{\"name\":\"foo\"}";
        let result = RequestLength::decode(raw);
        assert_eq!(result, Some(raw.len()));
    }

    #[test]
    fn test_decode_request_incomplete_body() {
        // Content-Length says 13, but body is only 5 bytes
        let raw =
            b"POST /api/users HTTP/1.1\r\nHost: example.com\r\nContent-Length: 13\r\n\r\nhello";
        let result = RequestLength::decode(raw);
        assert_eq!(result, None);
    }

    #[test]
    fn test_decode_request_incomplete_headers() {
        let raw = b"POST /api/users HTTP/1.1\r\nHost: example.com\r\n";
        let result = RequestLength::decode(raw);
        assert_eq!(result, None);
    }

    #[test]
    fn test_decode_request_chunked_encoding() {
        let raw = b"POST /api/data HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let result = RequestLength::decode(raw);
        assert_eq!(result, Some(raw.len()));
    }

    #[test]
    fn test_decode_request_chunked_multiple_chunks() {
        let raw = b"POST /api/data HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let result = RequestLength::decode(raw);
        assert_eq!(result, Some(raw.len()));
    }

    #[test]
    fn test_decode_request_chunked_incomplete() {
        // Missing final 0\r\n\r\n
        let raw = b"POST /api/data HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n";
        let result = RequestLength::decode(raw);
        assert_eq!(result, None);
    }

    #[test]
    fn test_decode_request_extra_data_after() {
        // Buffer has extra data after the request - should return correct length
        let request = b"GET /api/users HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let mut raw = request.to_vec();
        raw.extend_from_slice(b"extra garbage data");
        let result = RequestLength::decode(&raw);
        assert_eq!(result, Some(request.len()));
    }

    #[test]
    fn test_decode_request_chunked_case_insensitive() {
        let raw = b"POST /api/data HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: CHUNKED\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let result = RequestLength::decode(raw);
        assert_eq!(result, Some(raw.len()));
    }
}
