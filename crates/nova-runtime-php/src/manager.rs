//! Worker mode, site side: `nova php-workers` runs as the site, inside its
//! sandbox, keeps `processes` PHP workers alive and feeds them requests.
//!
//! NOVA sends every request to the manager as soon as it arrives, tagged
//! with an id, over a few multiplexed connections. The manager queues them
//! and hands the next one to a PHP worker the moment that worker is free,
//! without a round trip through NOVA (when NOVA picked the worker, workers
//! sat idle ~13% of the time under load, waiting for the hand-off). Only
//! idle workers get a request, so a slow request never holds up another.
//!
//! Each worker talks goridge over its stdin/stdout (`RR_RELAY=pipes`, or
//! NOVA's PHP extension). The next request is written to a worker as soon
//! as it sends its response, while PHP still resets for the next request,
//! so it never sleeps between requests under load. Workers on the extension
//! (`ready`) announce each time they wait for a request: if one leaves its
//! loop instead (its own request limit, `exit`), the request sent ahead was
//! never read and goes to another worker. Being the workers' parent, the
//! manager enforces what a server must:
//!
//! * `{"cancel":true}` from NOVA (its timeout) drops a queued request, and
//!   kills and replaces the worker running it;
//! * a worker that exits is replaced (with backoff when it crashes before
//!   serving anything); a request it was running fails;
//! * after `max_requests` a worker is stopped gracefully and replaced;
//! * with `watch` (development), changed PHP code restarts the workers
//!   gracefully, as a booted application would otherwise keep old code;
//! * form bodies are parsed here (RoadRunner workers), so uploads land in
//!   the site's own tmp; each worker gets a private `TMPDIR`, and after a
//!   request with uploads the copies frameworks make of them (Symfony's
//!   PSR-7 bridge leaves `symfony*` files behind) are deleted;
//! * on SIGTERM queued and running requests finish, then workers stop.
//!
//! Frames between NOVA and the manager carry the request id as their
//! second option (`[header length, id]`); the manager strips it towards
//! PHP and adds it to PHP's answers.

use crate::goridge::{self, Frame};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio::time::Instant;

const MAX_FRAME: usize = 1 << 30;

/// Connections to NOVA (requests are multiplexed over them).
const LINKS: usize = 2;

/// Pipe buffer per direction (the unprivileged maximum by default).
const PIPE_SIZE: libc::c_int = 1 << 20;

/// Requests up to this size may be sent to a worker still finishing its
/// previous one (they fit in the pipe, so the write does not wait).
const PRESEND_MAX: usize = 512 << 10;

#[derive(Debug, Clone)]
pub struct ManagerOptions {
    pub socket: PathBuf,
    pub processes: u32,
    /// The worker command, e.g. `php -d ... vendor/laravel/octane/bin/roadrunner-worker`.
    pub command: Vec<OsString>,
    pub tmp_dir: PathBuf,
    /// How long a stopping worker may take to finish.
    pub grace: Duration,
    /// Development: restart workers when PHP code below this directory
    /// changes.
    pub watch: Option<PathBuf>,
    /// Replace a worker after this many requests (0: never).
    pub max_requests: u64,
    /// Workers announce `{"ready":true}` before each request (NOVA's PHP
    /// extension); otherwise a worker is ready once started and after
    /// each response (RoadRunner workers).
    pub ready: bool,
}

/// Run until SIGTERM/SIGINT, then let queued and running requests finish.
pub async fn run(opts: ManagerOptions) -> io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let (tx, rx) = mpsc::unbounded_channel();
    if let Some(dir) = opts.watch.clone() {
        tokio::spawn(watch_code(dir, tx.clone()));
    }
    for link in 0..LINKS {
        tokio::spawn(connect_loop(opts.socket.clone(), link, tx.clone()));
    }
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let stop = tx.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        let _ = stop.send(Ev::Stop);
    });
    Dispatcher::new(opts, tx).run(rx).await;
    Ok(())
}

enum Ev {
    Connected(usize, OwnedWriteHalf),
    FromNova(usize, Frame),
    NovaClosed(usize),
    FromWorker(usize, u64, Frame),
    WorkerExited(usize, u64),
    Respawn(usize),
    KillIfStill(usize, u64),
    CodeChanged,
    Stop,
}

