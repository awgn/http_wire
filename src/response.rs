//! HTTP response encoding and decoding.
//!
//! This module handles the serialization of `http::Response` objects into wire-format bytes
//! and the parsing of raw bytes to determine response boundaries.
//!
//! # Response Encoding
//!
//! The [`WireEncode`] and [`WireEncodeAsync`] traits are implemented for [`http::Response`],
//! allowing you to serialize responses to bytes.
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

use bytes::Bytes;
use http::Request;
use http_body_util::Empty;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::io::duplex;
use tokio::sync::oneshot;

use crate::error::WireError;
use crate::util::{is_chunked_slice, parse_chunked_body, parse_usize};
use crate::wire::WireCapture;
use crate::{WireDecode, WireEncode, WireEncodeAsync};

pub use httparse::{Header, Response};

// Implementation of WireEncode for Response
impl<B> WireEncode for http::Response<B>
where
    B: hyper::body::Body + Clone + Send + Sync + 'static,
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

impl<B> WireEncodeAsync for http::Response<B>
where
    B::Data: Send + Sync + 'static,
    B: hyper::body::Body + Clone + Send + Sync + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    async fn encode_async(self) -> Result<Bytes, WireError> {
        use std::convert::Infallible;

        // Check HTTP version - only HTTP/1.1 and HTTP/1.0 are supported
        let version = self.version();
        if version != http::Version::HTTP_11 && version != http::Version::HTTP_10 {
            return Err(WireError::UnsupportedVersion);
        }

        let (client, server) = duplex(8192);
        let capture_server = WireCapture::new(server);
        let captured_ref = capture_server.captured.clone();

        let (tx, rx) = oneshot::channel::<Result<(), WireError>>();

        let handle = tokio::spawn(async move {
            let service = service_fn(move |_req: Request<hyper::body::Incoming>| {
                let res = self.clone();
                async move { Ok::<_, Infallible>(res) }
            });

            hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(capture_server), service)
                .await
        });

        let req = hyper::Request::builder()
            .method("GET")
            .uri("/")
            .header("host", "localhost")
            .body(Empty::<Bytes>::new())
            .unwrap();

        tokio::spawn(async move {
            let client_connection = hyper::client::conn::http1::Builder::new()
                .handshake(TokioIo::new(client))
                .await;

            match client_connection {
                Ok((mut sender, connection)) => {
                    tokio::spawn(connection);
                    // When send_request completes, the response has been received
                    let result = sender
                        .send_request(req)
                        .await
                        .map(|_| ())
                        .map_err(|e| WireError::Connection(Box::new(e)));
                    let _ = tx.send(result);
                }
                Err(e) => {
                    let _ = tx.send(Err(WireError::Connection(Box::new(e))));
                }
            }
        });

        // Wait for completion
        rx.await.map_err(|_| WireError::Sync)??;
        let _ = handle.await;

        let result = captured_ref.lock().clone();
        Ok(Bytes::from(result))
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
    pub head: httparse::Response<'headers, 'buf>,
    pub body: &'buf [u8],
}

