use std::{
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite};

pub trait Wire {
    fn to_bytes(&self) -> Bytes;
}

/// Socket wrapper that captures written bytes while simulating a real connection
pub(crate) struct WireCapture {
    pub(crate) inner: tokio::io::DuplexStream,
    pub(crate) captured: Arc<Mutex<Vec<u8>>>,
}

impl WireCapture {
    pub(crate) fn new(inner: tokio::io::DuplexStream) -> Self {
        Self {
            inner,
            captured: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl AsyncRead for WireCapture {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for WireCapture {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        // Capture the bytes being written
        self.captured.lock().extend_from_slice(buf);
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
