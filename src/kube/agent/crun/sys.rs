//! The Linux system calls the crun backend needs (mounts, namespaces,
//! pidfds, bridge ports). On other platforms they fail: the backend is
//! never selected there, but the rest of it compiles and is unit-tested.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// The overlayfs mount data for `lowers` (top layer first). Mount data is
/// limited to one page, which bounds how many layers an image may have.
///
/// An image may contain the same layer twice (an identical or empty diff);
/// overlayfs refuses overlapping lower directories (ELOOP), so only the
/// topmost occurrence is kept. That is the same union: everything the
/// lower copy adds or removes, the upper copy adds or removes again on top
/// of the layers in between.
pub(crate) fn overlay_options(lowers: &[PathBuf], upper: &Path, work: &Path) -> Result<String> {
    if lowers.is_empty() {
        bail!("an image needs at least one layer");
    }
    let mut seen = std::collections::HashSet::new();
    let lowers: Vec<PathBuf> = lowers.iter().filter(|p| seen.insert(p.as_path())).cloned().collect();
    for p in lowers.iter().map(PathBuf::as_path).chain([upper, work]) {
        let s = p.to_string_lossy();
        if s.contains(',') || s.contains(':') {
            bail!("path {s} cannot be used in an overlay mount");
        }
    }
    let lower = lowers.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join(":");
    let options = format!("lowerdir={lower},upperdir={},workdir={}", upper.display(), work.display());
    if options.len() >= 4096 {
        bail!("image has too many layers for an overlay mount ({} layers)", lowers.len());
    }
    Ok(options)
}

#[cfg(target_os = "linux")]
pub(crate) use linux::*;