impl<'headers, 'buf> FullResponse<'headers, 'buf> {
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
                        content_len = parse_usize(header.value);
                    } else if name.len() == 17 && name.eq_ignore_ascii_case(b"Transfer-Encoding") {
                        is_chunked = is_chunked_slice(header.value);
                    }
                }

                // Calculate body length
                if is_chunked {
                    let body_len = parse_chunked_body(&buf[headers_len..])
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
    use http::Response;
    use http_body_util::Full;
    #[test]
    fn test_response_sync_ok() {
        let response = http::Response::builder()
            .status(200)
            .header("Content-Type", "text/plain")
            .body(Full::new(Bytes::from("Hello")))
            .unwrap();

        let bytes = response.encode().unwrap();
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.contains("HTTP/1.1 200 OK"));
        assert!(output.contains("Hello"));
    }

    #[test]
    fn test_response_sync_404() {
        let response = http::Response::builder()
            .status(404)
            .body(Full::new(Bytes::from("Not Found")))
            .unwrap();

        let bytes = response.encode().unwrap();
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.contains("HTTP/1.1 404"));
        assert!(output.contains("Not Found"));
    }

    #[test]
    fn test_response_sync_http2_rejected() {
        let response = http::Response::builder()
            .status(200)
            .version(http::Version::HTTP_2)
            .body(Full::new(Bytes::from("Hello")))
            .unwrap();

        let result = response.encode();
        assert!(matches!(result, Err(WireError::UnsupportedVersion)));
    }

    #[tokio::test]
    async fn test_http1_capture() {
        let response = Response::builder()
            .status(200)
            .header("Content-Type", "text/plain")
            .body(Full::new(Bytes::from("Hello World")))
            .unwrap();

        let bytes = response.encode_async().await.unwrap();
        let output = String::from_utf8_lossy(&bytes);

        println!("HTTP/1.1 Response:\n{}", output);
        assert!(output.contains("HTTP/1.1 200 OK"));
        assert!(output.contains("Hello World"));
    }

    #[tokio::test]
    async fn test_response_is_complete() {
        let body = "Hello World";
        let response = Response::builder()
            .status(200)
            .header("Content-Type", "text/plain")
            .body(Full::new(Bytes::from(body)))
            .unwrap();

        let bytes = response.encode_async().await.unwrap();
        let output = String::from_utf8_lossy(&bytes);

        // Verify the response has proper HTTP structure
        assert!(output.contains("HTTP/1.1 200 OK"));
        // Headers and body are separated by \r\n\r\n
        assert!(
            output.contains("\r\n\r\n"),
            "Response should have header/body separator"
        );
        // Body should be present after the separator
        let parts: Vec<&str> = output.splitn(2, "\r\n\r\n").collect();
        assert_eq!(parts.len(), 2, "Response should have headers and body");
        assert!(parts[1].contains(body), "Body should contain the payload");
    }
    #[tokio::test]
    async fn test_response_to_wire() {
        let response = Response::builder()
            .status(200)
            .header("Content-Type", "text/plain")
            .body(Full::new(Bytes::from("Hello")))
            .unwrap();

        let bytes = response.encode_async().await.unwrap();
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.contains("HTTP/1.1 200 OK"));
        assert!(output.contains("Hello"));
    }

    #[tokio::test]
    async fn test_response_with_status_to_wire() {
        let response = Response::builder()
            .status(404)
            .body(Full::new(Bytes::from("Not Found")))
            .unwrap();

        let bytes = response.encode_async().await.unwrap();
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.contains("HTTP/1.1 404"));
        assert!(output.contains("Not Found"));
    }

    #[tokio::test]
    async fn test_http2_response_rejected() {
        let response = Response::builder()
            .status(200)
            .version(http::Version::HTTP_2)
            .body(Full::new(Bytes::from("Hello")))
            .unwrap();

        let result = response.encode_async().await;
        assert!(matches!(result, Err(WireError::UnsupportedVersion)));
    }

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
        // Buffer has extra data after the response - should return correct length
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let mut raw = response.to_vec();
        raw.extend_from_slice(b"extra garbage data");
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullResponse::decode(&raw, &mut headers);
        assert!(result.is_ok());
        let (_, len) = result.unwrap();
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
    fn test_full_response_fields_access() {
        // Test completo che accede a tutti i campi della FullResponse
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 11\r\n\r\nHello World";
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let result = FullResponse::decode(raw, &mut headers);
        assert!(result.is_ok());

        let (response, total_len) = result.unwrap();

        // Verifica status code
        assert_eq!(response.head.code, Some(200));
        assert_eq!(response.head.reason, Some("OK"));
        assert_eq!(response.head.version, Some(1));

        // Verifica headers
        assert_eq!(response.head.headers.len(), 2);
        assert_eq!(response.head.headers[0].name, "Content-Type");
        assert_eq!(response.head.headers[0].value, b"text/plain");
        assert_eq!(response.head.headers[1].name, "Content-Length");
        assert_eq!(response.head.headers[1].value, b"11");

        // Verifica body
        assert_eq!(response.body, b"Hello World");
        assert_eq!(total_len, raw.len());
    }

    #[test]
    #[should_panic(
        expected = "decode_uninit is not available for this type due to missing parse_with_uninit_headers method"
    )]
    fn test_decode_response_uninit_panics() {
        // FullResponse does not support decode_uninit because httparse::Response
        // does not have parse_with_uninit_headers method
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let mut headers = [const { std::mem::MaybeUninit::uninit() }; 16];
        let _result = FullResponse::decode_uninit(raw, &mut headers);
    }
}
