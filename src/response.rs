use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Empty;
use hyper::{body::Body, service::service_fn};
use hyper_util::rt::TokioIo;
use tokio::io::duplex;
use tokio::sync::oneshot;

use crate::error::WireError;
use crate::wire::WireCapture;

/// Serialize an HTTP response to raw bytes using hyper's HTTP/1.1 serialization.
/// This uses a duplex stream to capture the exact bytes that would be sent over the wire.
pub async fn to_bytes<B>(response: Response<B>) -> Result<Vec<u8>, WireError>
where
    B: Body + Clone + Send + Sync + 'static,
    <B as Body>::Error: std::error::Error + Send + Sync + 'static,
    <B as Body>::Data: Send + Sync + 'static,
{
    use std::convert::Infallible;

    let (client, server) = duplex(8192);
    let capture_server = WireCapture::new(server);
    let captured_ref = capture_server.captured.clone();

    let (tx, rx) = oneshot::channel::<Result<(), WireError>>();

    let handle = tokio::spawn(async move {
        let service = service_fn(move |_req: Request<hyper::body::Incoming>| {
            let res = response.clone();
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

    Ok(captured_ref.lock().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;

    #[tokio::test]
    async fn test_http1_capture() {
        let response = Response::builder()
            .status(200)
            .header("Content-Type", "text/plain")
            .body(Full::new(Bytes::from("Hello World")))
            .unwrap();

        let bytes = to_bytes(response).await.unwrap();
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

        let bytes = to_bytes(response).await.unwrap();
        let output = String::from_utf8_lossy(&bytes);

        // Verify the response has proper HTTP structure
        assert!(output.contains("HTTP/1.1 200 OK"));
        // Headers and body are separated by \r\n\r\n
        assert!(output.contains("\r\n\r\n"), "Response should have header/body separator");
        // Body should be present after the separator
        let parts: Vec<&str> = output.splitn(2, "\r\n\r\n").collect();
        assert_eq!(parts.len(), 2, "Response should have headers and body");
        assert!(parts[1].contains(body), "Body should contain the payload");
    }
}