#[cfg(not(target_os = "linux"))]
pub(crate) use other::*;

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::CString;
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result, bail};

    fn cstr(p: &Path) -> Result<CString> {
        Ok(CString::new(p.as_os_str().as_encoded_bytes())?)
    }

    fn check(rc: libc::c_int, what: impl FnOnce() -> String) -> Result<()> {
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).with_context(what);
        }
        Ok(())
    }

    pub(crate) fn mount_overlay(target: &Path, options: &str) -> Result<()> {
        let data = CString::new(options)?;
        let t = cstr(target)?;
        // SAFETY: all pointers are valid NUL-terminated strings.
        let rc = unsafe { libc::mount(c"overlay".as_ptr(), t.as_ptr(), c"overlay".as_ptr(), 0, data.as_ptr().cast()) };
        check(rc, || format!("failed to mount the container root filesystem at {}", target.display()))
    }

    pub(crate) fn mount_tmpfs(target: &Path, options: &str) -> Result<()> {
        let data = CString::new(options)?;
        let t = cstr(target)?;
        // SAFETY: valid strings.
        let rc = unsafe {
            libc::mount(c"tmpfs".as_ptr(), t.as_ptr(), c"tmpfs".as_ptr(), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, data.as_ptr().cast())
        };
        check(rc, || format!("failed to mount tmpfs at {}", target.display()))
    }

    /// Lazily unmount `target` if something is mounted there.
    pub(crate) fn unmount(target: &Path) {
        if let Ok(t) = cstr(target) {
            // SAFETY: valid string; failure (not mounted) is fine.
            unsafe { libc::umount2(t.as_ptr(), libc::MNT_DETACH) };
        }
    }

    fn bind(source: &Path, target: &Path) -> Result<()> {
        let (s, t) = (cstr(source)?, cstr(target)?);
        // SAFETY: valid strings.
        let rc = unsafe { libc::mount(s.as_ptr(), t.as_ptr(), std::ptr::null(), libc::MS_BIND, std::ptr::null()) };
        check(rc, || format!("failed to bind {} to {}", source.display(), target.display()))
    }

    fn is_mount_point(dir: &Path) -> bool {
        let wanted = dir.to_string_lossy();
        std::fs::read_to_string("/proc/self/mountinfo")
            .is_ok_and(|m| m.lines().any(|l| l.split(' ').nth(4) == Some(wanted.as_ref())))
    }

    /// Make `dir` a shared mount of itself (as `ip netns` does for
    /// /run/netns), so unmounting a namespace file there also drops copies
    /// other mount namespaces made of it and the namespace is freed.
    pub(crate) fn ensure_shared_dir(dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let d = cstr(dir)?;
        if !is_mount_point(dir) {
            // SAFETY: valid strings.
            let rc = unsafe { libc::mount(d.as_ptr(), d.as_ptr(), std::ptr::null(), libc::MS_BIND | libc::MS_REC, std::ptr::null()) };
            check(rc, || format!("failed to bind {}", dir.display()))?;
        }
        // SAFETY: valid string.
        let rc = unsafe { libc::mount(std::ptr::null(), d.as_ptr(), std::ptr::null(), libc::MS_SHARED | libc::MS_REC, std::ptr::null()) };
        check(rc, || format!("failed to make {} shared", dir.display()))
    }

    /// Create the pod's namespaces on a throwaway thread and keep them alive
    /// as bind mounts: IPC at `ipc`, and network at `net` (unless the pod
    /// uses the host's). `ip_batch` runs inside the new network namespace;
    /// `sysctls` (`net.ipv4.x` = value) are set there.
    pub(crate) fn create_namespaces(net: Option<PathBuf>, ipc: PathBuf, ip_batch: Option<String>, sysctls: Vec<(String, String)>) -> Result<()> {
        std::thread::spawn(move || -> Result<()> {
            let mut flags = libc::CLONE_NEWIPC;
            if net.is_some() {
                flags |= libc::CLONE_NEWNET;
            }
            // SAFETY: affects only this thread, which ends right after.
            check(unsafe { libc::unshare(flags) }, || "failed to create pod namespaces".into())?;
            for target in [Some(&ipc), net.as_ref()].into_iter().flatten() {
                std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(target)?;
            }
            bind(Path::new("/proc/thread-self/ns/ipc"), &ipc)?;
            if let Some(n) = &net {
                bind(Path::new("/proc/thread-self/ns/net"), n)?;
            }
            for (key, value) in &sysctls {
                let path = format!("/proc/sys/{}", key.replace('.', "/"));
                std::fs::write(&path, value).with_context(|| format!("failed to set sysctl {key}"))?;
            }
            if let Some(batch) = ip_batch {
                // A child inherits this thread's namespaces.
                let mut child = std::process::Command::new("ip")
                    .args(["-batch", "-"])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .context("failed to run ip (is iproute2 installed?)")?;
                child.stdin.take().unwrap().write_all(batch.as_bytes())?;
                let out = child.wait_with_output()?;
                if !out.status.success() {
                    bail!("pod network setup failed: {}", String::from_utf8_lossy(&out.stderr).trim());
                }
            }
            Ok(())
        })
        .join()
        .map_err(|_| anyhow::anyhow!("namespace thread panicked"))?
    }

    /// Unmount and remove a namespace file.
    pub(crate) fn remove_ns_file(path: &Path) {
        unmount(path);
        let _ = std::fs::remove_file(path);
    }

    const SIOCBRADDIF: libc::c_ulong = 0x89a2;

    fn ifreq(name: &str) -> Result<libc::ifreq> {
        // SAFETY: ifreq is plain data; all-zero is valid.
        let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
        if name.len() >= req.ifr_name.len() {
            bail!("interface name {name} too long");
        }
        for (dst, src) in req.ifr_name.iter_mut().zip(name.bytes()) {
            *dst = src as libc::c_char;
        }
        Ok(req)
    }

    /// Attach `port` to `bridge` and bring it up (host network namespace).
    pub(crate) fn bridge_attach(bridge: &str, port: &str) -> Result<()> {
        // SAFETY: plain socket for ioctls, closed by OwnedFd.
        let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if sock < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let sock = unsafe { OwnedFd::from_raw_fd(sock) };
        let pname = CString::new(port)?;
        // SAFETY: valid string.
        let index = unsafe { libc::if_nametoindex(pname.as_ptr()) };
        if index == 0 {
            bail!("interface {port} not found");
        }
        let mut req = ifreq(bridge)?;
        req.ifr_ifru.ifru_ifindex = index as libc::c_int;
        // SAFETY: SIOCBRADDIF reads ifr_name and ifr_ifindex.
        check(unsafe { libc::ioctl(sock.as_raw_fd(), SIOCBRADDIF as _, &mut req) }, || format!("failed to add {port} to {bridge}"))?;
        let mut req = ifreq(port)?;
        // SAFETY: SIOCGIFFLAGS/SIOCSIFFLAGS read and write ifr_flags.
        check(unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCGIFFLAGS as _, &mut req) }, || format!("failed to read {port} flags"))?;
        unsafe { req.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short };
        check(unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCSIFFLAGS as _, &mut req) }, || format!("failed to bring {port} up"))
    }

    /// Connect to `port` on the pod's loopback, from inside its network
    /// namespace (what the kubelet's port-forward does).
    pub(crate) fn connect_in_netns(netns: PathBuf, port: u16) -> std::io::Result<std::net::TcpStream> {
        std::thread::spawn(move || {
            let f = std::fs::File::open(&netns)?;
            // SAFETY: setns on this short-lived thread only.
            if unsafe { libc::setns(f.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), std::time::Duration::from_secs(1))
        })
        .join()
        .unwrap_or_else(|_| Err(std::io::Error::other("connect thread panicked")))
    }

    /// Accept one connection on `listener` and receive the file descriptor
    /// sent over it (crun's console socket hands over the pty master).
    pub(crate) fn receive_fd(listener: &std::os::unix::net::UnixListener, timeout_ms: i32) -> std::io::Result<OwnedFd> {
        let mut pfd = libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut pfd, 1, timeout_ms) } <= 0 {
            return Err(std::io::Error::other("timed out waiting for the terminal"));
        }
        let (stream, _) = listener.accept()?;
        let mut buf = [0u8; 256];
        let mut cmsg = [0u8; 64];
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
        // SAFETY: msghdr points at buffers that outlive the call.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg.as_mut_ptr().cast();
        msg.msg_controllen = cmsg.len() as _;
        if unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: walking the control messages the kernel filled in.
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            if !c.is_null() && (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let fd = std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const std::os::fd::RawFd);
                return Ok(OwnedFd::from_raw_fd(fd));
            }
        }
        Err(std::io::Error::other("no file descriptor received"))
    }

    pub(crate) fn pidfd_open(pid: i32) -> std::io::Result<OwnedFd> {
        // SAFETY: pidfd_open has no memory arguments.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    }
}

