use std::convert::Infallible;

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Empty;
use hyper_util::rt::TokioIo;
use tokio::io::duplex;
use tokio::sync::oneshot;

use crate::wire::WireCapture;

// Serialize an HTTP request to raw bytes using hyper's HTTP/1.1 serialization.
/// This uses a duplex stream to capture the exact bytes that would be sent over the wire.
pub async fn to_bytes<B>(request: Request<B>) -> Vec<u8>
where
    B: http_body_util::BodyExt + Send + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    use hyper::service::service_fn;

    let (client, server) = duplex(8192);
    let capture_client = WireCapture::new(client);
    let captured_ref = capture_client.captured.clone();

    let (tx, rx) = oneshot::channel::<()>();

    // Spawn a mock server that will accept the connection and read the request
    let server_handle = tokio::spawn(async move {
        let tx = std::sync::Mutex::new(Some(tx));
        let service = service_fn(move |_req: Request<hyper::body::Incoming>| {
            // Signal that the request has been received
            if let Some(tx) = tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
            async move {
                // Return a minimal response
                Ok::<_, Infallible>(Response::new(Empty::<Bytes>::new()))
            }
        });

        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(server), service)
            .await;
    });

    // Send the request through the client side and capture what's written
    let client_handle = tokio::spawn(async move {
        let client_connection = hyper::client::conn::http1::Builder::new()
            .handshake(TokioIo::new(capture_client))
            .await;

        if let Ok((mut sender, connection)) = client_connection {
            // Spawn the connection driver
            tokio::spawn(connection);

            // Send the request
            let _ = sender.send_request(request).await;
        }
    });

    // Wait for the server to receive the request (instead of sleeping)
    let _ = rx.await;

    // Cleanup
    client_handle.abort();
    server_handle.abort();

    captured_ref.lock().clone()
}