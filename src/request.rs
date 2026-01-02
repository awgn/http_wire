use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Empty;
use hyper_util::rt::TokioIo;
use tokio::io::duplex;
use tokio::sync::oneshot;

use crate::error::WireError;
use crate::util::{is_chunked_slice, parse_chunked_body, parse_usize};
use crate::wire::WireCapture;
use crate::{DummyRequest, WireDecode, WireEncode};

impl<B> WireEncode for http::Request<B>
where
    B: http_body_util::BodyExt + Send + Sync + 'static,
    B::Error: std::error::Error + Send + Sync + 'static,
    B::Data: Send + Sync + 'static,
{
    #[inline]
    async fn encode(self) -> Result<Bytes, WireError> {
        let bytes = to_bytes(self).await?;
        Ok(Bytes::from(bytes))
    }
}

impl WireDecode for DummyRequest {
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

/// Serialize an HTTP request to raw bytes using hyper's HTTP/1.1 serialization.
/// This uses a duplex stream to capture the exact bytes that would be sent over the wire.
async fn to_bytes<B>(request: Request<B>) -> Result<Vec<u8>, WireError>
where
    B: http_body_util::BodyExt + Send + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    use hyper::service::service_fn;
    use std::convert::Infallible;

    // Check HTTP version - only HTTP/1.1 and HTTP/1.0 are supported
    let version = request.version();
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
                    .send_request(request)
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
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;

    #[tokio::test]
    async fn test_get_request_to_bytes() {
        let request = Request::builder()
            .method("GET")
            .uri("/api/users")
            .header("Host", "example.com")
            .header("Accept", "application/json")
            .body(Empty::<Bytes>::new())
            .unwrap();

        let bytes = to_bytes(request).await.unwrap();
        let output = String::from_utf8_lossy(&bytes);

        println!("HTTP/1.1 Request:\n{}", output);
        assert!(output.contains("GET /api/users HTTP/1.1"));
        assert!(output.contains("host: example.com"));
        assert!(output.contains("accept: application/json"));
    }

