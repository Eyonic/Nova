//! Site background processes: cron-scheduled tasks (`[[site.task]]`) and
//! supervised long-running workers (`[[site.worker]]`), e.g. Laravel's
//! `schedule:run` and `queue:work`.
//!
//! Each command runs exactly like the site's PHP: under the site's uid,
//! inside its Landlock sandbox, with the site's environment, its project
//! directory as working directory and its state directory as `HOME`/`TMPDIR`.
//! Output is forwarded line by line into NOVA's log.

use crate::isolation::Effective;
use crate::{php, sites};
use anyhow::{Context, Result};
use nova_config::cron::{Moment, Schedule};
use nova_config::{Config, SiteConfig};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;

#[derive(Debug, Clone)]
struct ProcSpec {
    site: String,
    name: String,
    argv: Vec<OsString>,
    env: BTreeMap<String, String>,
    cwd: PathBuf,
    user: Option<(u32, u32)>,
}

pub struct Tasks {
    stop: watch::Sender<bool>,
    handles: Vec<JoinHandle<()>>,
}

impl Tasks {
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// Stop schedules and workers; running commands get SIGTERM, then SIGKILL.
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        futures_util::future::join_all(self.handles).await;
    }
}

fn spec(
    cfg: &Config,
    site: &SiteConfig,
    mode: Effective,
    name: &str,
    command: &[String],
) -> Result<ProcSpec> {
    let nova = std::env::current_exe().context("locating the nova binary")?;
    let mut argv = php::sandbox_wrapper(cfg, site, &nova);
    let mut command = command.iter().map(OsString::from).collect::<Vec<_>>();
    if command[0] == "php" {
        // The CLI gets the same OPcache/realpath tuning and memory limit as the pool.
        command[0] = cfg.php.cli_binary.clone().into_os_string();
        let mut flags = Vec::new();
        for (k, v) in php::master_ini(cfg, site) {
            flags.push(OsString::from("-d"));
            flags.push(format!("{k}={v}").into());
        }
        if let Some(p) = &site.php {
            flags.push("-d".into());
            flags.push(format!("memory_limit={}", p.memory_limit.to_php()).into());
        }
        command.splice(1..1, flags);
    }
    argv.extend(command);

    let state = sites::site_state_dir(cfg, site);
    let mut env = sites::site_env(cfg, site).map_err(anyhow::Error::msg)?;
    env.insert(
        "PATH".into(),
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
    );
    env.insert("HOME".into(), state.display().to_string());
    env.insert("TMPDIR".into(), state.join("tmp").display().to_string());
    if let Ok(tz) = std::env::var("TZ") {
        env.insert("TZ".into(), tz);
    }
    let uid = cfg.site_uid(site);
    Ok(ProcSpec {
        site: site.name.clone(),
        name: name.to_string(),
        argv,
        env,
        cwd: site.path.clone(),
        user: (mode == Effective::Strict).then_some((uid, uid)),
    })
}

/// Start every configured task schedule and worker.
pub fn start_all(cfg: &Config, mode: Effective) -> Result<Tasks> {
    let (stop, stop_rx) = watch::channel(false);
    let grace = Duration::from_secs(cfg.server.shutdown_grace_secs);
    let mut handles = Vec::new();
    for site in &cfg.sites {
        for t in &site.tasks {
            let schedule: Schedule = t.schedule.parse().map_err(anyhow::Error::msg)?;
            let spec = spec(cfg, site, mode, &t.name, &t.command)?;
            let timeout = Duration::from_secs(t.timeout_secs.max(1));
            tracing::info!(
                site = site.name,
                task = t.name,
                schedule = t.schedule,
                "task scheduled"
            );
            handles.push(tokio::spawn(run_task(
                spec,
                schedule,
                timeout,
                stop_rx.clone(),
                grace,
            )));
        }
        for w in &site.workers {
            for i in 0..w.processes {
                let mut spec = spec(cfg, site, mode, &w.name, &w.command)?;
                if w.processes > 1 {
                    spec.name = format!("{}#{i}", w.name);
                }
                tracing::info!(site = site.name, worker = spec.name, "worker started");
                handles.push(tokio::spawn(run_worker(spec, stop_rx.clone(), grace)));
            }
        }
    }
    Ok(Tasks { stop, handles })
}

