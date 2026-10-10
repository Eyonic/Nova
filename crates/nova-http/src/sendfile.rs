//! Plain-TCP connection IO with zero-copy file bodies (experiment).
//!
//! Wraps hyper-util's `TokioIo<TcpStream>` and adds hyper's
//! `poll_write_file` (vendored hyper with PR #4214): responses carrying a
//! `hyper::ext::SendFile` are written with Linux `sendfile(2)`, straight
//! from the page cache to the socket. TLS streams keep the normal path.

use hyper::rt::{Read, ReadBufCursor, Write};
use hyper_util::rt::TokioIo;
use std::io;
use std::os::fd::AsRawFd;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use tokio::io::Interest;
use tokio::net::TcpStream;

pub struct PlainTcp(pub TokioIo<TcpStream>);

impl Read for PlainTcp {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl Write for PlainTcp {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write_vectored(cx, bufs)
    }

    fn supports_write_file(&self) -> bool {
        true
    }

    fn poll_write_file(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        file: &std::fs::File,
        offset: u64,
        len: usize,
    ) -> Poll<io::Result<usize>> {
        let stream = self.get_mut().0.inner();
        loop {
            ready!(stream.poll_write_ready(cx))?;
            let sent = stream.try_io(Interest::WRITABLE, || {
                let mut off = offset as libc::off_t;
                // SAFETY: both descriptors are open for the duration of the call;
                // `off` is a local copy, so the file's cursor never moves.
                let n =
                    unsafe { libc::sendfile(stream.as_raw_fd(), file.as_raw_fd(), &mut off, len) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match sent {
                Ok(n) => return Poll::Ready(Ok(n)),
                // Readiness was stale: wait for the next writable event.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }
}
