//! `litek8s docker-fs`: copy files in and out of a container (`docker cp`,
//! `GET/PUT/HEAD /containers/{id}/archive`).
//!
//! Paths must resolve inside the container, symlinks included, with its
//! volumes and binds where they are mounted. So this runs as a separate
//! process (started from this binary before any async runtime, as setns
//! into a mount namespace needs a single thread) that either joins the
//! running container's mount namespace or, for a stopped one, builds a
//! private one: the overlay root filesystem with the mounts on top, then
//! chroot. It reads or writes a tar stream on stdin/stdout.
//!
//! ```text
//! litek8s docker-fs --pid <pid> <op> <path>
//! litek8s docker-fs --rootfs <dir> --overlay <options> [--bind <src>=<dst>[:ro]]... <op> <path>
//! op: stat | get | put [--no-overwrite-dir-non-dir] [--chown uid:gid]
//! ```
//! Exit status 2 means the path does not exist.

use serde_json::json;

/// Go's `os.FileMode` (what `X-Docker-Container-Path-Stat` carries).
#[cfg(unix)]
pub fn go_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let raw = meta.permissions().mode();
    let mut m = raw & 0o777;
    let ft = meta.file_type();
    if ft.is_dir() {
        m |= 1 << 31;
    }
    if ft.is_symlink() {
        m |= 1 << 27;
    }
    if ft.is_block_device() {
        m |= 1 << 26;
    }
    if ft.is_char_device() {
        m |= (1 << 26) | (1 << 21);
    }
    if ft.is_fifo() {
        m |= 1 << 25;
    }
    if ft.is_socket() {
        m |= 1 << 24;
    }
    if raw & 0o4000 != 0 {
        m |= 1 << 23;
    }
    if raw & 0o2000 != 0 {
        m |= 1 << 22;
    }
    if raw & 0o1000 != 0 {
        m |= 1 << 20;
    }
    m
}

pub fn stat_json(path: &str) -> std::io::Result<serde_json::Value> {
    let meta = std::fs::symlink_metadata(path)?;
    let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "/".into());
    let link = if meta.file_type().is_symlink() { std::fs::read_link(path).map(|l| l.to_string_lossy().to_string()).unwrap_or_default() } else { String::new() };
    let mtime = meta.modified().ok().map(chrono::DateTime::<chrono::Utc>::from).map(|t| t.to_rfc3339()).unwrap_or_default();
    Ok(json!({"name": name, "size": meta.len(), "mode": go_mode(&meta), "mtime": mtime, "linkTarget": link}))
}

/// Write a tar of `path` to `out`, named after its last component (or its
/// contents only for a path ending in `/.`).
pub fn write_tar(path: &str, out: impl std::io::Write) -> std::io::Result<()> {
    let mut b = tar::Builder::new(out);
    b.follow_symlinks(false);
    let contents_only = path.ends_with("/.") || path == ".";
    let p = std::path::Path::new(path);
    let meta = std::fs::symlink_metadata(p)?;
    if meta.is_dir() {
        let base = if contents_only { std::path::PathBuf::from(".") } else { p.file_name().map(std::path::PathBuf::from).unwrap_or_else(|| ".".into()) };
        b.append_dir_all(&base, p)?;
    } else {
        let name = p.file_name().map(std::path::PathBuf::from).unwrap_or_else(|| "file".into());
        b.append_path_with_name(p, name)?;
    }
    b.into_inner()?.flush()
}

/// Extract the tar on `input` into the directory `dir`.
pub fn read_tar(dir: &str, input: impl std::io::Read, no_overwrite_dir_non_dir: bool, chown: Option<(u32, u32)>) -> std::io::Result<()> {
    let meta = std::fs::metadata(dir)?;
    if !meta.is_dir() {
        return Err(std::io::Error::new(std::io::ErrorKind::NotADirectory, format!("extraction point is not a directory: {dir}")));
    }
    let mut archive = tar::Archive::new(input);
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(chown.is_none());
    archive.set_overwrite(true);
    archive.set_unpack_xattrs(false);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let rel = entry.path()?.into_owned();
        let target = std::path::Path::new(dir).join(&rel);
        if no_overwrite_dir_non_dir {
            if let Ok(existing) = std::fs::symlink_metadata(&target) {
                let is_dir = entry.header().entry_type().is_dir();
                if existing.is_dir() != is_dir {
                    return Err(std::io::Error::other(format!("cannot overwrite {} with a {}", target.display(), if is_dir { "directory" } else { "non-directory" })));
                }
            }
        }
        entry.unpack_in(dir)?;
        #[cfg(unix)]
        if let Some((uid, gid)) = chown {
            let c = std::ffi::CString::new(target.as_os_str().as_encoded_bytes()).unwrap_or_default();
            // SAFETY: valid string.
            unsafe { libc::lchown(c.as_ptr(), uid, gid) };
        }
    }
    Ok(())
}

