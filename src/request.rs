use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Empty;
use hyper_util::rt::TokioIo;
use tokio::io::duplex;
use tokio::sync::oneshot;

use crate::error::WireError;
use crate::wire::WireCapture;

/// Serialize an HTTP request to raw bytes using hyper's HTTP/1.1 serialization.
/// This uses a duplex stream to capture the exact bytes that would be sent over the wire.
pub async fn to_bytes<B>(request: Request<B>) -> Result<Vec<u8>, WireError>
where
    B: http_body_util::BodyExt + Send + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    use hyper::service::service_fn;
    use std::convert::Infallible;

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

    Ok(captured_ref.lock().clone())
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
        assert!(output.contains("\r\n\r\n"), "Request should have header/body separator");
        // Body should be present after the separator
        let parts: Vec<&str> = output.splitn(2, "\r\n\r\n").collect();
        assert_eq!(parts.len(), 2, "Request should have headers and body");
        assert!(parts[1].contains(body), "Body should contain the JSON payload");
    }
}