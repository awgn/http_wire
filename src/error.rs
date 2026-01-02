/// Errors that can occur during HTTP wire serialization.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// HTTP connection error (handshake or send failed)
    #[error("HTTP connection error: {0}")]
    Connection(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Synchronization error (internal channel closed unexpectedly)
    #[error("synchronization error: channel closed unexpectedly")]
    Sync,

    /// Unsupported HTTP version (only HTTP/1.1 is supported)
    #[error("unsupported HTTP version: only HTTP/1.1 is supported")]
    UnsupportedVersion,
}
