use bytes::Bytes;
use std::future::Future;

mod error;
pub mod request;
pub mod response;
mod wire;

pub use error::WireError;

pub trait ToWire {
    fn to_bytes(self) -> impl Future<Output = Result<Bytes, WireError>> + Send;
}

impl<B> ToWire for http::Request<B>
where
    B: http_body_util::BodyExt + Send + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    async fn to_bytes(self) -> Result<Bytes, WireError> {
        let bytes = request::to_bytes(self).await?;
        Ok(Bytes::from(bytes))
    }
}

impl<B> ToWire for http::Response<B>
where
    B: hyper::body::Body + Clone + Send + Sync + 'static,
    B::Error: std::error::Error + Send + Sync + 'static,
    B::Data: Send + Sync + 'static,
{
    async fn to_bytes(self) -> Result<Bytes, WireError> {
        let bytes = response::to_bytes(self).await?;
        Ok(Bytes::from(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Request, Response};
    use http_body_util::{Empty, Full};

    #[tokio::test]
    async fn test_request_to_wire() {
        let request = Request::builder()
            .method("GET")
            .uri("/api/test")
            .header("Host", "example.com")
            .body(Empty::<Bytes>::new())
            .unwrap();

        let bytes = request.to_bytes().await.unwrap();
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

        let bytes = request.to_bytes().await.unwrap();
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.contains("POST /api/submit HTTP/1.1"));
        assert!(output.contains(body));
    }

    #[tokio::test]
    async fn test_response_to_wire() {
        let response = Response::builder()
            .status(200)
            .header("Content-Type", "text/plain")
            .body(Full::new(Bytes::from("Hello")))
            .unwrap();

        let bytes = response.to_bytes().await.unwrap();
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

        let bytes = response.to_bytes().await.unwrap();
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.contains("HTTP/1.1 404"));
        assert!(output.contains("Not Found"));
    }

    #[tokio::test]
    async fn test_http2_request_rejected() {
        let request = Request::builder()
            .method("GET")
            .uri("/")
            .version(http::Version::HTTP_2)
            .body(Empty::<Bytes>::new())
            .unwrap();

        let result = request.to_bytes().await;
        assert!(matches!(result, Err(WireError::UnsupportedVersion)));
    }

    #[tokio::test]
    async fn test_http2_response_rejected() {
        let response = Response::builder()
            .status(200)
            .version(http::Version::HTTP_2)
            .body(Full::new(Bytes::from("Hello")))
            .unwrap();

        let result = response.to_bytes().await;
        assert!(matches!(result, Err(WireError::UnsupportedVersion)));
    }
}