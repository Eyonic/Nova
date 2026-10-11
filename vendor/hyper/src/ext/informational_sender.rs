use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use http::Response;

/// Sends 1xx informational responses, such as `103 Early Hints`, before the
/// final response of an HTTP/2 request.
///
/// hyper's HTTP/2 server inserts an `InformationalSender` into the
/// extensions of every request. A service that is still working on the
/// final response (for example waiting on a slow backend) can send interim
/// responses through it; hyper writes them as HEADERS frames on the stream
/// right away. Once the final response is on its way, `send` returns
/// `false` and nothing more is sent.
///
/// HTTP/1 connections get no `InformationalSender`.
#[derive(Clone, Debug)]
pub struct InformationalSender {
    inner: Arc<Mutex<Shared>>,
}

#[derive(Debug, Default)]
struct Shared {
    queue: Vec<Response<()>>,
    waker: Option<Waker>,
    closed: bool,
}

impl InformationalSender {
    /// Queue `response` (its status must be 1xx). Returns `false` when the
    /// final response has already been sent or the status is not 1xx.
    pub fn send(&self, response: Response<()>) -> bool {
        if !response.status().is_informational() {
            return false;
        }
        let mut shared = self.inner.lock().unwrap();
        if shared.closed {
            return false;
        }
        shared.queue.push(response);
        if let Some(waker) = shared.waker.take() {
            waker.wake();
        }
        true
    }
}

/// The server side of an [`InformationalSender`].
pub(crate) struct InformationalReceiver {
    inner: Arc<Mutex<Shared>>,
}

pub(crate) fn informational_channel() -> (InformationalSender, InformationalReceiver) {
    let inner = Arc::new(Mutex::new(Shared::default()));
    (
        InformationalSender {
            inner: Arc::clone(&inner),
        },
        InformationalReceiver { inner },
    )
}

impl InformationalReceiver {
    /// Take queued responses; registers the task to be woken by the next send.
    pub(crate) fn poll_take(&self, cx: &mut Context<'_>) -> Poll<Vec<Response<()>>> {
        let mut shared = self.inner.lock().unwrap();
        if shared.queue.is_empty() {
            shared.waker = Some(cx.waker().clone());
            Poll::Pending
        } else {
            Poll::Ready(std::mem::take(&mut shared.queue))
        }
    }

    /// No more interim responses: the final one is being sent.
    pub(crate) fn close(&self) {
        let mut shared = self.inner.lock().unwrap();
        shared.closed = true;
        shared.queue.clear();
        shared.waker = None;
    }
}

impl Drop for InformationalReceiver {
    fn drop(&mut self) {
        self.close();
    }
}
