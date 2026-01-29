//! Error handling for HTTP wire operations.
//!
//! This module defines the [`WireError`] type, which encompasses all possible errors
//! that can occur during encoding or decoding.

/// Errors that can occur during HTTP wire format encoding.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// HTTP connection error during the encoding process.
    ///
    /// This occurs when there's a failure during the HTTP handshake
    /// or while transmitting the message through the internal pipeline.
    #[error("http connection error: {0}")]
    Connection(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Internal synchronization error.
    ///
    /// This occurs when an internal communication channel closes unexpectedly.
    /// If you encounter this error, please report it as a bug.
    #[error("synchronization error: channel closed unexpectedly")]
    Sync,

    /// Unsupported HTTP version.
    ///
    /// Only HTTP/1.0 and HTTP/1.1 are supported. HTTP/2 and HTTP/3 use
    /// binary framing and compression which make wire format serialization
    /// impractical for single messages.
    #[error("unsupported HTTP version: only HTTP/1.0 and HTTP/1.1 are supported")]
    UnsupportedVersion,

    /// Error from the httparse library during header parsing.
    ///
    /// This wraps errors from the `httparse` crate when parsing HTTP headers fails.
    /// Common causes include malformed header lines, invalid characters, or
    /// header values that violate HTTP specifications.
    #[error("{0}")]
    HttparseError(#[from] httparse::Error),

    /// HTTP headers are incomplete.
    ///
    /// This error indicates that the received data does not contain a complete
    /// HTTP header section (missing the `\r\n\r\n` terminator).
    /// More data needs to be received before the message can be parsed.
    #[error("partial head")]
    PartialHead,

    /// HTTP body is incomplete.
    ///
    /// This error occurs when the `Content-Length` header indicates a body
    /// size larger than what was actually received. The argument specifies
    /// how many bytes are still missing.
    ///
    /// For chunked encoding, this error is not used; instead,
    /// [`WireError::InvalidChunkedBody`] is returned.
    #[error("partial body: {0} bytes missing")]
    IncompleteBody(usize),

    /// Invalid chunked transfer encoding.
    ///
    /// This error occurs when parsing a chunked body fails due to malformed
    /// chunk formatting, invalid chunk sizes, missing chunk terminators,
    /// or incomplete chunked data.
    #[error("invalid chunked body")]
    InvalidChunkedBody,
}
