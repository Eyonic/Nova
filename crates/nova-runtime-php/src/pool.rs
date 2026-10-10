//! Keep-alive FastCGI connections to one PHP-FPM pool (`FCGI_KEEP_CONN`).
//!
//! Reusing a connection saves the connect/accept round trip and FPM's
//! accept lock per request. The catch: an FPM worker stays bound to a kept
//! connection even while it is idle, so the pool must never hold as many
//! connections as FPM has workers, or new connections (including NOVA's
//! readiness probe) would wait in the listen backlog forever. Hence:
//!
//! * at most `size` connections exist (idle + busy); `size` is one less
//!   than the pool's `max_children`, and a request waits for a returned
//!   connection instead of opening an extra one;
//! * idle connections are checked before reuse (FPM recycles workers after
//!   `pm.max_requests` and ends idle ones after `process_idle_timeout`) and
//!   dropped after [`MAX_IDLE`];
//! * a connection is returned only after a complete response; errors,
//!   aborted clients and unread request bodies drop it.

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UnixStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Idle connections older than this are closed (FPM's ondemand workers
/// exit after 30 s idle).
const MAX_IDLE: Duration = Duration::from_secs(20);

pub struct Pool {
    socket: PathBuf,
    idle: Mutex<Vec<(UnixStream, Instant)>>,
    permits: Arc<Semaphore>,
}

/// A connection checked out of the pool. Dropping it closes the socket and
/// frees its slot; [`Conn::recycle`] hands a healthy socket back instead.
pub struct Conn {
    pub stream: UnixStream,
    /// Came from the idle list (it may have died since: callers may retry).
    pub reused: bool,
    slot: Slot,
}

pub struct Slot {
    pool: Arc<Pool>,
    _permit: OwnedSemaphorePermit,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("socket", &self.socket)
            .field("idle", &self.idle.lock().map(|i| i.len()).unwrap_or(0))
            .finish()
    }
}

impl Pool {
    /// `max_children` of the FPM pool behind `socket`.
    pub fn new(socket: impl Into<PathBuf>, max_children: u32) -> Arc<Self> {
        let size = (max_children as usize).saturating_sub(1).max(1);
        Arc::new(Self {
            socket: socket.into(),
            idle: Mutex::new(Vec::new()),
            permits: Arc::new(Semaphore::new(size)),
        })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// A live idle connection, or a new one when none is idle. Waits while
    /// all `size` connections are busy.
    pub async fn get(self: &Arc<Self>) -> Result<Conn, crate::PhpError> {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .expect("pool semaphore is never closed");
        let slot = Slot {
            pool: Arc::clone(self),
            _permit: permit,
        };
        while let Some((stream, since)) = self.idle.lock().unwrap().pop() {
            if since.elapsed() < MAX_IDLE && alive(&stream) {
                return Ok(Conn {
                    stream,
                    reused: true,
                    slot,
                });
            }
        }
        let stream = crate::client::connect(&self.socket).await?;
        Ok(Conn {
            stream,
            reused: false,
            slot,
        })
    }

    /// Fresh connection for a retry (never from the idle list).
    pub async fn fresh(self: &Arc<Self>, slot: Slot) -> Result<Conn, crate::PhpError> {
        let stream = crate::client::connect(&self.socket).await?;
        Ok(Conn {
            stream,
            reused: false,
            slot,
        })
    }

    #[cfg(test)]
    pub fn idle_count(&self) -> usize {
        self.idle.lock().unwrap().len()
    }
}

impl Conn {
    pub fn into_parts(self) -> (UnixStream, Slot) {
        (self.stream, self.slot)
    }
}

impl Slot {
    /// Return a healthy connection for the next request.
    pub fn recycle(self, stream: UnixStream) {
        self.pool
            .idle
            .lock()
            .unwrap()
            .push((stream, Instant::now()));
        // `self` (and its permit) is dropped here: the connection now
        // counts as idle, and the next `get` takes the permit again.
    }
}

/// Peek without blocking: a closed connection reads EOF, a live idle one
/// has nothing to read (FPM never sends unsolicited data).
fn alive(stream: &UnixStream) -> bool {
    let mut b = 0u8;
    // SAFETY: valid fd, one-byte buffer, non-blocking peek.
    let n = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&mut b as *mut u8).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn liveness_check_sees_closed_peers() {
        let (a, b) = UnixStream::pair().unwrap();
        assert!(alive(&a), "idle open connection");
        drop(b); // FPM worker exited or closed the connection
        assert!(!alive(&a), "closed by the peer");

        let (a, mut b) = UnixStream::pair().unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut b, b"x")
            .await
            .unwrap();
        assert!(!alive(&a), "unexpected data: not a clean idle connection");
    }
}