fn spawn(spec: &ProcSpec) -> std::io::Result<Child> {
    let mut cmd = Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..])
        .env_clear()
        .envs(&spec.env)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group: stopping a task also stops what it spawned.
        .process_group(0)
        .kill_on_drop(true);
    if let Some((uid, gid)) = spec.user {
        cmd.uid(uid).gid(gid);
    }
    let mut child = cmd.spawn()?;
    for (stream, is_err) in [
        (
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            false,
        ),
        (
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            true,
        ),
    ] {
        let Some(stream) = stream else { continue };
        let (site, name) = (spec.site.clone(), spec.name.clone());
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // The sandbox wrapper announces itself on stderr; that is not a warning.
                if is_err && !line.starts_with("nova sandbox:") {
                    tracing::warn!(target: "nova::task", site, task = name, "{line}");
                } else {
                    tracing::info!(target: "nova::task", site, task = name, "{line}");
                }
            }
        });
    }
    Ok(child)
}

fn signal_group(child: &Child, sig: libc::c_int) {
    if let Some(pid) = child.id() {
        // SAFETY: kill(2) on the process group we created for our own child.
        unsafe { libc::kill(-(pid as libc::pid_t), sig) };
    }
}

async fn terminate(mut child: Child, grace: Duration) {
    signal_group(&child, libc::SIGTERM);
    if tokio::time::timeout(grace, child.wait()).await.is_err() {
        signal_group(&child, libc::SIGKILL);
        let _ = child.wait().await;
    }
}

async fn run_worker(spec: ProcSpec, mut stop: watch::Receiver<bool>, grace: Duration) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if *stop.borrow() {
            return;
        }
        let started = Instant::now();
        match spawn(&spec) {
            Ok(mut child) => {
                tokio::select! {
                    status = child.wait() => {
                        tracing::warn!(site = spec.site, worker = spec.name, ?status, "worker exited; restarting");
                    }
                    _ = stop.changed() => {
                        terminate(child, grace).await;
                        tracing::info!(site = spec.site, worker = spec.name, "worker stopped");
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::error!(site = spec.site, worker = spec.name, error = %e, "cannot start worker")
            }
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = stop.changed() => return,
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn run_task(
    spec: ProcSpec,
    schedule: Schedule,
    timeout: Duration,
    mut stop: watch::Receiver<bool>,
    grace: Duration,
) {
    loop {
        let now = unix_now();
        let Some(next) = next_run(&schedule, now) else {
            tracing::warn!(
                site = spec.site,
                task = spec.name,
                "schedule never matches within a year"
            );
            return;
        };
        let wait = Duration::from_secs(next.saturating_sub(now));
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = stop.changed() => return,
        }
        let started = Instant::now();
        let mut child = match spawn(&spec) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(site = spec.site, task = spec.name, error = %e, "cannot start task");
                continue;
            }
        };
        // Runs never overlap: the next one is scheduled after this one ends.
        tokio::select! {
            status = tokio::time::timeout(timeout, child.wait()) => match status {
                Ok(Ok(s)) => tracing::info!(
                    target: "nova::task",
                    site = spec.site, task = spec.name,
                    exit = s.code(), duration_ms = started.elapsed().as_millis() as u64,
                    "task finished"
                ),
                Ok(Err(e)) => tracing::warn!(site = spec.site, task = spec.name, error = %e, "task failed"),
                Err(_) => {
                    tracing::warn!(site = spec.site, task = spec.name, ?timeout, "task timed out; stopping it");
                    terminate(child, grace).await;
                }
            },
            _ = stop.changed() => {
                terminate(child, grace).await;
                return;
            }
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn local(t: u64) -> Moment {
    let secs = t as libc::time_t;
    // SAFETY: localtime_r writes into the provided struct only.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    Moment {
        minute: tm.tm_min as u32,
        hour: tm.tm_hour as u32,
        day: tm.tm_mday as u32,
        month: tm.tm_mon as u32 + 1,
        weekday: tm.tm_wday as u32,
    }
}

/// Start of the next minute (strictly after `now`) the schedule matches.
fn next_run(schedule: &Schedule, now: u64) -> Option<u64> {
    let first = (now / 60 + 1) * 60;
    (0..366 * 24 * 60)
        .map(|i| first + i * 60)
        .find(|t| schedule.matches(&local(*t)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_minute_boundaries() {
        // SAFETY: test-only; the process has no other threads reading TZ yet.
        unsafe { std::env::set_var("TZ", "UTC") };
        let every: Schedule = "* * * * *".parse().unwrap();
        assert_eq!(next_run(&every, 1_000_000_000), Some(1_000_000_020));
        assert_eq!(next_run(&every, 1_000_000_020), Some(1_000_000_080));
        // 2001-09-09 01:46:40 UTC → next 02:00 run.
        let hourly: Schedule = "@hourly".parse().unwrap();
        assert_eq!(next_run(&hourly, 1_000_000_000), Some(1_000_000_800));
    }
}
