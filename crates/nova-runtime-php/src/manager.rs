//! Worker mode, site side: `nova php-workers` runs as the site, inside its
//! sandbox, and keeps `processes` PHP workers alive. Each worker talks
//! goridge over its stdin/stdout (`RR_RELAY=pipes`); the manager connects
//! once per worker to NOVA's socket and relays frames between the two.
//!
//! Being the workers' parent, the manager enforces what a server must:
//!
//! * a worker whose connection closes mid-request (NOVA's timeout) is
//!   killed and replaced; one whose connection closes while idle (NOVA
//!   restarted) is kept and reconnected, still warm;
//! * a worker that exits is replaced (with backoff when it crashes at once);
//! * `{"stop":true}` from NOVA (after `max_requests`) stops the worker
//!   gracefully and starts a fresh one;
//! * before closing an idle connection the manager sends `{"stop":true}`
//!   itself, so NOVA retries a request that crossed it instead of failing;
//! * with `watch` (development), changed PHP code restarts the workers
//!   gracefully, as a booted application would otherwise keep old code;
//! * form bodies are parsed here, so uploads land in the site's own tmp;
//! * each worker gets a private `TMPDIR`, and after a request with uploads
//!   the copies frameworks make of them (Symfony's PSR-7 bridge, used by
//!   Octane, leaves `symfony*` files behind) are deleted.

use crate::goridge::{self, Frame};
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

const MAX_FRAME: usize = 1 << 30;

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
}

/// Run until SIGTERM/SIGINT, then let busy workers finish and stop all.
pub async fn run(opts: ManagerOptions) -> io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let (stop_tx, stop_rx) = watch::channel(false);
    let (code_tx, code_rx) = watch::channel(0u64);
    if let Some(dir) = opts.watch.clone() {
        tokio::spawn(watch_code(dir, code_tx));
    }
    let mut slots = Vec::new();
    for i in 0..opts.processes.max(1) {
        slots.push(tokio::spawn(slot(
            opts.clone(),
            i,
            stop_rx.clone(),
            code_rx.clone(),
        )));
    }
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    let _ = stop_tx.send(true);
    for s in slots {
        let _ = s.await;
    }
    Ok(())
}

/// Poll the project's PHP code once a second; bump the generation when
/// it changes (cheap enough for development projects).
async fn watch_code(dir: PathBuf, generation: watch::Sender<u64>) {
    let scan = move || code_fingerprint(&dir);
    let mut last = tokio::task::spawn_blocking(scan.clone()).await.ok();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let now = tokio::task::spawn_blocking(scan.clone()).await.ok();
        if now != last {
            last = now;
            generation.send_modify(|g| *g += 1);
            eprintln!("nova php-workers: code changed; restarting workers");
        }
    }
}