/// Keep one link to NOVA up: connect (retrying while NOVA starts), read
/// frames, reconnect when NOVA restarts.
async fn connect_loop(socket: PathBuf, link: usize, tx: mpsc::UnboundedSender<Ev>) {
    loop {
        let Ok(stream) = UnixStream::connect(&socket).await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let (rd, wr) = stream.into_split();
        if tx.send(Ev::Connected(link, wr)).is_err() {
            return;
        }
        let mut r = BufReader::with_capacity(64 << 10, rd);
        while let Ok(f) = goridge::read_frame(&mut r, MAX_FRAME).await {
            if tx.send(Ev::FromNova(link, f)).is_err() {
                return;
            }
        }
        if tx.send(Ev::NovaClosed(link)).is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll the project's PHP code once a second; report when it changes
/// (cheap enough for development projects).
async fn watch_code(dir: PathBuf, tx: mpsc::UnboundedSender<Ev>) {
    let scan = move || code_fingerprint(&dir);
    let mut last = tokio::task::spawn_blocking(scan.clone()).await.ok();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let now = tokio::task::spawn_blocking(scan.clone()).await.ok();
        if now != last {
            last = now;
            eprintln!("nova php-workers: code changed; restarting workers");
            if tx.send(Ev::CodeChanged).is_err() {
                return;
            }
        }
    }
}

/// (files, newest mtime, mtime sum) of `.php` files, `.env` and
/// `composer.lock`, skipping dependency and state directories.
fn code_fingerprint(root: &Path) -> (u64, u128, u128) {
    use std::os::unix::fs::MetadataExt;
    const SKIP: &[&str] = &["vendor", "node_modules", "storage", ".git", "public"];
    let mut fp = (0u64, 0u128, 0u128);
    let mut stack = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                if depth < 12 && !(depth == 0 && SKIP.contains(&name.as_ref())) {
                    stack.push((e.path(), depth + 1));
                }
            } else if (name.ends_with(".php") || name == ".env" || name == "composer.lock")
                && let Ok(m) = e.metadata()
            {
                let t = m.mtime() as u128 * 1_000_000_000 + m.mtime_nsec() as u128;
                fp.0 += 1;
                fp.1 = fp.1.max(t);
                fp.2 = fp.2.wrapping_add(t);
            }
        }
    }
    fp
}

