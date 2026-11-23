//! How much compression saves on one SSH connection's incoming traffic
//! (doc 04 §Compression savings).
//!
//! [`CountingStream`] counts the bytes read off the wire; the russh handler
//! adds what each channel data message would have cost uncompressed. The
//! saving is the difference, measured from authentication on.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// The framing an uncompressed channel data message costs beside its bytes:
/// type, channel and length fields (9), packet length and padding length
/// (5), typical padding (8) and a 16-byte AEAD tag.
const DATA_OVERHEAD: u64 = 9 + 5 + 8 + 16;
/// Extended data (stderr) also carries its type code.
const EXTENDED_OVERHEAD: u64 = DATA_OVERHEAD + 4;
/// Below this much estimated traffic the figure is noise.
pub const MIN_MEASURED: u64 = 32 * 1024;

#[derive(Debug, Default)]
pub struct Traffic {
    compressing: bool,
    wire_in: AtomicU64,
    wire_at_auth: AtomicU64,
    plain_in: AtomicU64,
}

impl Traffic {
    /// Counters for a connection that offered compression or not.
    pub fn new(compressing: bool) -> Arc<Self> {
        Arc::new(Self {
            compressing,
            ..Self::default()
        })
    }

    /// Whether the connection offered compression; the server may still
    /// have declined it.
    pub fn compressing(&self) -> bool {
        self.compressing
    }

    pub(crate) fn add_wire(&self, bytes: usize) {
        self.wire_in.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Key exchange and authentication are not channel traffic: count the
    /// wire from here on.
    pub(crate) fn mark_authenticated(&self) {
        self.wire_at_auth
            .store(self.wire_in.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    pub(crate) fn add_data(&self, bytes: usize, extended: bool) {
        let overhead = if extended {
            EXTENDED_OVERHEAD
        } else {
            DATA_OVERHEAD
        };
        self.plain_in
            .fetch_add(bytes as u64 + overhead, Ordering::Relaxed);
    }

    /// The share of incoming traffic compression saved, in percent: `None`
    /// without compression or before [`MIN_MEASURED`]. Never below 0, so a
    /// server that declined compression reads about 0%.
    pub fn savings_percent(&self) -> Option<u8> {
        let plain = self.plain_in.load(Ordering::Relaxed);
        if !self.compressing || plain < MIN_MEASURED {
            return None;
        }
        let wire = self
            .wire_in
            .load(Ordering::Relaxed)
            .saturating_sub(self.wire_at_auth.load(Ordering::Relaxed));
        let saved = plain.saturating_sub(wire) * 100 / plain;
        Some(saved as u8)
    }
}

/// A stream that counts what is read from it into [`Traffic`].
pub(crate) struct CountingStream<S> {
    inner: S,
    traffic: Arc<Traffic>,
}

impl<S> CountingStream<S> {
    pub(crate) fn new(inner: S, traffic: Arc<Traffic>) -> Self {
        Self { inner, traffic }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for CountingStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = poll {
            self.traffic.add_wire(buf.filled().len() - before);
        }
        poll
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for CountingStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests;