/// (files, newest mtime, mtime sum) of `.php` files, `.env` and
/// `composer.lock`, skipping dependency and state directories.
fn code_fingerprint(root: &std::path::Path) -> (u64, u128, u128) {
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

enum Outcome {
    /// NOVA closed an idle connection: keep the worker, reconnect.
    Reconnect,
    /// Replace the worker (it exited, was killed, or stopped).
    Respawn,
    /// Stop the worker gracefully and start a fresh one (code changed).
    Restart,
    Shutdown,
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    frames: mpsc::Receiver<io::Result<Frame>>,
    started: Instant,
    /// Requests answered; a worker that served any is not crash-looping.
    served: u64,
}

fn spawn(opts: &ManagerOptions, tmp: &std::path::Path) -> io::Result<Worker> {
    clean_tmp(tmp, |_| true);
    let mut cmd = Command::new(&opts.command[0]);
    cmd.args(&opts.command[1..])
        .env("TMPDIR", tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .env("RR_MODE", "http")
        .env("RR_RELAY", "pipes")
        .env("RR_VERSION", "2025.1.0")
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
    let mut child = cmd.spawn()?;
    let stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let (tx, frames) = mpsc::channel(4);
    tokio::spawn(async move {
        let mut r = BufReader::with_capacity(64 << 10, stdout);
        loop {
            let f = goridge::read_frame(&mut r, MAX_FRAME).await;
            let end = f.is_err();
            if tx.send(f).await.is_err() || end {
                return;
            }
        }
    });
    Ok(Worker {
        child,
        stdin,
        frames,
        started: Instant::now(),
        served: 0,
    })
}

/// Remove regular files in a worker's private temp directory.
fn clean_tmp(dir: &std::path::Path, select: impl Fn(&str) -> bool) {
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

async fn slot(
    mut opts: ManagerOptions,
    n: u32,
    mut stop: watch::Receiver<bool>,
    mut code: watch::Receiver<u64>,
) {
    let mut backoff = Duration::from_millis(250);
    // One request at a time per worker: whatever its uploads leave in this
    // directory belongs to the request that just finished.
    opts.tmp_dir = opts.tmp_dir.join(format!("w{n}"));
    {
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new().mode(0o700).create(&opts.tmp_dir);
    }
    'spawn: loop {
        if *stop.borrow() {
            return;
        }
        let mut w = match spawn(&opts, &opts.tmp_dir) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("nova php-workers: starting worker {n}: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        loop {
            let Some(conn) = connect(&opts, &mut stop, &mut w).await else {
                // Stopped while connecting, or the worker died idle.
                if *stop.borrow() {
                    finish(w, &opts).await;
                    return;
                }
                break;
            };
            match relay(conn, &mut w, &opts, &mut stop, &mut code).await {
                Outcome::Reconnect => continue,
                Outcome::Respawn => break,
                Outcome::Restart => {
                    finish(w, &opts).await;
                    continue 'spawn;
                }
                Outcome::Shutdown => {
                    finish(w, &opts).await;
                    return;
                }
            }
        }
        let lived = w.started.elapsed();
        let _ = w.child.start_kill();
        let _ = w.child.wait().await;
        if w.served == 0 && lived < Duration::from_secs(2) {
            // Crashing at startup: do not spin.
            eprintln!(
                "nova php-workers: worker {n} exited after {lived:?}; retrying in {backoff:?}"
            );
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = stop.changed() => continue 'spawn,
            }
            backoff = (backoff * 2).min(Duration::from_secs(10));
        } else {
            backoff = Duration::from_millis(250);
        }
    }
}

/// Connect to NOVA (retrying while it starts). `None` when stopping or
/// when the worker exits in the meantime.
async fn connect(
    opts: &ManagerOptions,
    stop: &mut watch::Receiver<bool>,
    w: &mut Worker,
) -> Option<UnixStream> {
    loop {
        if *stop.borrow() {
            return None;
        }
        if let Ok(s) = UnixStream::connect(&opts.socket).await {
            return Some(s);
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            _ = stop.changed() => {}
            _ = w.child.wait() => return None,
        }
    }
}

/// Stop a worker gracefully: `{"stop":true}`, then kill after the grace.
async fn finish(mut w: Worker, opts: &ManagerOptions) {
    let _ = Frame::control(r#"{"stop":true}"#)
        .write_to(&mut w.stdin)
        .await;
    if tokio::time::timeout(opts.grace, w.child.wait())
        .await
        .is_err()
    {
        let _ = w.child.start_kill();
        let _ = w.child.wait().await;
    }
}

async fn relay(
    conn: UnixStream,
    w: &mut Worker,
    opts: &ManagerOptions,
    stop: &mut watch::Receiver<bool>,
    code: &mut watch::Receiver<u64>,
) -> Outcome {
    let (rd, mut wr) = conn.into_split();
    let (tx, mut from_nova) = mpsc::channel::<io::Result<Frame>>(4);
    let reader = tokio::spawn(async move {
        let mut r = BufReader::with_capacity(64 << 10, rd);
        loop {
            let f = goridge::read_frame(&mut r, MAX_FRAME).await;
            let end = f.is_err();
            if tx.send(f).await.is_err() || end {
                return;
            }
        }
    });
    let _abort = AbortOnDrop(reader);
    let mut busy = false;
    // Uploaded files of the current request; deleted when it completes.
    let mut uploads = None;
    let mut stopping = *stop.borrow();
    let mut restart = false;
    loop {
        if (stopping || restart) && !busy {
            let _ = Frame::control(r#"{"stop":true}"#).write_to(&mut wr).await;
            return if stopping {
                Outcome::Shutdown
            } else {
                Outcome::Restart
            };
        }
        tokio::select! {
            f = from_nova.recv() => {
                let Some(Ok(mut f)) = f else {
                    if busy {
                        // NOVA gave up on this request (timeout): the worker
                        // may be stuck, replace it.
                        return Outcome::Respawn;
                    }
                    return Outcome::Reconnect;
                };
                if f.flags & goridge::CONTROL != 0 {
                    if f.payload.windows(4).any(|w| w == b"stop") {
                        // max_requests reached: stop gracefully, then respawn.
                        let _ = f.write_to(&mut w.stdin).await;
                        if tokio::time::timeout(opts.grace, w.child.wait()).await.is_err() {
                            let _ = w.child.start_kill();
                        }
                        return Outcome::Respawn;
                    }
                    continue;
                }
                if !busy && f.byte10 == 0 {
                    busy = true;
                    if f.byte11 & goridge::NOVA_PARSE_FORM != 0 {
                        match f.split().and_then(|(h, b)| crate::form::parse_request(&h, &b, &opts.tmp_dir)) {
                            Ok((h, b, m)) => {
                                f = Frame::message(&h, &b);
                                uploads = m;
                            }
                            Err(e) => {
                                eprintln!("nova php-workers: request body: {e}");
                                let mut resp = Frame::message(br#"{"status":400,"headers":{}}"#, b"");
                                resp.byte10 = 0;
                                let _ = resp.write_to(&mut wr).await;
                                busy = false;
                                continue;
                            }
                        }
                    }
                    f.byte11 = 0;
                }
                if f.write_to(&mut w.stdin).await.is_err() {
                    return Outcome::Respawn;
                }
            }
            f = w.frames.recv() => {
                let Some(Ok(f)) = f else {
                    // The worker exited (crash, exit(), memory limit).
                    if !busy {
                        let _ = Frame::control(r#"{"stop":true}"#).write_to(&mut wr).await;
                    }
                    return Outcome::Respawn;
                };
                let last = f.flags & goridge::CONTROL == 0 && f.byte10 & goridge::STREAM == 0;
                let mut buf = bytes::BytesMut::new();
                f.encode(&mut buf);
                if wr.write_all(&buf).await.is_err() {
                    return if busy { Outcome::Respawn } else { Outcome::Reconnect };
                }
                if busy && last {
                    busy = false;
                    w.served += 1;
                    if uploads.take().is_some() {
                        clean_tmp(&opts.tmp_dir, |n| n.starts_with("symfony"));
                    }
                }
            }
            _ = stop.changed(), if !stopping => {
                stopping = true;
            }
            _ = code.changed(), if !restart => {
                restart = true;
            }
        }
        let _ = &uploads;
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
