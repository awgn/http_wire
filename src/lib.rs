//! Serialize and parse HTTP/1.x requests and responses to/from wire format bytes.
//!
//! This crate provides encoding and decoding for HTTP/1.0 and HTTP/1.1 messages.
//! HTTP/2 and HTTP/3 are not supported.
//!
//! # Encoding
//!
//! Use the [`WireEncode`] trait to convert HTTP messages to their wire format:
//!
//! ```rust,no_run
//! use http_wire::WireEncode;
//! use http::Request;
//! use http_body_util::Empty;
//! use bytes::Bytes;
//!
//! # async fn example() {
//! let request = Request::builder()
//!     .uri("/api/users")
//!     .header("Host", "example.com")
//!     .body(Empty::<Bytes>::new())
//!     .unwrap();
//!
//! let bytes = request.encode().await.unwrap();
//! # }
//! ```
//!
//! # Decoding
//!
//! Use the [`WireDecode`] trait to parse raw bytes and determine message boundaries:
//!
//! ```rust
//! use http_wire::WireDecode;
//! use http_wire::request::RequestLength;
//!
//! let raw = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
//!
//! if let Some(length) = RequestLength::decode(raw) {
//!     println!("Complete request: {} bytes", length);
//! }
//! ```

use bytes::Bytes;
use std::future::Future;

mod error;
pub mod request;
pub mod response;
mod util;
mod wire;

pub use error::WireError;

/// Encode HTTP messages to their wire format bytes (async version).
///
/// Implemented for `http::Request<B>` and `http::Response<B>`.
/// Only HTTP/1.0 and HTTP/1.1 are supported.
///
/// For synchronous encoding without requiring an async runtime,
/// use [`WireEncodeSync`] instead.
pub trait WireEncode {
    /// Encodes the HTTP message to wire format bytes.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::UnsupportedVersion`] for HTTP/2 or later.
    fn encode(self) -> impl Future<Output = Result<Bytes, WireError>> + Send;
}

/// Encode HTTP messages to their wire format bytes (sync version).
///
/// This trait provides synchronous encoding without requiring an async runtime.
/// It creates a minimal single-threaded Tokio runtime internally and blocks on
/// the async [`WireEncode::encode`] method.
///
/// Implemented for `http::Request<B>` and `http::Response<B>`.
/// Only HTTP/1.0 and HTTP/1.1 are supported.
///
/// # Example
///
/// ```rust
/// use http_wire::WireEncodeSync;
/// use http::Request;
/// use http_body_util::Full;
/// use bytes::Bytes;
///
/// let request = Request::builder()
///     .method("GET")
///     .uri("/api/users")
///     .header("Host", "example.com")
///     .body(Full::new(Bytes::from("hello")))
///     .unwrap();
///
/// let bytes = request.encode_sync().unwrap();
/// ```
pub trait WireEncodeSync {
    /// Encodes the HTTP message to wire format bytes synchronously.
    ///
    /// This method creates a minimal single-threaded Tokio runtime and blocks
    /// until the encoding is complete.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::UnsupportedVersion`] for HTTP/2 or later.
    fn encode_sync(self) -> Result<Bytes, WireError>
    where
        Self: Sized;
}

/// Parse raw HTTP bytes to determine message boundaries.
///
/// Implementations:
/// - [`request::RequestLength`] - returns total message length
/// - [`response::ResponseStatusCode`] - returns status code and length
pub trait WireDecode: Sized {
    /// The type returned by successful decoding.
    type Output;

    /// Attempts to decode the byte slice.
    ///
    /// Returns `Some(Output)` if complete and valid, `None` otherwise.
    fn decode(bytes: &[u8]) -> Option<Self::Output>;
}

// Implementation of WireEncodeSync for Request
impl<B> WireEncodeSync for http::Request<B>
where
    B: http_body_util::BodyExt + Send + Sync + 'static,
    B::Data: Send + Sync + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    fn encode_sync(self) -> Result<Bytes, WireError> {
        // Create a minimal single-threaded runtime
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| WireError::Connection(Box::new(e)))?;

        // Block on the async encode method
        rt.block_on(self.encode())
    }
}

// Implementation of WireEncodeSync for Response
impl<B> WireEncodeSync for http::Response<B>
where
    B: hyper::body::Body + Clone + Send + Sync + 'static,
    B::Data: Send + Sync + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    fn encode_sync(self) -> Result<Bytes, WireError> {
        // Create a minimal single-threaded runtime
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| WireError::Connection(Box::new(e)))?;

        // Block on the async encode method
        rt.block_on(self.encode())
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

        let bytes = request.encode_sync().unwrap();
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

        let bytes = request.encode_sync().unwrap();
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

        let result = request.encode_sync();
        assert!(matches!(result, Err(WireError::UnsupportedVersion)));
    }

    #[test]
    fn test_response_sync_ok() {
        let response = http::Response::builder()
            .status(200)
            .header("Content-Type", "text/plain")
            .body(Full::new(Bytes::from("Hello")))
            .unwrap();

        let bytes = response.encode_sync().unwrap();
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

        let bytes = response.encode_sync().unwrap();
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

        let result = response.encode_sync();
        assert!(matches!(result, Err(WireError::UnsupportedVersion)));
    }
}