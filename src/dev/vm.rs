//! macOS: the dev cluster in a small Linux VM on Apple's
//! Virtualization.framework.
//!
//! `litek8s dev up` starts `litek8s dev-vm <config>` in the background: one
//! process that owns the VM (the framework runs it in its XPC helper) and
//! makes it feel local:
//!
//! - the API server on 127.0.0.1:<port>, forwarded to the VM's address on
//!   the framework's NAT network;
//! - every NodePort, and LoadBalancer ports, on 127.0.0.1 (it watches the
//!   Services through the API);
//! - the shared directory (default $HOME) mounted in the VM at the same
//!   path, so hostPath volumes name Mac paths;
//! - the console (hvc0) as the control channel: the guest reports its
//!   address there and powers off on `poweroff` (see [`super::guest`]).
//!
//! The kernel and initramfs come from dev/vm/build-guest.py. The framework
//! needs the `com.apple.security.virtualization` entitlement; when this
//! binary lacks it, `dev up` runs an ad-hoc signed copy.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{DEFAULT_PORT, DevState, UpOptions, now_ms, write_state};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VmConfig {
    home: PathBuf,
    kernel: PathBuf,
    initrd: PathBuf,
    disk: PathBuf,
    share: Option<PathBuf>,
    memory_mib: u64,
    cpus: usize,
    port: Option<u16>,
    mac: String,
    /// More Mac directories shared at the same path: (virtiofs tag, path).
    /// /tmp and /var/folders, as Docker Desktop shares them.
    #[serde(default)]
    extra_shares: Vec<(String, PathBuf)>,
}

const ENTITLEMENTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>com.apple.security.virtualization</key><true/></dict></plist>
"#;

/// The guest kernel and initramfs: an installation, not cluster data, so
/// next to the dev data directory rather than in it (`dev reset` keeps them).
fn assets_dir(home: &Path) -> PathBuf {
    std::env::var_os("LITEK8S_DEV_VM_ASSETS")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.parent().unwrap_or(home).join("vm"))
}

fn has_entitlement(path: &Path) -> bool {
    std::process::Command::new("codesign")
        .args(["-d", "--entitlements", ":-"])
        .arg(path)
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("com.apple.security.virtualization"))
}

/// This binary if it may use Virtualization.framework, else an ad-hoc
/// signed copy with the entitlement (re-made when the binary changes).
fn signed_runner(state_dir: &Path) -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    if has_entitlement(&exe) {
        return Ok(exe);
    }
    let copy = state_dir.join("litek8s-vm");
    let fresh = |p: &Path| -> bool {
        let (Ok(a), Ok(b)) = (std::fs::metadata(&exe), std::fs::metadata(p)) else { return false };
        a.len() == b.len() && b.modified().ok() >= a.modified().ok()
    };
    if fresh(&copy) && has_entitlement(&copy) {
        return Ok(copy);
    }
    let tmp = state_dir.join("litek8s-vm.tmp");
    std::fs::copy(&exe, &tmp)?;
    let plist = state_dir.join("entitlements.plist");
    std::fs::write(&plist, ENTITLEMENTS)?;
    let out = std::process::Command::new("codesign")
        .args(["--force", "--sign", "-", "--entitlements"])
        .arg(&plist)
        .arg(&tmp)
        .output()
        .context("failed to run codesign")?;
    if !out.status.success() {
        bail!("codesign failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    std::fs::rename(&tmp, &copy)?;
    Ok(copy)
}

fn random_mac() -> String {
    let mut b = [0u8; 6];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b));
    b[0] = (b[0] & 0xfc) | 0x02; // locally administered, unicast
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(":")
}

