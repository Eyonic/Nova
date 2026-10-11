//! Log output in batches: lines are appended to a shared buffer and one
//! thread writes whatever accumulated every [`BATCH`], in one `write`.
//!
//! The previous writer (tracing-appender) handed each line to its thread
//! through a channel and wrote it with its own syscall: with an access
//! line per request that was a thread wake-up and a `write` per request,
//! ~7% of NOVA's CPU under load. Lines are never dropped: when the reader
//! of stdout falls [`MAX_PENDING`] behind, writers wait for it.

use std::io::{self, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// How long lines may wait to be written together.
const BATCH: Duration = Duration::from_millis(10);
/// Pending bytes at which writers wait for the output to catch up.
const MAX_PENDING: usize = 8 << 20;

#[derive(Default)]
struct State {
    buf: Vec<u8>,
    closed: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    /// Signalled when the buffer stops being empty, or on close.
    filled: Condvar,
    /// Signalled after each batch is written.
    drained: Condvar,
}

/// The writer handed to tracing; cheap to clone.
#[derive(Clone)]
pub struct BatchWriter(Arc<Shared>);

/// Writes what is left when dropped (end of `main`).
pub struct Guard {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub fn stdout() -> (BatchWriter, Guard) {
    let shared = Arc::new(Shared::default());
    let s = Arc::clone(&shared);
    let thread = std::thread::Builder::new()
        .name("nova-log".into())
        .spawn(move || run(&s, &mut io::stdout()))
        .expect("spawning the log thread");
    (
        BatchWriter(Arc::clone(&shared)),
        Guard {
            shared,
            thread: Some(thread),
        },
    )
}

fn run(s: &Shared, out: &mut impl Write) {
    let mut batch = Vec::new();
    loop {
        {
            let mut st = s.state.lock().unwrap();
            while st.buf.is_empty() && !st.closed {
                st = s.filled.wait(st).unwrap();
            }
            if st.buf.is_empty() && st.closed {
                return;
            }
        }
        // Let lines accumulate (unless shutting down).
        let closed = s.state.lock().unwrap().closed;
        if !closed {
            std::thread::sleep(BATCH);
        }
        {
            let mut st = s.state.lock().unwrap();
            std::mem::swap(&mut batch, &mut st.buf);
        }
        s.drained.notify_all();
        let _ = out.write_all(&batch);
        let _ = out.flush();
        batch.clear();
        if batch.capacity() > MAX_PENDING {
            batch = Vec::new();
        }
    }
}

impl BatchWriter {
    pub fn write_line(&self, line: &[u8]) {
        let s = &self.0;
        let mut st = s.state.lock().unwrap();
        while st.buf.len() > MAX_PENDING && !st.closed {
            st = s.drained.wait(st).unwrap();
        }
        let was_empty = st.buf.is_empty();
        st.buf.extend_from_slice(line);
        drop(st);
        if was_empty {
            s.filled.notify_one();
        }
    }
}

impl Write for BatchWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_line(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BatchWriter {
    type Writer = BatchWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().closed = true;
        self.shared.filled.notify_one();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_arrive_in_order_and_are_flushed_on_drop() {
        let shared = Arc::new(Shared::default());
        let w = BatchWriter(Arc::clone(&shared));
        let out = Arc::new(Mutex::new(Vec::new()));
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (s, o) = (Arc::clone(&shared), Arc::clone(&out));
        let guard = Guard {
            shared,
            thread: Some(std::thread::spawn(move || run(&s, &mut Sink(o)))),
        };
        for i in 0..1000 {
            w.write_line(format!("{i}\n").as_bytes());
        }
        drop(guard);
        let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 1000);
        assert_eq!(lines[999], "999");
    }
}
