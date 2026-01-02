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

/// Encode HTTP messages to their wire format bytes.
///
/// Implemented for `http::Request<B>` and `http::Response<B>`.
/// Only HTTP/1.0 and HTTP/1.1 are supported.
pub trait WireEncode {
    /// Encodes the HTTP message to wire format bytes.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::UnsupportedVersion`] for HTTP/2 or later.
    fn encode(self) -> impl Future<Output = Result<Bytes, WireError>> + Send;
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