pub fn start(home: &Path, opts: &UpOptions) -> Result<u32> {
    use std::os::unix::process::CommandExt;
    let assets = assets_dir(home);
    let (kernel, initrd) = (assets.join("Image"), assets.join("initramfs.gz"));
    if !kernel.is_file() || !initrd.is_file() {
        bail!(
            "the dev VM's kernel and initramfs are not in {} (build them: dev/vm/build-guest-binary.sh, then dev/vm/build-guest.py --litek8s <binary> --out {})",
            assets.display(),
            assets.display()
        );
    }
    let state_dir = home.join("vm-state");
    std::fs::create_dir_all(&state_dir)?;
    let disk = state_dir.join("disk.img");
    if !disk.exists() {
        // Sparse: only what the guest writes takes space.
        std::fs::File::create(&disk)?.set_len(64 << 30)?;
    }
    let mac_file = state_dir.join("mac");
    let mac = std::fs::read_to_string(&mac_file).map(|m| m.trim().to_string()).unwrap_or_else(|_| {
        let m = random_mac();
        let _ = std::fs::write(&mac_file, &m);
        m
    });
    let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    let share = opts.share.clone().or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    let config = VmConfig {
        home: home.to_path_buf(),
        kernel,
        initrd,
        disk,
        share: share.map(|s| s.canonicalize().unwrap_or(s)),
        memory_mib: opts.memory.unwrap_or(2048),
        cpus: opts.cpus.unwrap_or((ncpu / 2).clamp(1, 8)),
        port: opts.port,
        mac,
        extra_shares: [("tmp", "/private/tmp"), ("folders", "/private/var/folders")]
            .iter()
            .filter(|(_, p)| Path::new(p).is_dir())
            .map(|(t, p)| (t.to_string(), PathBuf::from(p)))
            .collect(),
    };
    let config_path = state_dir.join("vm.json");
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config)?)?;
    let runner = signed_runner(&state_dir)?;
    let log = std::fs::File::create(super::log_path(home))?;
    let mut cmd = std::process::Command::new(runner);
    cmd.arg("dev-vm").arg(&config_path);
    cmd.stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // SAFETY: setsid only.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    Ok(cmd.spawn().context("failed to start the dev VM")?.id())
}

/// The VM's memory as macOS accounts it: the physical footprint of the
/// framework's helper process that runs it.
pub fn footprint(state: &DevState) -> Option<u64> {
    let pid = state.helper_pid?;
    // SAFETY: proc_pid_rusage fills the struct we own.
    unsafe {
        let mut info: libc::rusage_info_v2 = std::mem::zeroed();
        let rc = libc::proc_pid_rusage(pid as i32, libc::RUSAGE_INFO_V2, (&mut info as *mut libc::rusage_info_v2).cast());
        (rc == 0).then_some(info.ri_phys_footprint)
    }
}

/// Pids of Virtualization.framework's VM helper processes.
fn helper_pids() -> Vec<u32> {
    let out = std::process::Command::new("ps").args(["-axo", "pid=,comm="]).output();
    let Ok(out) = out else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("com.apple.Virtualization.VirtualMachine"))
        .filter_map(|l| l.split_whitespace().next()?.parse().ok())
        .collect()
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // SAFETY: pipe fills both descriptors.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            eprintln!("dev-vm: pipe failed");
            std::process::exit(1);
        }
        (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
    }
}

fn kernel_cmdline(config: &VmConfig, rosetta: bool) -> String {
    let verbose = std::env::var_os("LITEK8S_DEV_VM_DEBUG").is_some();
    let mut cmdline = String::from(if verbose { "console=hvc0 loglevel=7 printk.time=1 rdinit=/init" } else { "console=hvc0 quiet loglevel=3 rdinit=/init" });
    if let Some(share) = &config.share {
        cmdline.push_str(&format!(" litek8s.share={}", share.display()));
    }
    if !config.extra_shares.is_empty() {
        let list: Vec<String> = config.extra_shares.iter().map(|(t, p)| format!("{t}:{}", p.display())).collect();
        cmdline.push_str(&format!(" litek8s.mounts={}", list.join(",")));
    }
    if rosetta {
        cmdline.push_str(" litek8s.rosetta=1");
    }
    let mut seed = [0u8; 32];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut seed));
    cmdline.push_str(&format!(" litek8s.seed={}", hex::encode(seed)));
    cmdline
}

/// `litek8s dev-vm <config.json>`: run the VM (main thread: the
/// framework's run loop) and its port forwarding (a Tokio thread).
pub fn vm_main(args: &[String]) -> ! {
    match vm_main_inner(args) {
        Ok(never) => match never {},
        Err(e) => {
            eprintln!("dev-vm: {e:#}");
            std::process::exit(1)
        }
    }
}