#[cfg(not(target_os = "linux"))]
mod other {
    use std::path::{Path, PathBuf};

    use anyhow::{Result, bail};

    pub(crate) fn mount_overlay(_: &Path, _: &str) -> Result<()> {
        bail!("the crun runtime needs Linux")
    }
    pub(crate) fn mount_tmpfs(_: &Path, _: &str) -> Result<()> {
        bail!("the crun runtime needs Linux")
    }
    pub(crate) fn unmount(_: &Path) {}
    pub(crate) fn ensure_shared_dir(_: &Path) -> Result<()> {
        bail!("the crun runtime needs Linux")
    }
    pub(crate) fn create_namespaces(_: Option<PathBuf>, _: PathBuf, _: Option<String>, _: Vec<(String, String)>) -> Result<()> {
        bail!("the crun runtime needs Linux")
    }
    pub(crate) fn remove_ns_file(_: &Path) {}
    pub(crate) fn bridge_attach(_: &str, _: &str) -> Result<()> {
        bail!("the crun runtime needs Linux")
    }
    pub(crate) fn connect_in_netns(_: PathBuf, _: u16) -> std::io::Result<std::net::TcpStream> {
        Err(std::io::Error::other("the crun runtime needs Linux"))
    }
    pub(crate) fn receive_fd(_: &std::os::unix::net::UnixListener, _: i32) -> std::io::Result<std::os::fd::OwnedFd> {
        Err(std::io::Error::other("the crun runtime needs Linux"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_options_put_the_top_layer_first() {
        let lowers = vec![PathBuf::from("/s/l/top"), PathBuf::from("/s/l/base")];
        let o = overlay_options(&lowers, Path::new("/c/1/upper"), Path::new("/c/1/work")).unwrap();
        assert_eq!(o, "lowerdir=/s/l/top:/s/l/base,upperdir=/c/1/upper,workdir=/c/1/work");
        assert!(overlay_options(&[], Path::new("/u"), Path::new("/w")).is_err());
        let repeated = vec![PathBuf::from("/l/a"), PathBuf::from("/l/b"), PathBuf::from("/l/a"), PathBuf::from("/l/c")];
        let o = overlay_options(&repeated, Path::new("/u"), Path::new("/w")).unwrap();
        assert_eq!(o, "lowerdir=/l/a:/l/b:/l/c,upperdir=/u,workdir=/w");
        assert!(overlay_options(&[PathBuf::from("/a:b")], Path::new("/u"), Path::new("/w")).is_err());
        let many: Vec<PathBuf> = (0..200).map(|i| PathBuf::from(format!("/var/lib/litek8s/node-agent/crun/images/l/{i:016x}"))).collect();
        assert!(overlay_options(&many, Path::new("/u"), Path::new("/w")).is_err_and(|e| e.to_string().contains("too many layers")));
    }
}