pub fn main(args: &[String]) -> ! {
    #[cfg(target_os = "linux")]
    linux::main(args);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = args;
        eprintln!("docker-fs needs Linux");
        std::process::exit(1)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::CString;
    use std::io::Write;
    use std::os::fd::AsRawFd;

    fn die(msg: &str) -> ! {
        eprintln!("{msg}");
        std::process::exit(1)
    }

    fn cstr(s: &str) -> CString {
        CString::new(s).unwrap_or_default()
    }

    fn mount(src: &str, dst: &str, fstype: Option<&str>, flags: libc::c_ulong, data: Option<&str>) -> std::io::Result<()> {
        let (s, d) = (cstr(src), cstr(dst));
        let f = fstype.map(cstr);
        let o = data.map(cstr);
        // SAFETY: valid strings or null.
        let rc = unsafe {
            libc::mount(s.as_ptr(), d.as_ptr(), f.as_ref().map_or(std::ptr::null(), |f| f.as_ptr()), flags, o.as_ref().map_or(std::ptr::null(), |o| o.as_ptr().cast()))
        };
        if rc != 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
    }

    pub fn main(args: &[String]) -> ! {
        let mut pid = None;
        let mut rootfs = None;
        let mut overlay = None;
        let mut binds: Vec<(String, String, bool)> = Vec::new();
        let mut rest = Vec::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--pid" => pid = it.next().cloned(),
                "--rootfs" => rootfs = it.next().cloned(),
                "--overlay" => overlay = it.next().cloned(),
                "--bind" => {
                    let spec = it.next().cloned().unwrap_or_default();
                    let (spec, ro) = match spec.strip_suffix(":ro") {
                        Some(s) => (s.to_string(), true),
                        None => (spec, false),
                    };
                    if let Some((s, d)) = spec.split_once('=') {
                        binds.push((s.to_string(), d.to_string(), ro));
                    }
                }
                _ => rest.push(a.clone()),
            }
        }
        if let Some(pid) = pid {
            let f = std::fs::File::open(format!("/proc/{pid}/ns/mnt")).unwrap_or_else(|e| die(&format!("container is not running: {e}")));
            // SAFETY: single-threaded process; setns to a mount namespace
            // also moves our root and cwd to its root.
            if unsafe { libc::setns(f.as_raw_fd(), libc::CLONE_NEWNS) } != 0 {
                die(&format!("setns: {}", std::io::Error::last_os_error()));
            }
        } else if let Some(root) = rootfs {
            // SAFETY: single-threaded process.
            if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
                die(&format!("unshare: {}", std::io::Error::last_os_error()));
            }
            let _ = mount("none", "/", None, libc::MS_REC | libc::MS_PRIVATE, None);
            if let Some(opts) = &overlay {
                if let Err(e) = mount("overlay", &root, Some("overlay"), 0, Some(opts)) {
                    die(&format!("mounting the container filesystem: {e}"));
                }
            }
            let mut binds = binds;
            binds.sort_by_key(|(_, d, _)| d.split('/').filter(|s| !s.is_empty()).count());
            for (src, dst, ro) in binds {
                let target = format!("{root}/{}", dst.trim_start_matches('/'));
                let is_dir = std::fs::metadata(&src).map(|m| m.is_dir()).unwrap_or(true);
                if is_dir {
                    let _ = std::fs::create_dir_all(&target);
                } else {
                    if let Some(p) = std::path::Path::new(&target).parent() {
                        let _ = std::fs::create_dir_all(p);
                    }
                    let _ = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&target);
                }
                if mount(&src, &target, None, libc::MS_BIND | libc::MS_REC, None).is_ok() && ro {
                    let _ = mount("none", &target, None, libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY, None);
                }
            }
            if let Err(e) = std::os::unix::fs::chroot(&root) {
                die(&format!("chroot: {e}"));
            }
            let _ = std::env::set_current_dir("/");
        } else {
            die("docker-fs: --pid or --rootfs required");
        }
        let op = rest.first().cloned().unwrap_or_default();
        let path = rest.get(1).cloned().unwrap_or_else(|| "/".into());
        let exists = std::fs::symlink_metadata(&path).is_ok() || path.ends_with("/.") && std::fs::metadata(path.trim_end_matches("/.")).is_ok();
        if !exists {
            eprintln!("Could not find the file {path} in container");
            std::process::exit(2);
        }
        let result = match op.as_str() {
            "stat" => super::stat_json(path.trim_end_matches("/.")).map(|v| {
                println!("{v}");
            }),
            "get" => {
                let stdout = std::io::stdout();
                let out = std::io::BufWriter::with_capacity(256 * 1024, stdout.lock());
                super::write_tar(&path, out)
            }
            "put" => {
                let no_overwrite = rest.iter().any(|a| a == "--no-overwrite-dir-non-dir");
                let chown = rest.iter().position(|a| a == "--chown").and_then(|i| rest.get(i + 1)).and_then(|s| {
                    let (u, g) = s.split_once(':')?;
                    Some((u.parse().ok()?, g.parse().ok()?))
                });
                let stdin = std::io::stdin();
                super::read_tar(&path, std::io::BufReader::with_capacity(256 * 1024, stdin.lock()), no_overwrite, chown)
            }
            other => die(&format!("unknown docker-fs operation {other}")),
        };
        let _ = std::io::stdout().flush();
        match result {
            Ok(()) => std::process::exit(0),
            Err(e) => die(&e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tar_round_trip() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("app/sub")).unwrap();
        std::fs::write(src.path().join("app/sub/a.txt"), "hello").unwrap();
        let mut buf = Vec::new();
        write_tar(src.path().join("app").to_str().unwrap(), &mut buf).unwrap();
        let dst = tempfile::tempdir().unwrap();
        read_tar(dst.path().to_str().unwrap(), &buf[..], false, None).unwrap();
        assert_eq!(std::fs::read_to_string(dst.path().join("app/sub/a.txt")).unwrap(), "hello");
        let st = stat_json(src.path().join("app").to_str().unwrap()).unwrap();
        assert_eq!(st["name"], "app");
        assert!(st["mode"].as_u64().unwrap() & (1 << 31) != 0);
        // Contents only.
        let mut buf = Vec::new();
        write_tar(&format!("{}/.", src.path().join("app").display()), &mut buf).unwrap();
        let dst = tempfile::tempdir().unwrap();
        read_tar(dst.path().to_str().unwrap(), &buf[..], false, None).unwrap();
        assert!(dst.path().join("sub/a.txt").exists());
    }
}