enum Never {}

fn vm_main_inner(args: &[String]) -> Result<Never> {
    use objc2::AnyThread;
    use objc2::rc::Retained;
    use objc2_foundation::{NSArray, NSFileHandle, NSRunLoop, NSString, NSURL};
    use objc2_virtualization::*;

    let config: VmConfig = serde_json::from_slice(&std::fs::read(args.first().context("usage: dev-vm <config.json>")?)?)?;
    let listener = match std::net::TcpListener::bind(("127.0.0.1", config.port.unwrap_or(DEFAULT_PORT))) {
        Ok(l) => l,
        Err(_) if config.port.is_none() => std::net::TcpListener::bind(("127.0.0.1", 0))?,
        Err(e) => return Err(e).context("failed to listen for the API server"),
    };
    let (guest_in_r, guest_in_w) = pipe();
    let (guest_out_r, guest_out_w) = pipe();
    let before = helper_pids();
    let url = |p: &Path| NSURL::fileURLWithPath(&NSString::from_str(&p.to_string_lossy()));
    let started = now_ms();

    // SAFETY: Virtualization.framework calls on the main thread with valid
    // objects; the file handles own the descriptors (closeOnDealloc).
    let vm = unsafe {
        // Rosetta runs linux/amd64 binaries in the VM (binfmt_misc in the
        // guest points at it), when it is installed on the Mac.
        let rosetta = std::env::var_os("LITEK8S_DEV_NO_ROSETTA").is_none()
            && VZLinuxRosettaDirectoryShare::availability() == VZLinuxRosettaAvailability::Installed;
        let boot = VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &url(&config.kernel));
        boot.setInitialRamdiskURL(Some(&url(&config.initrd)));
        boot.setCommandLine(&NSString::from_str(&kernel_cmdline(&config, rosetta)));

        let vz = VZVirtualMachineConfiguration::new();
        vz.setBootLoader(Some(&boot));
        vz.setPlatform(&VZGenericPlatformConfiguration::new());
        vz.setCPUCount(config.cpus);
        vz.setMemorySize(config.memory_mib << 20);

        let disk = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(VZDiskImageStorageDeviceAttachment::alloc(), &url(&config.disk), false)
            .map_err(|e| anyhow::anyhow!("disk: {}", e.localizedDescription()))?;
        let block = VZVirtioBlockDeviceConfiguration::initWithAttachment(VZVirtioBlockDeviceConfiguration::alloc(), &disk);
        vz.setStorageDevices(&NSArray::from_retained_slice(&[Retained::into_super(block)]));

        let net = VZVirtioNetworkDeviceConfiguration::new();
        net.setAttachment(Some(&VZNATNetworkDeviceAttachment::new()));
        if let Some(mac) = VZMACAddress::initWithString(VZMACAddress::alloc(), &NSString::from_str(&config.mac)) {
            net.setMACAddress(&mac);
        }
        vz.setNetworkDevices(&NSArray::from_retained_slice(&[Retained::into_super(net)]));

        let read = NSFileHandle::initWithFileDescriptor_closeOnDealloc(NSFileHandle::alloc(), std::os::fd::IntoRawFd::into_raw_fd(guest_in_r), true);
        let write = NSFileHandle::initWithFileDescriptor_closeOnDealloc(NSFileHandle::alloc(), std::os::fd::IntoRawFd::into_raw_fd(guest_out_w), true);
        let attachment = VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
            VZFileHandleSerialPortAttachment::alloc(),
            Some(&read),
            Some(&write),
        );
        let console = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        console.setAttachment(Some(&attachment));
        vz.setSerialPorts(&NSArray::from_retained_slice(&[Retained::into_super(console)]));

        vz.setEntropyDevices(&NSArray::from_retained_slice(&[Retained::into_super(VZVirtioEntropyDeviceConfiguration::new())]));
        vz.setMemoryBalloonDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new(),
        )]));
        let mut shares: Vec<Retained<VZDirectorySharingDeviceConfiguration>> = Vec::new();
        let dirs = config.share.iter().map(|s| ("share".to_string(), s.clone())).chain(config.extra_shares.iter().cloned());
        for (tag, path) in dirs {
            let dir = VZSharedDirectory::initWithURL_readOnly(VZSharedDirectory::alloc(), &url(&path), false);
            let single = VZSingleDirectoryShare::initWithDirectory(VZSingleDirectoryShare::alloc(), &dir);
            let fs = VZVirtioFileSystemDeviceConfiguration::initWithTag(VZVirtioFileSystemDeviceConfiguration::alloc(), &NSString::from_str(&tag));
            fs.setShare(Some(&single));
            shares.push(Retained::into_super(fs));
        }
        if rosetta {
            match VZLinuxRosettaDirectoryShare::initWithError(VZLinuxRosettaDirectoryShare::alloc()) {
                Ok(r) => {
                    let fs = VZVirtioFileSystemDeviceConfiguration::initWithTag(VZVirtioFileSystemDeviceConfiguration::alloc(), &NSString::from_str("rosetta"));
                    fs.setShare(Some(&r));
                    shares.push(Retained::into_super(fs));
                }
                Err(e) => eprintln!("dev-vm: no Rosetta: {}", e.localizedDescription()),
            }
        }
        if !shares.is_empty() {
            vz.setDirectorySharingDevices(&NSArray::from_retained_slice(&shares));
        }
        vz.setSocketDevices(&NSArray::from_retained_slice(&[Retained::into_super(VZVirtioSocketDeviceConfiguration::new())]));
        vz.validateWithError().map_err(|e| anyhow::anyhow!("invalid VM configuration: {}", e.localizedDescription()))?;
        let vm = VZVirtualMachine::initWithConfiguration(VZVirtualMachine::alloc(), &vz);
        let done = block2::RcBlock::new(|err: *mut objc2_foundation::NSError| {
            if let Some(err) = err.as_ref() {
                eprintln!("dev-vm: the VM failed to start: {}", err.localizedDescription());
                std::process::exit(1);
            }
        });
        vm.startWithCompletionHandler(&done);
        vm
    };
    eprintln!("dev-vm: starting the VM ({} CPUs, {} MiB)", config.cpus, config.memory_mib);

    let host = Host { config: config.clone(), started, before, vm: VmHandle(Retained::as_ptr(&vm)) };
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
        rt.block_on(host.run(listener, guest_in_w, guest_out_r));
        std::process::exit(0);
    });
    // The VM lives as long as this process; the run loop delivers its
    // completion handlers.
    let _keep = vm;
    loop {
        NSRunLoop::mainRunLoop().run();
    }
}

