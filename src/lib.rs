//! Serialize and parse HTTP/1.x requests and responses to/from wire format bytes.
//!
//! This crate provides encoding and decoding for HTTP/1.0 and HTTP/1.1 messages.
//!
//! # Implementation Details
//!
//! Encoding is performed via direct serialization: the request/response line,
//! headers, and body are written sequentially into a `BytesMut` buffer.
//! No async runtime or HTTP pipeline is required.
//!
//! The body is collected synchronously using [`futures::executor::block_on`],
//! which works without a Tokio runtime for any body type whose future completes
//! without async I/O (e.g. [`http_body_util::Full`], [`http_body_util::Empty`]).
//!
//! # Encoding
//!
//! Use the [`WireEncode`] trait to convert HTTP messages to their wire format:
//!
//! ```rust
//! use http_wire::WireEncode;
//! use http::Request;
//! use http_body_util::Full;
//! use bytes::Bytes;
//!
//! let request = Request::builder()
//!     .method("POST")
//!     .uri("/api/users")
//!     .header("Host", "example.com")
//!     .header("Content-Type", "application/json")
//!     .body(Full::new(Bytes::from(r#"{"name":"John"}"#)))
//!     .unwrap();
//!
//! let bytes = request.encode().unwrap();
//! ```
//!
//! # Decoding
//!
//! Use the [`WireDecode`] trait with [`request::FullRequest`] or
//! [`response::FullResponse`] to parse raw bytes and determine message boundaries.

use bytes::Bytes;
#[doc(no_inline)]
pub use httparse::Header;
use std::mem::MaybeUninit;

mod error;
pub mod request;
pub mod response;
mod util;

pub use error::WireError;

/// Encode HTTP messages to their wire format bytes.
///
/// Implemented for `http::Request<B>` and `http::Response<B>`.
/// Only HTTP/1.0 and HTTP/1.1 are supported.
///
/// Encoding is performed by direct serialization into a pre-allocated buffer.
/// The body is collected synchronously via [`futures::executor::block_on`],
/// which requires no Tokio runtime for in-memory body types such as
/// [`http_body_util::Full`] and [`http_body_util::Empty`].
///
/// # Example
///
/// ```rust
/// use http_wire::WireEncode;
/// use http::Request;
/// use http_body_util::Empty;
/// use bytes::Bytes;
///
/// let request = Request::builder()
///     .method("GET")
///     .uri("/api/users")
///     .header("Host", "example.com")
///     .body(Empty::<Bytes>::new())
///     .unwrap();
///
/// let bytes = request.encode().unwrap();
/// ```
pub trait WireEncode {
    /// Encodes the HTTP message to wire format bytes.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::UnsupportedVersion`] for HTTP/2 or later.
    /// Returns [`WireError::Connection`] if body collection fails.
    fn encode(self) -> Result<Bytes, WireError>
    where
        Self: Sized;
}

/// Decode HTTP messages from raw bytes.
///
/// This trait provides two methods for decoding:
/// - `decode`: Uses initialized headers storage (works for all types)
/// - `decode_uninit`: Uses uninitialized headers storage (optimization, only available for some types)
///
/// # Examples
///
/// ## Decoding a request (standard method)
///
/// ```rust
/// use http_wire::WireDecode;
/// use http_wire::request::FullRequest;
///
/// let raw = b"GET /api/users HTTP/1.1\r\nHost: example.com\r\n\r\n";
/// let mut headers = [httparse::EMPTY_HEADER; 16];
/// let (request, total_len) = FullRequest::decode(raw, &mut headers).unwrap();
///
/// assert_eq!(request.head.method, Some("GET"));
/// assert_eq!(request.head.path, Some("/api/users"));
/// ```
///
/// ## Decoding a request (optimized method with uninitialized headers)
///
/// ```rust
/// use http_wire::WireDecode;
/// use http_wire::request::FullRequest;
/// use std::mem::MaybeUninit;
///
/// let raw = b"GET /api/users HTTP/1.1\r\nHost: example.com\r\n\r\n";
/// let mut headers = [const { MaybeUninit::uninit() }; 16];
/// let (request, total_len) = FullRequest::decode_uninit(raw, &mut headers).unwrap();
///
/// assert_eq!(request.head.method, Some("GET"));
/// ```
///
/// ## Decoding a response
///
/// ```rust
/// use http_wire::WireDecode;
/// use http_wire::response::FullResponse;
///
/// let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
/// let mut headers = [httparse::EMPTY_HEADER; 16];
/// let (response, total_len) = FullResponse::decode(raw, &mut headers).unwrap();
///
/// assert_eq!(response.head.code, Some(200));
/// assert_eq!(response.body, b"hello");
/// ```
///
/// Note: `decode_uninit` is not available for `FullResponse` because the underlying
/// `httparse::Response` parser does not support uninitialized headers.
pub trait WireDecode<'headers, 'buf>: Sized {
    /// Decode using initialized headers storage.
    ///
    /// This method works for both requests and responses.
    ///
    /// # Arguments
    ///
    /// * `buf` - The buffer containing the raw HTTP message bytes
    /// * `headers` - A mutable slice of initialized `Header` structs to store parsed headers
    ///
    /// # Returns
    ///
    /// Returns `Ok((Self, usize))` where `Self` is the decoded message and `usize` is the
    /// total length of the message in bytes (headers + body).
    ///
    /// # Errors
    ///
    /// Returns `WireError` if:
    /// - Headers are incomplete (`WireError::PartialHead`)
    /// - Body is incomplete (`WireError::IncompleteBody`)
    /// - Chunked encoding is malformed (`WireError::InvalidChunkedBody`)
    fn decode(
        buf: &'buf [u8],
        headers: &'headers mut [Header<'buf>],
    ) -> Result<(Self, usize), WireError>;

    /// Decode using uninitialized headers storage (performance optimization).
    ///
    /// This method avoids the overhead of initializing the headers array before parsing.
    /// It is only available for types where the underlying parser supports
    /// `parse_with_uninit_headers`. Currently only `FullRequest` implements this.
    ///
    /// # Arguments
    ///
    /// * `buf` - The buffer containing the raw HTTP message bytes
    /// * `headers` - A mutable slice of uninitialized `Header` structs to store parsed headers
    ///
    /// # Returns
    ///
    /// Returns `Ok((Self, usize))` where `Self` is the decoded message and `usize` is the
    /// total length of the message in bytes (headers + body).
    ///
    /// # Errors
    ///
    /// Returns the same errors as `decode`.
    ///
    /// # Panics
    ///
    /// The default implementation panics with an explanatory message for types that don't
    /// support this optimization (e.g., `FullResponse`).
    fn decode_uninit(
        buf: &'buf [u8],
        headers: &'headers mut [MaybeUninit<Header<'buf>>],
    ) -> Result<(Self, usize), WireError> {
        let _ = (buf, headers);
        unimplemented!(
            "decode_uninit is not available for this type due to missing parse_with_uninit_headers method in the underlying httparse parser"
        )
    }
}