    #[tokio::test]
    async fn test_post_request_with_body() {
        let body = r#"{"name":"John","age":30}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/api/users")
            .header("Host", "api.example.com")
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(body)))
            .unwrap();

        let bytes = to_bytes(request).await.unwrap();
        let output = String::from_utf8_lossy(&bytes);

        println!("HTTP/1.1 POST Request:\n{}", output);
        assert!(output.contains("POST /api/users HTTP/1.1"));
        assert!(output.contains("host: api.example.com"));
        assert!(output.contains("content-type: application/json"));
        assert!(output.contains(body));
    }

    #[tokio::test]
    async fn test_request_with_multiple_headers() {
        let request = Request::builder()
            .method("GET")
            .uri("/")
            .header("Host", "localhost")
            .header("User-Agent", "test-client/1.0")
            .header("Accept", "*/*")
            .header("Accept-Language", "en-US")
            .header("Connection", "keep-alive")
            .body(Empty::<Bytes>::new())
            .unwrap();

        let bytes = to_bytes(request).await.unwrap();
        let output = String::from_utf8_lossy(&bytes);

        println!("HTTP/1.1 Request with multiple headers:\n{}", output);
        assert!(output.contains("GET / HTTP/1.1"));
        assert!(output.contains("host: localhost"));
        assert!(output.contains("user-agent: test-client/1.0"));
        assert!(output.contains("accept: */*"));
        assert!(output.contains("accept-language: en-US"));
    }

    #[tokio::test]
    async fn test_put_request_with_body() {
        let body = "updated content";
        let request = Request::builder()
            .method("PUT")
            .uri("/resources/123")
            .header("Host", "example.com")
            .header("Content-Type", "text/plain")
            .body(Full::new(Bytes::from(body)))
            .unwrap();

        let bytes = to_bytes(request).await.unwrap();
        let output = String::from_utf8_lossy(&bytes);

        println!("HTTP/1.1 PUT Request:\n{}", output);
        assert!(output.contains("PUT /resources/123 HTTP/1.1"));
        assert!(output.contains(body));
    }

    #[tokio::test]
    async fn test_delete_request() {
        let request = Request::builder()
            .method("DELETE")
            .uri("/resources/456")
            .header("Host", "example.com")
            .body(Empty::<Bytes>::new())
            .unwrap();

        let bytes = to_bytes(request).await.unwrap();
        let output = String::from_utf8_lossy(&bytes);

        println!("HTTP/1.1 DELETE Request:\n{}", output);
        assert!(output.contains("DELETE /resources/456 HTTP/1.1"));
        assert!(output.contains("host: example.com"));
    }

    #[tokio::test]
    async fn test_request_is_complete() {
        let body = r#"{"key":"value"}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/api/data")
            .header("Host", "example.com")
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(body)))
            .unwrap();

        let bytes = to_bytes(request).await.unwrap();
        let output = String::from_utf8_lossy(&bytes);

        // Verify the request has proper HTTP structure
        assert!(output.contains("POST /api/data HTTP/1.1"));
        // Headers and body are separated by \r\n\r\n
        assert!(
            output.contains("\r\n\r\n"),
            "Request should have header/body separator"
        );
        // Body should be present after the separator
        let parts: Vec<&str> = output.splitn(2, "\r\n\r\n").collect();
        assert_eq!(parts.len(), 2, "Request should have headers and body");
        assert!(
            parts[1].contains(body),
            "Body should contain the JSON payload"
        );
    }

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
        let result = DummyRequest::decode(raw);
        assert_eq!(result, Some(raw.len()));
    }

    #[test]
    fn test_decode_request_with_content_length() {
        let raw = b"POST /api/users HTTP/1.1\r\nHost: example.com\r\nContent-Length: 14\r\n\r\n{\"name\":\"foo\"}";
        let result = DummyRequest::decode(raw);
        assert_eq!(result, Some(raw.len()));
    }

    #[test]
    fn test_decode_request_incomplete_body() {
        // Content-Length says 13, but body is only 5 bytes
        let raw =
            b"POST /api/users HTTP/1.1\r\nHost: example.com\r\nContent-Length: 13\r\n\r\nhello";
        let result = DummyRequest::decode(raw);
        assert_eq!(result, None);
    }

    #[test]
    fn test_decode_request_incomplete_headers() {
        let raw = b"POST /api/users HTTP/1.1\r\nHost: example.com\r\n";
        let result = DummyRequest::decode(raw);
        assert_eq!(result, None);
    }

    #[test]
    fn test_decode_request_chunked_encoding() {
        let raw = b"POST /api/data HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let result = DummyRequest::decode(raw);
        assert_eq!(result, Some(raw.len()));
    }

    #[test]
    fn test_decode_request_chunked_multiple_chunks() {
        let raw = b"POST /api/data HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let result = DummyRequest::decode(raw);
        assert_eq!(result, Some(raw.len()));
    }

    #[test]
    fn test_decode_request_chunked_incomplete() {
        // Missing final 0\r\n\r\n
        let raw = b"POST /api/data HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n";
        let result = DummyRequest::decode(raw);
        assert_eq!(result, None);
    }

    #[test]
    fn test_decode_request_extra_data_after() {
        // Buffer has extra data after the request - should return correct length
        let request = b"GET /api/users HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let mut raw = request.to_vec();
        raw.extend_from_slice(b"extra garbage data");
        let result = DummyRequest::decode(&raw);
        assert_eq!(result, Some(request.len()));
    }

    #[test]
    fn test_decode_request_chunked_case_insensitive() {
        let raw = b"POST /api/data HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: CHUNKED\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let result = DummyRequest::decode(raw);
        assert_eq!(result, Some(raw.len()));
    }
}
