use bytes::Bytes;
use http::{Request, StatusCode};
use http_body_util::Empty;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::io::duplex;
use tokio::sync::oneshot;

use crate::error::WireError;
use crate::util::{is_chunked_slice, parse_chunked_body, parse_usize};
use crate::wire::WireCapture;
use crate::{WireDecode, WireEncode};

impl<B> WireEncode for http::Response<B>
where
    B::Data: Send + Sync + 'static,
    B: hyper::body::Body + Clone + Send + Sync + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    async fn encode(self) -> Result<Bytes, WireError> {
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

pub struct ResponseStatusCode;

impl WireDecode for ResponseStatusCode {
    type Output = (StatusCode, usize);

    fn decode(buf: &[u8]) -> Option<Self::Output> {
        // Determine headers array size. 32 is standard, but keeping it on stack is fast.
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut res = httparse::Response::new(&mut headers);

        // 1. Parse Headers
        match res.parse(buf) {
            Ok(httparse::Status::Complete(headers_len)) => {
                let code = res.code.unwrap_or(200);
                let status_code = StatusCode::from_u16(code).unwrap_or(StatusCode::OK);

                // Fast path for responses that never have a body (1xx, 204, 304)
                // Note: 1xx responses usually continue, but for a single response frame logic:
                if code == 204 || code == 304 || (code >= 100 && code < 200) {
                    return Some((status_code, headers_len));
                }

                let mut content_len: Option<usize> = None;
                let mut is_chunked = false;

                // 2. Scan headers (Optimized Loop)
                for header in res.headers.iter() {
                    let name = header.name.as_bytes();
                    if name.len() == 14 && name.eq_ignore_ascii_case(b"Content-Length") {
                        // Manual fast parse u64
                        content_len = parse_usize(header.value);
                    } else if name.len() == 17 && name.eq_ignore_ascii_case(b"Transfer-Encoding") {
                        // Check if value contains "chunked" (case insensitive)
                        // We check the last 7 bytes usually, or just linear scan.
                        // Given specific HTTP formatting, it's often exactly "chunked".
                        is_chunked = is_chunked_slice(header.value);
                    }
                }

                // 3. Calculate Body End
                if is_chunked {
                    let body_len = parse_chunked_body(&buf[headers_len..])?;
                    Some((status_code, headers_len + body_len))
                } else {
                    // If content-length is missing, length is 0 (unless connection close,
                    // but for parsing complete frames we assume 0).
                    let len = content_len.unwrap_or(0);
                    let total = headers_len + len;
                    if buf.len() >= total {
                        Some((status_code, total))
                    } else {
                        None
                    }
                }
            }
            _ => None, // Partial or Error
        }
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;
    use http::Response;

    #[tokio::test]
    async fn test_http1_capture() {
        let response = Response::builder()
            .status(200)
            .header("Content-Type", "text/plain")
            .body(Full::new(Bytes::from("Hello World")))
            .unwrap();

        let bytes = response.encode().await.unwrap();
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

        let bytes = response.encode().await.unwrap();
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

        let bytes = response.encode().await.unwrap();
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

        let bytes = response.encode().await.unwrap();
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

        let result = response.encode().await;
        assert!(matches!(result, Err(WireError::UnsupportedVersion)));
    }

    #[test]
    fn test_decode_response_no_body() {
        let raw = b"HTTP/1.1 204 No Content\r\nServer: test\r\n\r\n";
        let result = ResponseStatusCode::decode(raw);
        assert_eq!(result, Some((StatusCode::NO_CONTENT, raw.len())));
    }

    #[test]
    fn test_decode_response_with_content_length() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let result = ResponseStatusCode::decode(raw);
        assert_eq!(result, Some((StatusCode::OK, raw.len())));
    }

    #[test]
    fn test_decode_response_incomplete_body() {
        // Content-Length says 10, but body is only 5 bytes
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhello";
        let result = ResponseStatusCode::decode(raw);
        assert_eq!(result, None);
    }

    #[test]
    fn test_decode_response_incomplete_headers() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n";
        let result = ResponseStatusCode::decode(raw);
        assert_eq!(result, None);
    }

    #[test]
    fn test_decode_response_chunked_encoding() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let result = ResponseStatusCode::decode(raw);
        assert_eq!(result, Some((StatusCode::OK, raw.len())));
    }

    #[test]
    fn test_decode_response_chunked_multiple_chunks() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let result = ResponseStatusCode::decode(raw);
        assert_eq!(result, Some((StatusCode::OK, raw.len())));
    }

    #[test]
    fn test_decode_response_chunked_incomplete() {
        // Missing final 0\r\n\r\n
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n";
        let result = ResponseStatusCode::decode(raw);
        assert_eq!(result, None);
    }

    #[test]
    fn test_decode_response_extra_data_after() {
        // Buffer has extra data after the response - should return correct length
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let mut raw = response.to_vec();
        raw.extend_from_slice(b"extra garbage data");
        let result = ResponseStatusCode::decode(&raw);
        assert_eq!(result, Some((StatusCode::OK, response.len())));
    }

    #[test]
    fn test_decode_response_chunked_case_insensitive() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: CHUNKED\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let result = ResponseStatusCode::decode(raw);
        assert_eq!(result, Some((StatusCode::OK, raw.len())));
    }

    #[test]
    fn test_decode_response_304_no_body() {
        // 304 responses never have a body
        let raw = b"HTTP/1.1 304 Not Modified\r\nETag: \"abc\"\r\n\r\n";
        let result = ResponseStatusCode::decode(raw);
        assert_eq!(result, Some((StatusCode::NOT_MODIFIED, raw.len())));
    }

    #[test]
    fn test_decode_response_1xx_no_body() {
        // 1xx responses never have a body
        let raw = b"HTTP/1.1 100 Continue\r\n\r\n";
        let result = ResponseStatusCode::decode(raw);
        assert_eq!(result, Some((StatusCode::CONTINUE, raw.len())));
    }
}
