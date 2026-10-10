//! NOVA Security: kernel-enforced isolation primitives.
//!
//! * [`Sandbox`]: a Landlock policy (filesystem paths, TCP ports, signal and
//!   abstract-socket scoping) a process applies to itself before exec'ing
//!   untrusted code. It survives exec and cannot be lifted afterwards.
//! * [`ensure_dir`] / [`chown_tree`]: ownership management for the
//!   privileged supervisor.

use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, NetPort, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus, Scope, path_beneath_rules,
};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Highest Landlock ABI this build knows; older kernels get best effort.
const TARGET_ABI: ABI = ABI::V9;

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("landlock: {0}")]
    Landlock(#[from] landlock::RulesetError),
    #[error("landlock is not available on this kernel, refusing to run unsandboxed")]
    Unavailable,
    #[error(
        "the kernel cannot enforce the required Landlock rules (filesystem: ABI 1, TCP: ABI 4) \
         ({0}); refusing to run with a weaker sandbox (set isolation.require_landlock = false to allow it)"
    )]
    Insufficient(landlock::RulesetError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforcement {
    Full,
    Partial,
    None,
}

impl Enforcement {
    pub fn as_str(self) -> &'static str {
        match self {
            Enforcement::Full => "full",
            Enforcement::Partial => "partial",
            Enforcement::None => "none",
        }
    }
}

/// What a sandboxed process may touch. Everything else is denied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sandbox {
    /// Read and execute (files and directory trees).
    pub read: Vec<PathBuf>,
    /// Full access, including creating sockets and files.
    pub write: Vec<PathBuf>,
    /// Outbound TCP ports.
    pub connect_tcp: Vec<u16>,
    /// TCP ports the process may listen on.
    pub bind_tcp: Vec<u16>,
}

impl Sandbox {
    /// Restrict the calling process (and everything it later executes).
    /// Missing paths are skipped.
    ///
    /// With `require`, the policy fails closed: the kernel must enforce the
    /// filesystem rules (Landlock ABI 1) and the TCP bind/connect rules
    /// (ABI 4), or nothing is applied and an error is returned. Only newer
    /// refinements (truncate, ioctl, scoping, ...) remain best effort.
    /// Without `require`, everything is best effort.
    pub fn apply(&self, require: bool) -> Result<Enforcement, SandboxError> {
        let abi = TARGET_ABI;
        let mut ruleset = Ruleset::default();
        if require {
            ruleset = ruleset
                .set_compatibility(CompatLevel::HardRequirement)
                .handle_access(AccessFs::from_all(ABI::V1))
                .and_then(|r| r.handle_access(AccessNet::from_all(ABI::V4)))
                .map_err(SandboxError::Insufficient)?
                .set_compatibility(CompatLevel::BestEffort);
        }
        let mut ruleset = ruleset
            .handle_access(AccessFs::from_all(abi))?
            .handle_access(AccessNet::from_all(abi))?
            .scope(Scope::from_all(abi))?
            .create()?;
        ruleset = ruleset.add_rules(path_beneath_rules(
            existing(&self.read),
            AccessFs::from_read(abi),
        ))?;
        ruleset = ruleset.add_rules(path_beneath_rules(
            existing(&self.write),
            AccessFs::from_all(abi),
        ))?;
        for &port in &self.connect_tcp {
            ruleset = ruleset.add_rule(NetPort::new(port, AccessNet::ConnectTcp))?;
        }
        for &port in &self.bind_tcp {
            ruleset = ruleset.add_rule(NetPort::new(port, AccessNet::BindTcp))?;
        }
        let status = ruleset.restrict_self()?;
        let level = match status.ruleset {
            RulesetStatus::FullyEnforced => Enforcement::Full,
            RulesetStatus::PartiallyEnforced => Enforcement::Partial,
            RulesetStatus::NotEnforced => Enforcement::None,
        };
        if require && level == Enforcement::None {
            return Err(SandboxError::Unavailable);
        }
        Ok(level)
    }

    /// Encode as `nova sandbox` arguments.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        for p in &self.read {
            args.push("--read".into());
            args.push(p.display().to_string());
        }
        for p in &self.write {
            args.push("--write".into());
            args.push(p.display().to_string());
        }
        for p in &self.connect_tcp {
            args.push("--connect".into());
            args.push(p.to_string());
        }
        for p in &self.bind_tcp {
            args.push("--bind".into());
            args.push(p.to_string());
        }
        args
    }
}

fn existing(paths: &[PathBuf]) -> Vec<&PathBuf> {
    paths.iter().filter(|p| p.exists()).collect()
}

