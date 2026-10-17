//! The per-container shim (`litek8s crun-shim`), what conmon is to podman.
//!
//! The agent starts one per container. It forks: the parent stays until the
//! container started (or failed to) and exits with that result, which is all
//! the agent waits for; the child detaches (own session, own cgroup outside
//! the agent's service) and becomes the subreaper of the container's init,
//! so it
//!
//! - creates and starts the container with `crun`,
//! - owns the container's stdout/stderr (pipes, or the pty for `tty: true`)
//!   and writes them to the CRI-format log file,
//! - collects the exit status, checks the cgroup for an OOM kill, deletes
//!   the container from crun, unmounts its root filesystem and writes
//!   `exit.json`, then exits.
//!
//! So containers keep running, and their exit codes are kept, while the
//! agent restarts (or is upgraded). It is a plain single-threaded process
//! (no async runtime) of a few hundred KiB of private memory.

/// `litek8s crun-pause`: the PID 1 of a pod's shared PID namespace
/// (`shareProcessNamespace`), what the pause container is in that mode in
/// Kubernetes. A PID namespace cannot outlive its first process, and the
/// pod's containers come and go, so a process that does nothing but reap
/// orphans holds it. Only pods that share their PID namespace get one.
///
/// It unshares a PID namespace and forks; the child (PID 1 there) detaches
/// and stays; the parent prints the child's PID (as the node sees it) and
/// exits.
pub fn pause_main(args: &[String]) -> ! {
    #[cfg(target_os = "linux")]
    linux::pause(args);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = args;
        eprintln!("crun-pause needs Linux");
        std::process::exit(1)
    }
}