/// Remove regular files in a worker's private temp directory.
fn clean_tmp(dir: &Path, select: impl Fn(&str) -> bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        if e.file_type().is_ok_and(|t| t.is_file()) && select(&name.to_string_lossy()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// A request waiting for a worker: the link it came on, its id, the frame.
struct Pending {
    link: usize,
    id: u32,
    frame: Frame,
}

enum State {
    /// Running, not yet asking for a request (booting, or between
    /// requests until it announces it is ready).
    Starting,
    Idle {
        /// The worker announced that it waits (always, for RoadRunner
        /// workers). Right after a response it has not yet: a request sent
        /// then waits in its pipe and is read without a hand-off delay.
        confirmed: bool,
    },
    Busy {
        link: usize,
        id: u32,
        /// NOVA gave up or went away: let it finish, drop the answer.
        orphan: bool,
        uploads: Option<crate::form::Multipart>,
        /// Sent before the worker announced it waits: kept until it does,
        /// so that if it exits instead (it never read the request), the
        /// request goes to another worker.
        unread: Option<Frame>,
    },
    /// Told to stop (recycling, code change, shutdown) or killed.
    Stopping,
    Down,
}

struct Slot {
    state: State,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    /// Incremented per process: events of a replaced process are ignored.
    generation: u64,
    started: Instant,
    served: u64,
    tmp: PathBuf,
    backoff: Duration,
    /// Replace it once the current request is done (code changed).
    restart: bool,
}

struct Dispatcher {
    opts: ManagerOptions,
    tx: mpsc::UnboundedSender<Ev>,
    links: Vec<Option<OwnedWriteHalf>>,
    queue: VecDeque<Pending>,
    slots: Vec<Slot>,
    stopping: bool,
}

impl Dispatcher {
    fn new(opts: ManagerOptions, tx: mpsc::UnboundedSender<Ev>) -> Self {
        let slots = (0..opts.processes.max(1))
            .map(|n| {
                // One request at a time per worker: what its uploads leave in
                // this directory belongs to the request that just finished.
                let tmp = opts.tmp_dir.join(format!("w{n}"));
                use std::os::unix::fs::DirBuilderExt;
                let _ = std::fs::DirBuilder::new().mode(0o700).create(&tmp);
                Slot {
                    state: State::Down,
                    child: None,
                    stdin: None,
                    generation: 0,
                    started: Instant::now(),
                    served: 0,
                    tmp,
                    backoff: Duration::from_millis(250),
                    restart: false,
                }
            })
            .collect();
        Dispatcher {
            opts,
            tx,
            links: (0..LINKS).map(|_| None).collect(),
            queue: VecDeque::new(),
            slots,
            stopping: false,
        }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Ev>) {
        for n in 0..self.slots.len() {
            self.spawn(n);
        }
        while let Some(ev) = rx.recv().await {
            match ev {
                Ev::Connected(link, wr) => self.links[link] = Some(wr),
                Ev::FromNova(link, f) => self.on_request_frame(link, f).await,
                Ev::NovaClosed(link) => self.nova_closed(link),
                Ev::FromWorker(n, generation, f) => {
                    if self.slots[n].generation == generation {
                        self.on_worker_frame(n, f).await;
                    }
                }
                Ev::WorkerExited(n, generation) => {
                    if self.slots[n].generation == generation {
                        self.exited(n).await;
                    }
                }
                Ev::Respawn(n) => {
                    if matches!(self.slots[n].state, State::Down) && !self.stopping {
                        self.spawn(n);
                    }
                }
                Ev::KillIfStill(n, generation) => {
                    let s = &mut self.slots[n];
                    if s.generation == generation
                        && let Some(c) = s.child.as_mut()
                    {
                        let _ = c.start_kill();
                    }
                }
                Ev::CodeChanged => {
                    for n in 0..self.slots.len() {
                        self.slots[n].restart = true;
                        if matches!(self.slots[n].state, State::Idle { .. } | State::Starting) {
                            self.stop_slot(n).await;
                        }
                    }
                }
                Ev::Stop => self.stopping = true,
            }
            self.dispatch().await;
            if self.stopping {
                // Queued requests are served first.
                for n in 0..self.slots.len() {
                    if matches!(self.slots[n].state, State::Idle { .. })
                        || (self.queue.is_empty() && matches!(self.slots[n].state, State::Starting))
                    {
                        self.stop_slot(n).await;
                    }
                }
                if self.slots.iter().all(|s| matches!(s.state, State::Down)) {
                    return;
                }
            }
        }
    }

    /// Hand queued requests to idle workers, most recently used first
    /// (warm caches).
    async fn dispatch(&mut self) {
        while !self.queue.is_empty() {
            let front = &self.queue.front().unwrap().frame;
            let presend =
                front.payload.len() <= PRESEND_MAX && front.byte11 & goridge::NOVA_PARSE_FORM == 0;
            let idle = |confirmed: bool| {
                self.slots.iter().rposition(
                    |s| matches!(s.state, State::Idle { confirmed: c } if c == confirmed),
                )
            };
            let Some(n) = idle(true).or_else(|| if presend { idle(false) } else { None }) else {
                return;
            };
            let confirmed = matches!(self.slots[n].state, State::Idle { confirmed: true });
            let Pending {
                link,
                id,
                mut frame,
            } = self.queue.pop_front().unwrap();
            let mut uploads = None;
            if frame.byte11 & goridge::NOVA_PARSE_FORM != 0 {
                match frame
                    .split()
                    .and_then(|(h, b)| crate::form::parse_request(&h, &b, &self.slots[n].tmp))
                {
                    Ok((h, b, m)) => {
                        frame = Frame::message(&h, &b);
                        uploads = m;
                    }
                    Err(e) => {
                        eprintln!("nova php-workers: request body: {e}");
                        let mut resp = Frame::message(br#"{"status":400,"headers":{}}"#, b"");
                        resp.options.push(id);
                        self.reply(link, &resp).await;
                        continue;
                    }
                }
            }
            frame.byte11 &= !goridge::NOVA_PARSE_FORM;
            frame.options.truncate(1);
            let slot = &mut self.slots[n];
            let sent = match slot.stdin.as_mut() {
                Some(stdin) => frame.write_to(stdin).await.is_ok(),
                None => false,
            };
            if sent {
                slot.state = State::Busy {
                    link,
                    id,
                    orphan: false,
                    uploads,
                    unread: (!confirmed).then(|| frame.clone()),
                };
                continue;
            }
            // The worker is gone before it got the request: try another.
            slot.state = State::Stopping;
            if let Some(c) = slot.child.as_mut() {
                let _ = c.start_kill();
            }
            frame.options.push(id);
            if uploads.is_some() {
                // Parsed again for the next worker.
                drop(uploads);
                self.reply(link, &failed(id)).await;
            } else {
                self.queue.push_front(Pending { link, id, frame });
            }
        }
    }

    async fn on_request_frame(&mut self, link: usize, mut f: Frame) {
        let Some(id) = f.options.get(1).copied() else {
            return;
        };
        if f.flags & goridge::CONTROL != 0 {
            // `{"cancel":true}`: NOVA gave up on this request.
            if f.payload.windows(6).any(|w| w == b"cancel") {
                self.queue.retain(|p| !(p.link == link && p.id == id));
                if let Some(n) = self.running(link, id) {
                    // It may be stuck: replace the worker.
                    let s = &mut self.slots[n];
                    s.state = State::Stopping;
                    if let Some(c) = s.child.as_mut() {
                        let _ = c.start_kill();
                    }
                }
            }
            return;
        }
        if f.byte10 & (goridge::PONG | goridge::STOP) != 0 {
            // Stream control for a running request.
            f.options.truncate(1);
            if let Some(n) = self.running(link, id)
                && let Some(stdin) = self.slots[n].stdin.as_mut()
            {
                let _ = f.write_to(stdin).await;
            }
            return;
        }
        self.queue.push_back(Pending { link, id, frame: f });
    }

    /// The worker running request `id` from `link`.
    fn running(&self, link: usize, id: u32) -> Option<usize> {
        self.slots.iter().position(|s| {
            matches!(s.state, State::Busy { link: l, id: i, orphan: false, .. } if l == link && i == id)
        })
    }

    fn nova_closed(&mut self, link: usize) {
        self.links[link] = None;
        self.queue.retain(|p| p.link != link);
        for s in &mut self.slots {
            if let State::Busy {
                link: l, orphan, ..
            } = &mut s.state
                && *l == link
            {
                *orphan = true;
            }
        }
    }

    async fn reply(&mut self, link: usize, f: &Frame) {
        if let Some(wr) = self.links[link].as_mut()
            && f.write_to(wr).await.is_err()
        {
            self.links[link] = None;
        }
    }

    async fn on_worker_frame(&mut self, n: usize, mut f: Frame) {
        if f.flags & goridge::CONTROL != 0 {
            if self.opts.ready && f.payload.windows(5).any(|w| w == b"ready") {
                match &mut self.slots[n].state {
                    State::Starting => self.available(n, true).await,
                    State::Idle { confirmed } => *confirmed = true,
                    // It reads the request sent ahead now.
                    State::Busy { unread, .. } => *unread = None,
                    _ => {}
                }
            }
            return; // e.g. a pid announcement
        }
        let State::Busy {
            link, id, orphan, ..
        } = self.slots[n].state
        else {
            return;
        };
        // A RoadRunner worker quitting instead of answering (`{"stop":true}`).
        if f.flags & goridge::ERROR == 0
            && f.byte11 & goridge::NOVA_CGI_HEAD == 0
            && f.split().is_ok_and(|(h, _)| {
                h.windows(6).any(|w| w == b"\"stop\"") && !h.windows(8).any(|w| w == b"\"status\"")
            })
        {
            if !orphan {
                self.reply(link, &failed(id)).await;
            }
            self.slots[n].state = State::Stopping;
            return;
        }
        if f.options.is_empty() {
            f.options.push(0);
        }
        f.options.truncate(1);
        f.options.push(id);
        if f.byte10 & goridge::STREAM != 0 {
            if !orphan {
                self.reply(link, &f).await;
            }
            return;
        }
        // The response is complete: give the worker its next request
        // first, then pass the answer on.
        let slot = &mut self.slots[n];
        slot.served += 1;
        slot.backoff = Duration::from_millis(250);
        if let State::Busy {
            uploads: Some(u), ..
        } = std::mem::replace(&mut slot.state, State::Starting)
        {
            drop(u);
            clean_tmp(&slot.tmp, |name| name.starts_with("symfony"));
        }
        self.available(n, !self.opts.ready).await;
        self.dispatch().await;
        if !orphan {
            self.reply(link, &f).await;
        }
    }

    /// The worker is (about to be) waiting for a request: make it
    /// available, or stop it when due.
    async fn available(&mut self, n: usize, confirmed: bool) {
        let s = &self.slots[n];
        let recycle = self.opts.max_requests > 0 && s.served >= self.opts.max_requests;
        if recycle || s.restart || (self.stopping && self.queue.is_empty()) {
            self.stop_slot(n).await;
        } else {
            self.slots[n].state = State::Idle { confirmed };
        }
    }

    /// Ask a worker to stop after its current request; kill it after the grace.
    async fn stop_slot(&mut self, n: usize) {
        let grace = self.opts.grace;
        let slot = &mut self.slots[n];
        if matches!(slot.state, State::Down | State::Stopping) {
            return;
        }
        slot.state = State::Stopping;
        if let Some(stdin) = slot.stdin.as_mut() {
            let _ = Frame::control(r#"{"stop":true}"#).write_to(stdin).await;
        }
        let (tx, generation) = (self.tx.clone(), slot.generation);
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            let _ = tx.send(Ev::KillIfStill(n, generation));
        });
    }

    async fn exited(&mut self, n: usize) {
        let slot = &mut self.slots[n];
        let crashed_early =
            slot.served == 0 && !slot.restart && slot.started.elapsed() < Duration::from_secs(2);
        let mut lost = None;
        match std::mem::replace(&mut slot.state, State::Down) {
            State::Busy {
                link,
                id,
                orphan: false,
                unread: Some(mut frame),
                ..
            } => {
                // Never read: another worker takes it.
                frame.options.push(id);
                self.queue.push_front(Pending { link, id, frame });
            }
            State::Busy {
                link,
                id,
                orphan: false,
                ..
            } => lost = Some((link, id)),
            _ => {}
        }
        let slot = &mut self.slots[n];
        slot.stdin = None;
        if let Some(mut c) = slot.child.take() {
            let _ = c.start_kill();
            let _ = c.wait().await;
        }
        if let Some((link, id)) = lost {
            // It crashed (or exited) while running this request.
            self.reply(link, &failed(id)).await;
        }
        if self.stopping {
            return;
        }
        let slot = &mut self.slots[n];
        if crashed_early {
            let wait = slot.backoff;
            slot.backoff = (slot.backoff * 2).min(Duration::from_secs(10));
            eprintln!("nova php-workers: worker {n} exited at start; retrying in {wait:?}");
            let tx = self.tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(wait).await;
                let _ = tx.send(Ev::Respawn(n));
            });
        } else {
            self.spawn(n);
        }
    }

    fn spawn(&mut self, n: usize) {
        let opts = &self.opts;
        let slot = &mut self.slots[n];
        clean_tmp(&slot.tmp, |_| true);
        slot.generation += 1;
        slot.served = 0;
        slot.started = Instant::now();
        slot.restart = false;
        let mut cmd = Command::new(&opts.command[0]);
        cmd.args(&opts.command[1..])
            .env("TMPDIR", &slot.tmp)
            .env("RR_MODE", "http")
            .env("RR_RELAY", "pipes")
            .env("RR_VERSION", "2025.1.0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            // Its own process group: NOVA's SIGTERM to the manager's group
            // must not kill workers in the middle of a request.
            .process_group(0)
            .kill_on_drop(true);
        // SAFETY: prctl is async-signal-safe; it only marks this child to die
        // with the manager, so no worker outlives a killed manager.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("nova php-workers: starting worker {n}: {e}");
                slot.state = State::Down;
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let _ = tx.send(Ev::Respawn(n));
                });
                return;
            }
        };
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        // Room for a whole page: PHP does not block halfway through writing
        // a response (or the manager a request) waiting for the other side.
        {
            use std::os::fd::AsRawFd;
            for fd in [stdin.as_raw_fd(), stdout.as_raw_fd()] {
                // SAFETY: F_SETPIPE_SZ on a pipe we own; failure is harmless.
                unsafe { libc::fcntl(fd, libc::F_SETPIPE_SZ, PIPE_SIZE) };
            }
        }
        let (tx, generation) = (self.tx.clone(), slot.generation);
        tokio::spawn(async move {
            let mut r = BufReader::with_capacity(64 << 10, stdout);
            while let Ok(f) = goridge::read_frame(&mut r, MAX_FRAME).await {
                if tx.send(Ev::FromWorker(n, generation, f)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Ev::WorkerExited(n, generation));
        });
        slot.child = Some(child);
        slot.stdin = Some(stdin);
        slot.state = if opts.ready {
            State::Starting
        } else {
            State::Idle { confirmed: true }
        };
    }
}

/// Tells NOVA a request failed in the worker (it crashed or quit).
fn failed(id: u32) -> Frame {
    let mut f = Frame::control(r#"{"failed":true}"#);
    f.options = vec![0, id];
    f
}