struct Host {
    config: VmConfig,
    started: u64,
    before: Vec<u32>,
    vm: VmHandle,
}

/// The VM object, for the calls the main queue makes on it (it lives as
/// long as the process).
#[derive(Clone, Copy)]
struct VmHandle(*const objc2_virtualization::VZVirtualMachine);
// SAFETY: only dereferenced on the main queue, which owns the VM.
unsafe impl Send for VmHandle {}
unsafe impl Sync for VmHandle {}

impl VmHandle {
    /// A connection to the guest's vsock `port` (made on the main queue,
    /// as the framework requires).
    async fn connect(self, port: u32) -> Result<OwnedFd> {
        let (tx, rx) = tokio::sync::oneshot::channel::<std::result::Result<OwnedFd, String>>();
        let tx = Mutex::new(Some(tx));
        dispatch2::DispatchQueue::main().exec_async(move || {
            let this = self;
            let reply = move |r| {
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(r);
                }
            };
            // SAFETY: on the main queue; the VM outlives the run loop.
            unsafe {
                let devices = (*this.0).socketDevices();
                let Some(dev) = devices.iter().next().and_then(|d| d.downcast::<objc2_virtualization::VZVirtioSocketDevice>().ok()) else {
                    return reply(Err("the VM has no vsock device".into()));
                };
                let reply = std::cell::RefCell::new(Some(reply));
                let done = block2::RcBlock::new(move |conn: *mut objc2_virtualization::VZVirtioSocketConnection, err: *mut objc2_foundation::NSError| {
                    let Some(reply) = reply.borrow_mut().take() else { return };
                    match conn.as_ref() {
                        Some(conn) => {
                            let fd = libc::dup(conn.fileDescriptor());
                            reply(if fd >= 0 { Ok(OwnedFd::from_raw_fd(fd)) } else { Err("dup failed".into()) });
                        }
                        None => reply(Err(err.as_ref().map(|e| e.localizedDescription().to_string()).unwrap_or_default())),
                    }
                });
                dev.connectToPort_completionHandler(port, &done);
            }
        });
        rx.await?.map_err(|e| anyhow::anyhow!("vsock connect: {e}"))
    }

    /// A stream to 127.0.0.1:`tcp_port` inside the VM.
    async fn dial(self, tcp_port: u16) -> Result<tokio::net::UnixStream> {
        use tokio::io::AsyncWriteExt;
        let fd = self.connect(super::guest::VSOCK_PORT).await?;
        let std = std::os::unix::net::UnixStream::from(fd);
        std.set_nonblocking(true)?;
        let mut stream = tokio::net::UnixStream::from_std(std)?;
        stream.write_all(format!("tcp {tcp_port}\n").as_bytes()).await?;
        Ok(stream)
    }
}

