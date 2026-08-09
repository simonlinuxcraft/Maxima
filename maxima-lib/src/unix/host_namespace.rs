// MAXIMA-LINUX-PORT-MOD: make a hosted listen server reachable from its own
// client half on distributions that map the machine hostname to something
// other than 127.0.0.1.
//
// Kyber starts the listen server with an empty ServerIp, so Frostbite resolves
// the local address through the hostname. Debian, Ubuntu and other installers
// write `127.0.1.1 <hostname>` into /etc/hosts by convention. The server binds
// 0.0.0.0 and answers from 127.0.0.1, the reply arrives from an address the
// client did not connect to, and the local link never establishes ("The server
// did not reply."). The game then dies during the ingame transition.
//
// Rather than asking every user to edit /etc/hosts, the game is launched in its
// own UTS namespace whose hostname is "localhost", a name glibc resolves to
// 127.0.0.1 everywhere. Nothing outside the game process is affected and no
// privileges are required.

use std::io;

const CLONE_NEWUSER: libc::c_int = 0x1000_0000;
const CLONE_NEWUTS: libc::c_int = 0x0400_0000;
const LOCALHOST: &[u8] = b"localhost";

/// Whether the machine hostname resolves to a loopback address that is not
/// 127.0.0.1, which is the case this works around. Anything unexpected
/// (no hostname, resolver failure) reports false so the launch path stays
/// exactly as it was.
pub fn hostname_is_mismatched_loopback() -> bool {
    use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};

    let host = match std::fs::read_to_string("/proc/sys/kernel/hostname") {
        Ok(name) => name.trim().to_owned(),
        Err(_) => return false,
    };
    if host.is_empty() || host == "localhost" {
        return false;
    }

    match (host.as_str(), 0u16).to_socket_addrs() {
        Ok(addrs) => addrs.into_iter().any(|addr| match addr.ip() {
            IpAddr::V4(v4) => v4.is_loopback() && v4 != Ipv4Addr::LOCALHOST,
            IpAddr::V6(_) => false,
        }),
        Err(_) => false,
    }
}

/// The uid/gid maps for the new user namespace, formatted while allocation is
/// still allowed. Mapping our own id onto itself keeps the game running as the
/// same user; becoming root would break umu-run, which refuses to start as root.
pub struct NamespacePlan {
    uid_map: Vec<u8>,
    gid_map: Vec<u8>,
}

impl NamespacePlan {
    pub fn new() -> Self {
        // Safety: getuid/getgid cannot fail and touch no shared state.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Self {
            uid_map: format!("{uid} {uid} 1").into_bytes(),
            gid_map: format!("{gid} {gid} 1").into_bytes(),
        }
    }

    /// Runs between fork and exec. Only raw syscalls are allowed here: after a
    /// fork in a threaded process anything that allocates can deadlock, which
    /// is why the maps above are built in the parent.
    ///
    /// Every failure is swallowed. A kernel that forbids unprivileged user
    /// namespaces (hardened kernels, some distributions) must still launch the
    /// game, just without the workaround.
    ///
    /// # Safety
    /// Must only be called from `Command::pre_exec`.
    pub unsafe fn apply(&self) -> io::Result<()> {
        if libc::unshare(CLONE_NEWUSER | CLONE_NEWUTS) != 0 {
            return Ok(());
        }

        // setgroups has to be denied before gid_map may be written.
        write_proc_file(c"/proc/self/setgroups", b"deny");
        write_proc_file(c"/proc/self/uid_map", &self.uid_map);
        write_proc_file(c"/proc/self/gid_map", &self.gid_map);

        // Has to happen here rather than through /bin/hostname: exec drops the
        // capabilities that came with the new namespace, so a helper binary
        // would be denied.
        libc::sethostname(LOCALHOST.as_ptr().cast(), LOCALHOST.len());
        Ok(())
    }
}

/// Best-effort write with raw syscalls, safe to call after fork.
unsafe fn write_proc_file(path: &std::ffi::CStr, contents: &[u8]) {
    let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
    if fd < 0 {
        return;
    }
    libc::write(fd, contents.as_ptr().cast(), contents.len());
    libc::close(fd);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    /// The point of the whole module: a child launched this way sees
    /// "localhost" while the machine keeps its real name. Skipped on kernels
    /// that forbid unprivileged user namespaces, where the launch path falls
    /// back to the unchanged behaviour anyway.
    #[test]
    fn child_sees_localhost_and_the_host_is_untouched() {
        let real = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
        let real = real.trim().to_owned();

        let plan = NamespacePlan::new();
        let mut cmd = Command::new("cat");
        cmd.arg("/proc/sys/kernel/hostname");
        unsafe { cmd.pre_exec(move || plan.apply()) };

        let out = cmd.output().expect("child ran");
        let seen = String::from_utf8_lossy(&out.stdout).trim().to_owned();

        if seen == real {
            eprintln!("skipped: unprivileged user namespaces unavailable here");
            return;
        }
        assert_eq!(seen, "localhost");
        assert_eq!(
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .unwrap()
                .trim(),
            real,
            "the machine hostname must not change"
        );
    }

    #[test]
    fn detection_does_not_panic() {
        let _ = hostname_is_mismatched_loopback();
    }
}
