use std::convert::Infallible;

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Empty;
use hyper::{body::Body, service::service_fn};
use hyper_util::rt::TokioIo;
use tokio::io::duplex;
use tokio::sync::oneshot;

use crate::wire::WireCapture;

pub async fn to_bytes<B: Body + Clone>(response: Response<B>) -> Vec<u8>
where
    B: Body + Clone + Send + Sync + 'static,
    <B as Body>::Error: std::error::Error + Send + Sync + 'static,
    <B as Body>::Data: Send + Sync + 'static,
{
    let (client, server) = duplex(8192);
    let capture_server = WireCapture::new(server);
    let captured_ref = capture_server.captured.clone();

    let (tx, rx) = oneshot::channel::<()>();

    let handle = tokio::spawn(async move {
        let service = service_fn(move |_req: Request<hyper::body::Incoming>| {
            let res = response.clone();
            async move { Ok::<_, Infallible>(res) }
        });

        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(capture_server), service)
            .await;
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

        if let Ok((mut sender, connection)) = client_connection {
            tokio::spawn(connection);
            // When send_request completes, the response has been received
            let _ = sender.send_request(req).await;
            let _ = tx.send(()); // Signal completion
        }
    });

    // Wait for completion instead of sleep
    let _ = rx.await;
    let _ = handle.await;
    captured_ref.lock().clone()
}