enum GuestEvent {
    Vsock,
    Up { ip: String },
    Halted,
}

impl Host {
    async fn run(self, listener: std::net::TcpListener, guest_in: OwnedFd, guest_out: OwnedFd) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // The console: copy it to the log, pick out the guest's reports.
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(std::fs::File::from(guest_out));
            for line in reader.split(b'\n').map_while(Result::ok) {
                let line = String::from_utf8_lossy(&line).trim_end().to_string();
                eprintln!("vm: {line}");
                if let Some(rest) = line.strip_prefix("LITEK8S_GUEST ") {
                    if rest.starts_with("vsock=") {
                        let _ = tx.send(GuestEvent::Vsock);
                        continue;
                    }
                    let ip = rest.split_whitespace().find_map(|w| w.strip_prefix("ip=")).unwrap_or_default().to_string();
                    let _ = tx.send(GuestEvent::Up { ip });
                } else if line.contains("litek8s-guest: halted") || line.contains("reboot: Power down") {
                    let _ = tx.send(GuestEvent::Halted);
                }
            }
            let _ = tx.send(GuestEvent::Halted);
        });
        let mut console = std::fs::File::from(guest_in);
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(DEFAULT_PORT);
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
        let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).expect("signal");
        let listener = tokio::net::TcpListener::from_std({
            listener.set_nonblocking(true).ok();
            listener
        })
        .expect("listener");
        let mut forwarders: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        let mut state = DevState {
            pid: std::process::id(),
            port,
            mode: "vm".into(),
            started_unix_ms: self.started,
            cgroup: None,
            vm_ip: None,
            helper_pid: None,
        };
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Some(GuestEvent::Vsock) if forwarders.is_empty() => {
                        state.helper_pid = helper_pids().into_iter().find(|p| !self.before.contains(p));
                        if let Err(e) = write_state(&self.config.home, &state) {
                            eprintln!("dev-vm: {e:#}");
                        }
                        eprintln!("dev-vm: guest up after {} ms; API on 127.0.0.1:{port}", now_ms() - self.started);
                        forwarders.push(tokio::spawn(forward_accepts(listener_take(&listener), self.vm, super::guest::GUEST_API_PORT)));
                        forwarders.push(tokio::spawn(expose_services(self.vm, port)));
                        forwarders.push(tokio::spawn(serve_docker_socket(self.vm)));
                        forwarders.push(tokio::spawn(expose_docker_ports(self.vm)));
                        forwarders.push(tokio::spawn(forward_fs_events(self.vm)));
                    }
                    Some(GuestEvent::Vsock) => {}
                    Some(GuestEvent::Up { ip }) => {
                        eprintln!("dev-vm: guest network up at {ip} after {} ms", now_ms() - self.started);
                        state.vm_ip = Some(ip).filter(|ip| !ip.is_empty());
                        if !forwarders.is_empty() {
                            let _ = write_state(&self.config.home, &state);
                        }
                    }
                    Some(GuestEvent::Halted) | None => break,
                },
                _ = term.recv() => { self.power_off(&mut console, &mut rx).await; break }
                _ = int.recv() => { self.power_off(&mut console, &mut rx).await; break }
            }
        }
        let _ = std::fs::remove_file(super::state_path(&self.config.home));
        let _ = std::fs::remove_file(crate::docker::host_socket_path());
        eprintln!("dev-vm: stopped");
    }

    async fn power_off(&self, console: &mut std::fs::File, rx: &mut tokio::sync::mpsc::UnboundedReceiver<GuestEvent>) {
        eprintln!("dev-vm: stopping the VM");
        let _ = console.write_all(b"poweroff\n");
        let _ = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(ev) = rx.recv().await {
                if matches!(ev, GuestEvent::Halted) {
                    break;
                }
            }
        })
        .await;
    }
}

