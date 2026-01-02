//! Error types for HTTP wire format operations.

/// Errors that can occur during HTTP wire format encoding.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// HTTP connection error during the encoding process.
    ///
    /// This occurs when there's a failure during the HTTP handshake
    /// or while transmitting the message through the internal pipeline.
    #[error("HTTP connection error: {0}")]
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
}