/// Entry point, before any async runtime exists. `args` follow `crun-shim`.
pub fn main(args: &[String]) -> ! {
    #[cfg(target_os = "linux")]
    linux::main(args);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = args;
        eprintln!("crun-shim needs Linux");
        std::process::exit(1)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{BufRead, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    use super::super::logs::{self, LineSplitter};

    const MAX_LOG: u64 = 10 * 1024 * 1024;

    struct Opts {
        id: String,
        bundle: PathBuf,
        crun: String,
        root: String,
        tty: bool,
        stdin: bool,
        console: Option<PathBuf>,
        /// Cgroup directory the shim moves itself into.
        shim_cgroup: Option<PathBuf>,
        /// The container's cgroup directory (for `memory.events`).
        cgroup: Option<PathBuf>,
    }

    fn parse(args: &[String]) -> Opts {
        let mut o = Opts {
            id: String::new(),
            bundle: PathBuf::new(),
            crun: "crun".into(),
            root: "/run/litek8s/crun".into(),
            tty: false,
            stdin: false,
            console: None,
            shim_cgroup: None,
            cgroup: None,
        };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let mut val = || it.next().cloned().unwrap_or_default();
            match a.as_str() {
                "--id" => o.id = val(),
                "--bundle" => o.bundle = val().into(),
                "--crun" => o.crun = val(),
                "--root" => o.root = val(),
                "--tty" => o.tty = true,
                "--stdin" => o.stdin = true,
                "--console" => o.console = Some(val().into()),
                "--shim-cgroup" => o.shim_cgroup = Some(val().into()),
                "--cgroup" => o.cgroup = Some(val().into()),
                _ => {}
            }
        }
        o
    }

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        // SAFETY: fds is a valid 2-element array.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            fail_early("pipe failed");
        }
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    fn fail_early(msg: &str) -> ! {
        eprintln!("crun-shim: {msg}: {}", std::io::Error::last_os_error());
        std::process::exit(1)
    }

    pub fn main(args: &[String]) -> ! {
        let opts = parse(args);
        // Started as /proc/self/exe; give it a name `ps` and top can show.
        // SAFETY: PR_SET_NAME reads a NUL-terminated string.
        unsafe { libc::prctl(libc::PR_SET_NAME, c"litek8s-shim".as_ptr(), 0, 0, 0) };
        if opts.id.is_empty() || opts.bundle.as_os_str().is_empty() {
            eprintln!("crun-shim: --id and --bundle are required");
            std::process::exit(2);
        }
        let (ready_r, ready_w) = pipe();
        // SAFETY: single-threaded here (no runtime started yet).
        match unsafe { libc::fork() } {
            -1 => fail_early("fork failed"),
            0 => {
                drop(ready_r);
                let mut ready = std::fs::File::from(ready_w);
                let code = match run(&opts, &mut ready) {
                    Ok(()) => 0,
                    Err(e) => {
                        let _ = writeln!(ready, "{e}");
                        1
                    }
                };
                std::process::exit(code)
            }
            _ => {
                drop(ready_w);
                // The child keeps its end open while it runs: "ok" comes
                // as a line, an error as everything up to its exit.
                let mut reader = std::io::BufReader::new(std::fs::File::from(ready_r));
                let mut msg = String::new();
                let _ = reader.read_line(&mut msg);
                if msg.starts_with("ok") {
                    std::process::exit(0);
                }
                let _ = reader.read_to_string(&mut msg);
                eprint!("{}", if msg.is_empty() { "the shim exited before the container started\n" } else { &msg });
                std::process::exit(1)
            }
        }
    }

    extern "C" fn exit_now(_: libc::c_int) {
        // SAFETY: async-signal-safe.
        unsafe { libc::_exit(0) }
    }

    extern "C" fn ignore(_: libc::c_int) {}

    pub fn pause(args: &[String]) -> ! {
        let cgroup = args.iter().position(|a| a == "--shim-cgroup").and_then(|i| args.get(i + 1)).map(PathBuf::from);
        // SAFETY: single-threaded process; plain syscalls.
        unsafe {
            libc::prctl(libc::PR_SET_NAME, c"litek8s-pause".as_ptr(), 0, 0, 0);
            if libc::unshare(libc::CLONE_NEWPID) != 0 {
                fail_early("unshare(CLONE_NEWPID) failed");
            }
            match libc::fork() {
                -1 => fail_early("fork failed"),
                0 => {}
                child => {
                    println!("{child}");
                    std::process::exit(0);
                }
            }
            libc::setsid();
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
            for fd in 0..3 {
                libc::dup2(null, fd);
            }
            if null > 2 {
                libc::close(null);
            }
        }
        if let Some(cg) = cgroup {
            let _ = std::fs::create_dir_all(&cg);
            let _ = std::fs::write(cg.join("cgroup.procs"), "0");
        }
        // SAFETY: installing handlers; the loop only sleeps and reaps.
        unsafe {
            libc::signal(libc::SIGTERM, exit_now as libc::sighandler_t);
            libc::signal(libc::SIGINT, exit_now as libc::sighandler_t);
            // A handler (not SIG_IGN) so SIGCHLD wakes pause() up.
            libc::signal(libc::SIGCHLD, ignore as libc::sighandler_t);
            loop {
                while libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) > 0 {}
                libc::pause();
            }
        }
    }

    fn crun(o: &Opts) -> Command {
        let mut c = Command::new(&o.crun);
        let log = o.bundle.join("crun.log");
        c.arg("--root").arg(&o.root).arg("--log").arg(format!("file:{}", log.display()));
        c
    }

    /// The last error crun logged, for the pod's waiting message.
    fn crun_error(o: &Opts) -> String {
        let log = std::fs::read_to_string(o.bundle.join("crun.log")).unwrap_or_default();
        let line = log.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("crun failed").to_string();
        // `level=error msg="..."` or plain text.
        match line.split_once("msg=") {
            Some((_, m)) => m.trim_matches('"').to_string(),
            None => line,
        }
    }

    fn run(o: &Opts, ready: &mut std::fs::File) -> Result<(), String> {
        // SAFETY: detaching this process; no memory arguments.
        unsafe {
            libc::setsid();
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
            for fd in 0..3 {
                libc::dup2(null, fd);
            }
            if null > 2 {
                libc::close(null);
            }
        }
        if let Some(cg) = &o.shim_cgroup {
            let _ = std::fs::create_dir_all(cg);
            let _ = std::fs::write(cg.join("cgroup.procs"), "0");
        }
        // SAFETY: plain prctl/sigprocmask calls on this process.
        let sigfd = unsafe {
            libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGCHLD);
            libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            let fd = libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
            if fd < 0 {
                return Err(format!("signalfd: {}", std::io::Error::last_os_error()));
            }
            OwnedFd::from_raw_fd(fd)
        };
        let _ = std::fs::remove_file(o.bundle.join("crun.log"));

        let mut create = crun(o);
        create.arg("create").arg("--bundle").arg(&o.bundle).arg("--pid-file").arg(o.bundle.join("init.pid"));
        let mut outputs: Vec<(OwnedFd, &'static str)> = Vec::new();
        let mut keep_stdin = None;
        let listener = match (&o.console, o.tty) {
            (Some(path), true) => {
                let _ = std::fs::remove_file(path);
                let l = std::os::unix::net::UnixListener::bind(path).map_err(|e| format!("console socket: {e}"))?;
                create.arg("--console-socket").arg(path);
                create.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
                Some(l)
            }
            _ => {
                let (out_r, out_w) = pipe();
                let (err_r, err_w) = pipe();
                if o.stdin {
                    let (in_r, in_w) = pipe();
                    create.stdin(Stdio::from(in_r));
                    keep_stdin = Some(in_w);
                } else {
                    create.stdin(Stdio::null());
                }
                create.stdout(Stdio::from(out_w)).stderr(Stdio::from(err_w));
                outputs.push((out_r, "stdout"));
                outputs.push((err_r, "stderr"));
                None
            }
        };
        create.arg(&o.id);
        let status = create.status().map_err(|e| format!("failed to run {}: {e}", o.crun))?;
        drop(create);
        if !status.success() {
            return Err(crun_error(o));
        }
        if let Some(l) = listener {
            match super::super::sys::receive_fd(&l, 10_000) {
                Ok(master) => outputs.push((master, "stdout")),
                Err(e) => {
                    let _ = crun(o).arg("delete").arg("-f").arg(&o.id).status();
                    return Err(format!("no terminal from crun: {e}"));
                }
            }
            if let Some(p) = &o.console {
                let _ = std::fs::remove_file(p);
            }
        }
        let start = crun(o).arg("start").arg(&o.id).status().map_err(|e| e.to_string())?;
        if !start.success() {
            let err = crun_error(o);
            let _ = crun(o).arg("delete").arg("-f").arg(&o.id).status();
            return Err(err);
        }
        let init: i32 = std::fs::read_to_string(o.bundle.join("init.pid"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or("crun did not write the container's pid")?;
        let _ = std::fs::write(o.bundle.join("started"), logs::now());
        let _ = std::fs::write(o.bundle.join("shim.pid"), std::process::id().to_string());
        let _ = ready.write_all(b"ok\n");

        let status = pump(o, &sigfd, init, outputs);
        drop(keep_stdin);
        finish(o, status);
        Ok(())
    }

    struct LogFile {
        path: PathBuf,
        file: std::fs::File,
        size: u64,
    }

    impl LogFile {
        fn open(path: PathBuf) -> std::io::Result<Self> {
            let file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
            let size = file.metadata()?.len();
            Ok(Self { path, file, size })
        }

        fn write(&mut self, data: &[u8]) {
            if self.size + data.len() as u64 > MAX_LOG {
                // Rotate like the kubelet (one old file kept).
                let _ = std::fs::rename(&self.path, self.path.with_extension("1"));
                if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
                    self.file = f;
                    self.size = 0;
                }
            }
            if self.file.write_all(data).is_ok() {
                self.size += data.len() as u64;
            }
        }
    }

    /// Copy output to the log until the container's init exited and its
    /// streams closed; returns the wait status.
    fn pump(o: &Opts, sigfd: &OwnedFd, init: i32, outputs: Vec<(OwnedFd, &'static str)>) -> Option<i32> {
        let mut log = match LogFile::open(o.bundle.join("log")) {
            Ok(l) => Some(l),
            Err(_) => None,
        };
        let mut streams: Vec<(Option<OwnedFd>, &'static str, LineSplitter)> =
            outputs.into_iter().map(|(fd, name)| (Some(fd), name, LineSplitter::default())).collect();
        let mut status: Option<i32> = None;
        let mut exited_at: Option<std::time::Instant> = None;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if status.is_some() && streams.iter().all(|s| s.0.is_none()) {
                break;
            }
            if exited_at.is_some_and(|t| t.elapsed() > std::time::Duration::from_secs(2)) {
                break;
            }
            let mut pfds: Vec<libc::pollfd> = vec![libc::pollfd { fd: sigfd.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
            for (fd, _, _) in &streams {
                pfds.push(libc::pollfd { fd: fd.as_ref().map_or(-1, |f| f.as_raw_fd()), events: libc::POLLIN, revents: 0 });
            }
            let timeout = if exited_at.is_some() { 200 } else { -1 };
            // SAFETY: pfds is a valid array of pollfds.
            let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as _, timeout) };
            if n < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if pfds[0].revents != 0 {
                let mut info = [0u8; 128];
                // SAFETY: draining the signalfd into a local buffer.
                while unsafe { libc::read(sigfd.as_raw_fd(), info.as_mut_ptr().cast(), info.len()) } > 0 {}
                loop {
                    let mut st = 0;
                    // SAFETY: reaping children of this process.
                    let pid = unsafe { libc::waitpid(-1, &mut st, libc::WNOHANG) };
                    if pid <= 0 {
                        break;
                    }
                    if pid == init {
                        status = Some(st);
                        exited_at = Some(std::time::Instant::now());
                    }
                }
            }
            for (i, (fd, name, splitter)) in streams.iter_mut().enumerate() {
                let revents = pfds[i + 1].revents;
                if revents == 0 || fd.is_none() {
                    continue;
                }
                let raw = fd.as_ref().unwrap().as_raw_fd();
                // SAFETY: reading into our buffer.
                let r = unsafe { libc::read(raw, buf.as_mut_ptr().cast(), buf.len()) };
                if r > 0 {
                    let mut out = Vec::new();
                    let _ = splitter.feed(&buf[..r as usize], name, &logs::now(), &mut out);
                    if let Some(l) = log.as_mut() {
                        l.write(&out);
                    }
                } else if r == 0 || (r < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted) {
                    // EOF (or EIO from a pty whose other side closed).
                    let mut out = Vec::new();
                    let _ = splitter.finish(name, &logs::now(), &mut out);
                    if let Some(l) = log.as_mut() {
                        l.write(&out);
                    }
                    *fd = None;
                }
            }
        }
        for (fd, name, splitter) in streams.iter_mut() {
            if fd.take().is_some() {
                let mut out = Vec::new();
                let _ = splitter.finish(name, &logs::now(), &mut out);
                if let Some(l) = log.as_mut() {
                    l.write(&out);
                }
            }
        }
        status
    }

    fn finish(o: &Opts, status: Option<i32>) {
        let finished_at = logs::now();
        let oom = o
            .cgroup
            .as_ref()
            .and_then(|cg| std::fs::read_to_string(cg.join("memory.events")).ok())
            .is_some_and(|ev| ev.lines().any(|l| l.strip_prefix("oom_kill ").is_some_and(|n| n.trim() != "0")));
        let _ = crun(o).arg("delete").arg("-f").arg(&o.id).stdout(Stdio::null()).stderr(Stdio::null()).status();
        super::super::sys::unmount(&o.bundle.join("rootfs"));
        for dir in ["upper", "work"] {
            let _ = std::fs::remove_dir_all(o.bundle.join(dir));
        }
        let exit_code = match status {
            Some(st) if libc::WIFEXITED(st) => libc::WEXITSTATUS(st) as i64,
            Some(st) if libc::WIFSIGNALED(st) => 128 + libc::WTERMSIG(st) as i64,
            _ => 255,
        };
        let record = serde_json::json!({"exitCode": exit_code, "finishedAt": finished_at, "oomKilled": oom});
        let path = o.bundle.join("exit.json");
        let tmp = o.bundle.join("exit.json.tmp");
        if std::fs::write(&tmp, record.to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}