/// A second handle to the API listener for the forwarding task.
fn listener_take(listener: &tokio::net::TcpListener) -> std::net::TcpListener {
    use std::os::fd::{AsFd, AsRawFd};
    // SAFETY: dup of a live listening socket.
    let fd = unsafe { libc::dup(listener.as_fd().as_raw_fd()) };
    // SAFETY: we own the duplicate.
    let l = unsafe { std::net::TcpListener::from_raw_fd(fd) };
    l.set_nonblocking(true).ok();
    l
}

/// Relay connections on a localhost listener to `port` inside the VM.
async fn forward_accepts(listener: std::net::TcpListener, vm: VmHandle, port: u16) {
    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else { return };
    loop {
        let Ok((mut client, _)) = listener.accept().await else { continue };
        tokio::spawn(async move {
            let _ = client.set_nodelay(true);
            match vm.dial(port).await {
                Ok(mut upstream) => {
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                }
                Err(e) => eprintln!("dev-vm: port {port}: {e:#}"),
            }
        });
    }
}

/// The Docker API on the Mac: `~/.litek8s/docker.sock`, each connection
/// relayed to the engine on the VM's loopback.
async fn serve_docker_socket(vm: VmHandle) {
    use std::os::unix::fs::PermissionsExt;
    let path = crate::docker::host_socket_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if tokio::net::UnixStream::connect(&path).await.is_ok() {
        eprintln!("dev-vm: {} is in use; Docker API not exposed", path.display());
        return;
    }
    let _ = std::fs::remove_file(&path);
    let listener = match tokio::net::UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("dev-vm: cannot listen on {}: {e}", path.display());
            return;
        }
    };
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    eprintln!("dev-vm: Docker API on unix://{}", path.display());
    loop {
        let Ok((mut client, _)) = listener.accept().await else { continue };
        tokio::spawn(async move {
            match vm.dial(crate::docker::GUEST_PORT).await {
                Ok(mut upstream) => {
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                }
                Err(e) => eprintln!("dev-vm: docker: {e:#}"),
            }
        });
    }
}