/// System locations every sandboxed runtime needs to read: binaries and
/// shared libraries, the parts of `/etc` needed for linking, DNS, TLS roots
/// and time zones, and a few harmless `/proc` files PHP functions rely on.
/// The rest of `/etc` (accounts, service configuration) stays hidden.
pub fn system_read_paths() -> Vec<PathBuf> {
    [
        "/usr",
        "/lib",
        "/lib64",
        "/bin",
        "/sbin",
        "/etc/ld.so.cache",
        "/etc/ld.so.conf",
        "/etc/ld.so.conf.d",
        "/etc/ssl",
        "/etc/ca-certificates",
        "/etc/ca-certificates.conf",
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/nsswitch.conf",
        "/etc/host.conf",
        "/etc/gai.conf",
        "/etc/services",
        "/etc/protocols",
        "/etc/localtime",
        "/etc/timezone",
        "/etc/mime.types",
        "/proc/loadavg",
        "/proc/meminfo",
        "/proc/cpuinfo",
        "/proc/stat",
        "/sys/fs/cgroup",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

/// Device nodes that are safe to read and write.
pub fn safe_devices() -> Vec<PathBuf> {
    [
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/random",
        "/dev/urandom",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

/// Create `path` if needed and set owner and mode. Requires CAP_CHOWN (and
/// CAP_FOWNER/CAP_DAC_OVERRIDE when the directory belongs to someone else).
pub fn ensure_dir(path: &Path, uid: u32, gid: u32, mode: u32) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    std::os::unix::fs::chown(path, Some(uid), Some(gid))
}

/// Recursively give a tree to `uid:gid` (without following symlinks),
/// changing only entries with a different owner. Always walks the whole
/// tree: the root having the right owner says nothing about its contents
/// (e.g. files left by an earlier isolation mode).
pub fn chown_tree(root: &Path, uid: u32, gid: u32) -> io::Result<usize> {
    let mut changed = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        let m = std::fs::symlink_metadata(&p)?;
        if m.uid() != uid || m.gid() != gid {
            std::os::unix::fs::lchown(&p, Some(uid), Some(gid))?;
            changed += 1;
        }
        if m.is_dir() {
            for entry in std::fs::read_dir(&p)? {
                stack.push(entry?.path());
            }
        }
    }
    Ok(changed)
}

pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_roundtrip_shape() {
        let s = Sandbox {
            read: vec!["/usr".into()],
            write: vec!["/var/lib/x".into()],
            connect_tcp: vec![3306],
            bind_tcp: vec![],
        };
        assert_eq!(
            s.to_args(),
            [
                "--read",
                "/usr",
                "--write",
                "/var/lib/x",
                "--connect",
                "3306"
            ]
        );
    }

    /// Applies a real Landlock policy in a forked child (it cannot be undone
    /// in the test process). Skipped when the kernel lacks Landlock.
    #[test]
    fn landlock_denies_outside_paths_and_ports() {
        let dir = std::env::temp_dir().join(format!("nova-ll-{}", std::process::id()));
        let allowed = dir.join("allowed");
        let denied = dir.join("denied");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&denied).unwrap();
        std::fs::write(denied.join("secret"), "x").unwrap();

        // SAFETY: the child only runs simple syscalls and exits.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            let mut read = system_read_paths();
            read.push(allowed.clone());
            let sb = Sandbox {
                read,
                write: vec![allowed.clone()],
                connect_tcp: vec![1],
                bind_tcp: vec![],
            };
            // `require` exercises the fail-closed path: on kernels without
            // filesystem + TCP Landlock support it errors and the test skips.
            let code = match sb.apply(true) {
                Ok(Enforcement::None) | Err(_) => 77, // no (sufficient) landlock: skip
                Ok(_) => {
                    let can_write_allowed = std::fs::write(allowed.join("ok"), "y").is_ok();
                    let can_read_denied = std::fs::read(denied.join("secret")).is_ok();
                    let can_connect =
                        std::net::TcpStream::connect(("127.0.0.1", 9)).map_err(|e| e.kind());
                    let blocked_net = matches!(can_connect, Err(io::ErrorKind::PermissionDenied));
                    if can_write_allowed && !can_read_denied && blocked_net {
                        0
                    } else {
                        1
                    }
                }
            };
            unsafe { libc::_exit(code) };
        }
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        let code = libc::WEXITSTATUS(status);
        let _ = std::fs::remove_dir_all(&dir);
        if code == 77 {
            eprintln!("landlock unavailable; skipped");
            return;
        }
        assert_eq!(code, 0, "sandbox did not behave as expected");
    }
}
