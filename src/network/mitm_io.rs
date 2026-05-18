//! Replay a pre-buffered byte slice before delegating to a live stream.
//!
//! `handle_tcp` peeks the first bytes of the client connection to identify
//! TLS (and capture SNI). When MITM takes over, the rustls server needs to
//! see those bytes as the start of the ClientHello -- they have already
//! been read off the socket and are sitting in a `Vec<u8>`. This wrapper
//! yields the buffered bytes first, then delegates further reads to the
//! underlying stream.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

pub struct ReplayingStream {
    inner: TcpStream,
    buffered: Vec<u8>,
    cursor: usize,
}

impl ReplayingStream {
    pub fn new(inner: TcpStream, buffered: Vec<u8>) -> Self {
        Self {
            inner,
            buffered,
            cursor: 0,
        }
    }
}

impl AsyncRead for ReplayingStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.cursor < self.buffered.len() {
            let remaining = &self.buffered[self.cursor..];
            let take = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..take]);
            self.cursor += take;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for ReplayingStream {
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
}