/// Keep a localhost listener (IPv4 and IPv6) for every port Docker
/// containers publish, following the engine's port list.
async fn expose_docker_ports(vm: VmHandle) {
    use http_body_util::BodyExt;
    let listeners: Arc<Mutex<HashMap<u16, Vec<tokio::task::JoinHandle<()>>>>> = Arc::default();
    loop {
        let result = async {
            let stream = vm.dial(crate::docker::GUEST_PORT).await?;
            let (mut sender, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            let req = hyper::Request::builder()
                .uri("/litek8s/v1/ports?watch=1")
                .header(hyper::header::HOST, "docker")
                .body(http_body_util::Empty::<hyper::body::Bytes>::new())?;
            let mut body = sender.send_request(req).await?.into_body();
            let mut buf = Vec::new();
            while let Some(frame) = body.frame().await {
                let Ok(data) = frame?.into_data() else { continue };
                buf.extend_from_slice(&data);
                while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let Ok(wanted) = serde_json::from_slice::<Vec<u16>>(&line) else { continue };
                    let mut current = listeners.lock().unwrap();
                    current.retain(|port, tasks| {
                        let keep = wanted.contains(port);
                        if !keep {
                            tasks.iter().for_each(|t| t.abort());
                        }
                        keep
                    });
                    for port in wanted {
                        if current.contains_key(&port) {
                            continue;
                        }
                        let mut tasks = Vec::new();
                        for addr in [std::net::SocketAddr::from(([127, 0, 0, 1], port)), std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port))] {
                            match std::net::TcpListener::bind(addr) {
                                Ok(l) => {
                                    l.set_nonblocking(true).ok();
                                    tasks.push(tokio::spawn(forward_accepts(l, vm, port)));
                                }
                                Err(e) if addr.is_ipv4() => eprintln!("dev-vm: localhost:{port} not exposed: {e}"),
                                Err(_) => {}
                            }
                        }
                        current.insert(port, tasks);
                    }
                }
            }
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = result {
            if !format!("{e:#}").contains("vsock") {
                eprintln!("dev-vm: docker ports: {e:#}");
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// One request to the Docker engine in the VM; the response body.
async fn docker_request(vm: VmHandle, method: hyper::Method, path: &str, body: Vec<u8>) -> Result<hyper::Response<hyper::body::Incoming>> {
    let stream = vm.dial(crate::docker::GUEST_PORT).await?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let req = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header(hyper::header::HOST, "docker")
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(http_body_util::Full::new(hyper::body::Bytes::from(body)))?;
    Ok(sender.send_request(req).await?)
}

/// Mac file changes under the directories running containers bind-mount,
/// passed to the guest (see [`super::fsevents`]).
async fn forward_fs_events(vm: VmHandle) {
    use http_body_util::BodyExt;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<(String, u32)>>();
    // Sender of the paths: batches events and posts them to the guest.
    tokio::spawn(async move {
        let mut recent: HashMap<String, std::time::Instant> = HashMap::new();
        while let Some(batch) = rx.recv().await {
            let mut paths: Vec<String> = batch.into_iter().filter(|(_, f)| f & super::fsevents::CHANGE_FLAGS != 0).map(|(p, _)| p).collect();
            // More of the same burst.
            tokio::time::sleep(Duration::from_millis(20)).await;
            while let Ok(more) = rx.try_recv() {
                paths.extend(more.into_iter().filter(|(_, f)| f & super::fsevents::CHANGE_FLAGS != 0).map(|(p, _)| p));
            }
            let now = std::time::Instant::now();
            recent.retain(|_, t| now.duration_since(*t) < Duration::from_millis(300));
            paths.sort();
            paths.dedup();
            paths.retain(|p| recent.insert(p.clone(), now).is_none());
            if paths.is_empty() {
                continue;
            }
            let body = serde_json::to_vec(&paths).unwrap_or_default();
            if let Err(e) = docker_request(vm, hyper::Method::POST, "/litek8s/v1/fsevents", body).await {
                eprintln!("dev-vm: file events: {e:#}");
            }
        }
    });
    loop {
        let result = async {
            let mut body = docker_request(vm, hyper::Method::GET, "/litek8s/v1/binds?watch=1", Vec::new()).await?.into_body();
            let mut watcher: Option<super::fsevents::Watcher> = None;
            let mut buf = Vec::new();
            while let Some(frame) = body.frame().await {
                let Ok(data) = frame?.into_data() else { continue };
                buf.extend_from_slice(&data);
                while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let Ok(sources) = serde_json::from_slice::<Vec<String>>(&line) else { continue };
                    let roots = super::fsevents::roots(&sources);
                    watcher = super::fsevents::Watcher::new(&roots, tx.clone());
                }
            }
            drop(watcher);
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = result {
            if !format!("{e:#}").contains("vsock") {
                eprintln!("dev-vm: file events: {e:#}");
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// (host port, node port) for every NodePort and LoadBalancer port.
fn wanted_ports(services: &Value) -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    for svc in services["items"].as_array().into_iter().flatten() {
        let kind = svc.pointer("/spec/type").and_then(Value::as_str).unwrap_or_default();
        if !matches!(kind, "NodePort" | "LoadBalancer") {
            continue;
        }
        for p in svc.pointer("/spec/ports").and_then(Value::as_array).into_iter().flatten() {
            if !matches!(p["protocol"].as_str(), None | Some("TCP")) {
                continue;
            }
            let Some(np) = p["nodePort"].as_u64() else { continue };
            out.push((np as u16, np as u16));
            if kind == "LoadBalancer" {
                if let Some(port) = p["port"].as_u64().filter(|port| *port != np) {
                    out.push((port as u16, np as u16));
                }
            }
        }
    }
    out.sort();
    out.dedup_by_key(|(h, _)| *h);
    out
}

/// Keep a localhost listener for every NodePort / LoadBalancer port,
/// following the Services through a watch.
async fn expose_services(vm: VmHandle, api_port: u16) {
    let api = format!("127.0.0.1:{api_port}");
    let listeners: Arc<Mutex<HashMap<u16, tokio::task::JoinHandle<()>>>> = Arc::default();
    loop {
        let list = get(&api, "/api/v1/services").await;
        if let Ok(list) = &list {
            let wanted = wanted_ports(list);
            let mut current = listeners.lock().unwrap();
            current.retain(|port, task| {
                let keep = wanted.iter().any(|(h, _)| h == port);
                if !keep {
                    task.abort();
                }
                keep
            });
            for (host, node) in wanted {
                if current.contains_key(&host) {
                    continue;
                }
                match std::net::TcpListener::bind(("127.0.0.1", host)) {
                    Ok(l) => {
                        l.set_nonblocking(true).ok();
                        current.insert(host, tokio::spawn(forward_accepts(l, vm, node)));
                    }
                    Err(e) => eprintln!("dev-vm: localhost:{host} not exposed: {e}"),
                }
            }
        }
        let rv = list.as_ref().ok().and_then(|l| l.pointer("/metadata/resourceVersion")).and_then(Value::as_str).unwrap_or("").to_string();
        // Block until a Service changes (or the watch ends), then re-list.
        if wait_for_change(&api, &rv).await.is_err() {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn request(api: &str, path: &str) -> Result<hyper::Response<hyper::body::Incoming>> {
    let tcp = tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(api)).await??;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(tcp)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let req = hyper::Request::builder().uri(path).header(hyper::header::HOST, api).body(http_body_util::Empty::<hyper::body::Bytes>::new())?;
    Ok(sender.send_request(req).await?)
}

async fn get(api: &str, path: &str) -> Result<Value> {
    use http_body_util::BodyExt;
    let body = request(api, path).await?.into_body().collect().await?.to_bytes();
    Ok(serde_json::from_slice(&body)?)
}

async fn wait_for_change(api: &str, rv: &str) -> Result<()> {
    use http_body_util::BodyExt;
    let path = format!("/api/v1/services?watch=true&allowWatchBookmarks=false&resourceVersion={rv}");
    let mut body = request(api, &path).await?.into_body();
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame?.into_data() {
            if !data.is_empty() {
                return Ok(());
            }
        }
    }
    bail!("watch ended")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_node_and_load_balancer_ports() {
        let list = serde_json::json!({"items": [
            {"spec": {"type": "ClusterIP", "ports": [{"port": 80}]}},
            {"spec": {"type": "NodePort", "ports": [{"port": 80, "nodePort": 30080, "protocol": "TCP"}, {"port": 53, "nodePort": 30053, "protocol": "UDP"}]}},
            {"spec": {"type": "LoadBalancer", "ports": [{"port": 8080, "nodePort": 31000}]}},
        ]});
        assert_eq!(wanted_ports(&list), vec![(8080, 31000), (30080, 30080), (31000, 31000)]);
    }

    #[test]
    fn kernel_command_line_carries_the_share() {
        let c = VmConfig {
            home: "/h".into(),
            kernel: "/k".into(),
            initrd: "/i".into(),
            disk: "/d".into(),
            share: Some("/Users/me".into()),
            memory_mib: 2048,
            cpus: 2,
            port: None,
            mac: random_mac(),
            extra_shares: vec![("tmp".into(), "/private/tmp".into())],
        };
        let line = kernel_cmdline(&c, true);
        assert!(line.contains(" litek8s.share=/Users/me litek8s.mounts=tmp:/private/tmp litek8s.rosetta=1 litek8s.seed="), "{line}");
        assert_eq!(c.mac.len(), 17);
    }
}
