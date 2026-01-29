//! Serialize and parse HTTP/1.x requests and responses to/from wire format bytes.
//!
//! This crate provides encoding and decoding for HTTP/1.0 and HTTP/1.1 messages.
//!
//! # Implementation Details
//!
//! This crate leverages the [hyper](https://crates.io/crates/hyper) library for reliable
//! and specification-compliant HTTP serialization.
//!
//! Because hyper is asynchronous, the synchronous encoding APIs provided by this crate
//! internally create a temporary, single-threaded Tokio runtime to drive the serialization.
//! If you are already operating within an async context, you should prefer the `_async`
//! variants (e.g., [`WireEncodeAsync`]) to avoid the overhead of creating a nested runtime.
//!
//! # Encoding
//!
//! Use the [`WireEncode`] trait to convert HTTP messages to their wire format (synchronously):
//!

use bytes::Bytes;
pub use httparse::Header;
use std::{future::Future, mem::MaybeUninit};

mod error;
pub mod request;
pub mod response;
mod util;
mod wire;

pub use error::WireError;

/// Encode HTTP messages to their wire format bytes (synchronous version).
///
/// This trait provides synchronous encoding without requiring an async runtime.
/// It creates a minimal single-threaded Tokio runtime internally and blocks on
/// the async encoding method.
///
/// Implemented for `http::Request<B>` and `http::Response<B>`.
/// Only HTTP/1.0 and HTTP/1.1 are supported.
///
/// # Example
///
/// ```rust
/// use http_wire::WireEncode;
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
/// let bytes = request.encode().unwrap();
/// ```
///
/// For async encoding, use [`WireEncodeAsync`] instead.
pub trait WireEncode {
    /// Encodes the HTTP message to wire format bytes synchronously.
    ///
    /// This method creates a minimal single-threaded Tokio runtime and blocks
    /// until the encoding is complete.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::UnsupportedVersion`] for HTTP/2 or later.
    fn encode(self) -> Result<Bytes, WireError>
    where
        Self: Sized;
}

/// Encode HTTP messages to their wire format bytes (async version).
///
/// Implemented for `http::Request<B>` and `http::Response<B>`.
/// Only HTTP/1.0 and HTTP/1.1 are supported.
///
/// For synchronous encoding without requiring an async runtime,
/// use [`WireEncode`] instead.
///
/// # Example
///
/// ```rust,no_run
/// use http_wire::WireEncodeAsync;
/// use http::Request;
/// use http_body_util::Empty;
/// use bytes::Bytes;
///
/// # async fn example() {
/// let request = Request::builder()
///     .uri("/api/users")
///     .header("Host", "example.com")
///     .body(Empty::<Bytes>::new())
///     .unwrap();
///
/// let bytes = request.encode_async().await.unwrap();
/// # }
/// ```
pub trait WireEncodeAsync {
    /// Encodes the HTTP message to wire format bytes.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::UnsupportedVersion`] for HTTP/2 or later.
    fn encode_async(self) -> impl Future<Output = Result<Bytes, WireError>> + Send;
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
