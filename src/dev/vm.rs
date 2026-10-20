//! macOS: the dev cluster in a small Linux VM on Apple's
//! Virtualization.framework.
//!
//! `litek8s dev up` starts `litek8s dev-vm <config>` in the background: one
//! process that owns the VM (the framework runs it in its XPC helper) and
//! makes it feel local. Every local cluster runs in that one VM, each in
//! cells of its own ([`super::cells`]); for each the process keeps
//!
//! - the API server on 127.0.0.1:<port>, forwarded over vsock to the
//!   cluster's cell;
//! - every NodePort, and LoadBalancer ports, on 127.0.0.1 (it watches the
//!   Services through the API; two clusters cannot both have one port);
//! - its Docker API socket and published ports;
//! - the shared directory (default $HOME) mounted in the VM at the same
//!   path, so hostPath volumes name Mac paths;
//! - the console (hvc0) as the control channel: the guest reports its
//!   address there and powers off on `poweroff` (see [`super::guest`]);
//! - the guest's memory ceiling: the balloon keeps the guest on as little
//!   of its memory as it needs, so page cache does not spread it over all
//!   of it (macOS charges every page the VM ever touched; see
//!   [`super::balloon`]).
//!
//! It starts and stops clusters on request ([`super::env`]) and powers the
//! VM off when the last one stops.
//!
//! The kernel, the root disk image and the initrd that mounts it come from
//! dev/vm/build-guest.py. The framework needs the
//! `com.apple.security.virtualization` entitlement; when this binary lacks
//! it, `dev up` runs an ad-hoc signed copy.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::cluster::Cluster;
use super::env::{CellReply, CellRequest, CellStats, EnvState, Request, Response};
use super::{DEFAULT_PORT, DevState, UpOptions, now_ms, write_state};

/// The cell manager's port on the VM's loopback (`super::cells::CELLS_PORT`).
const CELLS_PORT: u16 = 2381;
/// The API port inside a cell (`super::cells::CELL_API_PORT`).
const CELL_API_PORT: u16 = 6443;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VmConfig {
    home: PathBuf,
    kernel: PathBuf,
    initrd: PathBuf,
    /// The read-only root filesystem image (romfs) the initrd mounts;
    /// none: the initrd is the whole root filesystem (older guests).
    #[serde(default)]
    root: Option<PathBuf>,
    disk: PathBuf,
    share: Option<PathBuf>,
    memory_mib: u64,
    cpus: usize,
    mac: String,
    /// More Mac directories shared at the same path: (virtiofs tag, path).
    /// /tmp and /var/folders, as Docker Desktop shares them, and the
    /// settings' shares after the first.
    #[serde(default)]
    extra_shares: Vec<(String, PathBuf)>,
    /// Published ports and NodePorts on every interface, not only localhost.
    #[serde(default)]
    publish_all: bool,
    /// Clusters deleted while the VM was down (their data goes at boot).
    #[serde(default)]
    purge: Vec<String>,
}

const ENTITLEMENTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>com.apple.security.virtualization</key><true/></dict></plist>
"#;

/// The guest kernel and root filesystem: an installation, not cluster data, so
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

fn purge_file(home: &Path) -> PathBuf {
    home.join("vm-state").join("purge")
}

fn pending_purges(home: &Path) -> Vec<String> {
    std::fs::read_to_string(purge_file(home)).unwrap_or_default().lines().filter(|n| super::cluster::validate(n).is_ok()).map(str::to_string).collect()
}

/// Delete a stopped cluster's data in the VM: now if the VM runs, else at
/// its next boot. With no cluster left at all, the whole disk goes.
pub async fn purge(home: &Path, cluster: &Cluster) -> Result<()> {
    if super::env::running(home).is_some() {
        let r = super::env::request(home, &Request::Purge { name: cluster.name.clone() }, Duration::from_secs(60)).await?;
        if let Some(e) = r.error {
            bail!("deleting {}'s data in the VM: {e}", cluster.name);
        }
        return Ok(());
    }
    if super::cluster::list(home).is_empty() {
        let state = home.join("vm-state");
        if state.exists() {
            std::fs::remove_dir_all(&state).with_context(|| format!("failed to delete {}", state.display()))?;
        }
        let _ = std::fs::remove_file(super::env_log_path(home));
        return Ok(());
    }
    let mut list = pending_purges(home);
    if !list.contains(&cluster.name) {
        list.push(cluster.name.clone());
    }
    std::fs::create_dir_all(home.join("vm-state"))?;
    std::fs::write(purge_file(home), list.join("\n"))?;
    Ok(())
}

pub fn start(home: &Path, opts: &UpOptions) -> Result<u32> {
    use std::os::unix::process::CommandExt;
    let assets = assets_dir(home);
    let kernel = assets.join("Image");
    // A root disk image and the small initrd that mounts it, else (an
    // older build) one initramfs with everything.
    let (initrd, root) = match (assets.join("initrd.gz"), assets.join("root.img")) {
        (i, r) if i.is_file() && r.is_file() => (i, Some(r)),
        _ => (assets.join("initramfs.gz"), None),
    };
    if !kernel.is_file() || !initrd.is_file() {
        bail!(
            "the dev VM's kernel and root filesystem are not in {} (build them: dev/vm/build.sh, or dev/vm/build-guest-binary.sh, then dev/vm/build-guest.py --litek8s <binary> --out {})",
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
    let mut shares = opts.shares.iter().map(|s| s.canonicalize().unwrap_or_else(|_| s.clone()));
    let share = shares.next().or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    let user_shares: Vec<(String, PathBuf)> = shares.enumerate().map(|(i, p)| (format!("share{}", i + 1), p)).collect();
    let config = VmConfig {
        home: home.to_path_buf(),
        kernel,
        initrd,
        root,
        disk,
        share: share.map(|s| s.canonicalize().unwrap_or(s)),
        // The most the guest may use; only what it touches costs memory on the
        // Mac. Docker workloads (builds, databases) want more than pods alone.
        memory_mib: opts.memory.unwrap_or(4096),
        cpus: opts.cpus.unwrap_or((ncpu / 2).clamp(1, 8)),
        mac,
        extra_shares: [("tmp", "/private/tmp"), ("folders", "/private/var/folders")]
            .iter()
            .filter(|(_, p)| Path::new(p).is_dir())
            .map(|(t, p)| (t.to_string(), PathBuf::from(p)))
            .chain(user_shares)
            .collect(),
        publish_all: opts.publish == super::config::Publish::All,
        purge: pending_purges(home),
    };
    let config_path = state_dir.join("vm.json");
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config)?)?;
    let runner = signed_runner(&state_dir)?;
    let log = std::fs::File::create(super::env_log_path(home))?;
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
    rusage(state.helper_pid?).map(|info| info.ri_phys_footprint)
}

pub fn footprint_of(helper_pid: u32) -> Option<u64> {
    rusage(helper_pid).map(|info| info.ri_phys_footprint)
}

/// CPU time the VM's helper process has used, in nanoseconds.
pub fn cpu_time_ns(state: &DevState) -> Option<u64> {
    cpu_time_ns_of(state.helper_pid?)
}

pub fn cpu_time_ns_of(helper_pid: u32) -> Option<u64> {
    let info = rusage(helper_pid)?;
    // rusage times are in Mach absolute time units.
    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
    }
    let mut tb = Timebase { numer: 0, denom: 0 };
    // SAFETY: fills the struct we own.
    let rc = unsafe { mach_timebase_info(&mut tb) };
    let (numer, denom) = if rc == 0 && tb.denom != 0 { (tb.numer as u128, tb.denom as u128) } else { (1, 1) };
    Some(((info.ri_user_time + info.ri_system_time) as u128 * numer / denom) as u64)
}

fn rusage(pid: u32) -> Option<libc::rusage_info_v2> {
    // SAFETY: proc_pid_rusage fills the struct we own.
    unsafe {
        let mut info: libc::rusage_info_v2 = std::mem::zeroed();
        let rc = libc::proc_pid_rusage(pid as i32, libc::RUSAGE_INFO_V2, (&mut info as *mut libc::rusage_info_v2).cast());
        (rc == 0).then_some(info)
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
    // Memory macOS would charge for nothing: init_on_alloc (Alpine's
    // default) zeroes every page allocated, so the balloon taking the
    // memory the guest may not use yet would touch all of it; the bounce
    // buffers (64 MiB, zeroed at boot) are for devices that cannot reach
    // all memory, and virtio devices here can. PSI tells the host when
    // the guest needs more memory (see `super::balloon`).
    cmdline.push_str(" init_on_alloc=0 swiotlb=noforce psi=1");
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
    if config.root.is_some() {
        // The second disk (after the data disk).
        cmdline.push_str(" litek8s.root=/dev/vdb");
    }
    if !config.purge.is_empty() {
        cmdline.push_str(&format!(" litek8s.purge={}", config.purge.join(",")));
    }
    if let Ok(extra) = std::env::var("LITEK8S_DEV_VM_CMDLINE") {
        cmdline.push(' ');
        cmdline.push_str(extra.trim());
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
        let mut disks = vec![Retained::into_super(block)];
        if let Some(root) = &config.root {
            let image = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(VZDiskImageStorageDeviceAttachment::alloc(), &url(root), true)
                .map_err(|e| anyhow::anyhow!("root image: {}", e.localizedDescription()))?;
            disks.push(Retained::into_super(VZVirtioBlockDeviceConfiguration::initWithAttachment(VZVirtioBlockDeviceConfiguration::alloc(), &image)));
        }
        vz.setStorageDevices(&NSArray::from_retained_slice(&disks));

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
        rt.block_on(host.run(guest_in_w, guest_out_r));
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

    /// Ask the guest's balloon driver to leave the guest `bytes` of memory
    /// (the VM's size gives it all back). The pages it takes stay as they
    /// were for macOS: never touched (free) or touched (still charged).
    fn set_memory_target(self, bytes: u64) {
        dispatch2::DispatchQueue::main().exec_async(move || {
            let this = self;
            // SAFETY: on the main queue; the VM outlives the run loop.
            unsafe {
                let devices = (*this.0).memoryBalloonDevices();
                if let Some(dev) = devices.iter().next().and_then(|d| d.downcast::<objc2_virtualization::VZVirtioTraditionalMemoryBalloonDevice>().ok()) {
                    dev.setTargetVirtualMachineMemorySize(bytes);
                }
            }
        });
    }

    async fn stream(self) -> Result<tokio::net::UnixStream> {
        let fd = self.connect(super::guest::VSOCK_PORT).await?;
        let std = std::os::unix::net::UnixStream::from(fd);
        std.set_nonblocking(true)?;
        Ok(tokio::net::UnixStream::from_std(std)?)
    }

    /// A stream to 127.0.0.1:`tcp_port` inside the VM, or inside a
    /// cluster's cell.
    async fn dial(self, tcp_port: u16, cell: Option<&str>) -> Result<tokio::net::UnixStream> {
        use tokio::io::AsyncWriteExt;
        let mut stream = self.stream().await?;
        let line = match cell {
            Some(c) => format!("tcp {tcp_port} {c}\n"),
            None => format!("tcp {tcp_port}\n"),
        };
        stream.write_all(line.as_bytes()).await?;
        Ok(stream)
    }

    /// Every cluster's memory and CPU time, as the guest's cgroups count
    /// it, and the guest's meminfo.
    async fn stats(self) -> Result<super::env::GuestStats> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = self.stream().await?;
        stream.write_all(b"stats\n").await?;
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut buf)).await??;
        // A guest from before the meminfo answers with the list alone.
        if let Ok(cells) = serde_json::from_slice::<Vec<CellStats>>(&buf) {
            return Ok(super::env::GuestStats { cells, meminfo: None, proc_stat: None });
        }
        Ok(serde_json::from_slice(&buf)?)
    }

    /// One request to the cell manager in the VM (it may still be starting).
    async fn cells(self, req: &CellRequest) -> Result<CellReply> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let attempt = async {
                let mut stream = self.dial(CELLS_PORT, None).await?;
                let mut line = serde_json::to_vec(req)?;
                line.push(b'\n');
                stream.write_all(&line).await?;
                let mut reply = String::new();
                tokio::io::BufReader::new(stream).read_line(&mut reply).await?;
                if reply.is_empty() {
                    bail!("the cell manager is not answering");
                }
                anyhow::Ok(serde_json::from_str::<CellReply>(&reply)?)
            };
            match attempt.await {
                Ok(r) => return Ok(r),
                Err(e) if std::time::Instant::now() > deadline => return Err(e),
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }
}

enum GuestEvent {
    Vsock,
    Up { ip: String },
    /// The guest's memory (`LITEK8S_MEM ...`).
    Memory(super::balloon::Report),
    Halted,
}

/// The VM's memory ceiling ([`super::balloon`]): what the guest may use
/// now and its last report.
struct Memory {
    controller: Option<super::balloon::Controller>,
    last: Option<super::balloon::Report>,
}

impl Memory {
    /// Whether the balloon's target is the controller's (set once the
    /// guest reports: a guest that does not would never see it grow).
    fn applied(&self) -> bool {
        self.last.is_some()
    }
}

/// A running cluster: its forwarders on the Mac.
struct Running {
    tasks: Vec<tokio::task::JoinHandle<()>>,
    socket: PathBuf,
}

/// What the control requests and the console share.
struct Shared {
    home: PathBuf,
    vm: VmHandle,
    publish: super::config::Publish,
    env: Mutex<EnvState>,
    clusters: Mutex<HashMap<String, Running>>,
    /// Cluster logs the console's `LITEK8S_LOG` lines go to.
    logs: Mutex<HashMap<String, std::fs::File>>,
    /// The last cluster stopped.
    done: tokio::sync::Notify,
    memory: Mutex<Memory>,
}

impl Shared {
    fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.clusters.lock().unwrap().keys().cloned().collect();
        v.sort();
        v
    }

    fn cluster_state(&self, name: &str, port: u16, nodes: u32, socket: &Path) -> DevState {
        let env = self.env.lock().unwrap();
        DevState {
            pid: env.pid,
            port,
            mode: "vm".into(),
            started_unix_ms: now_ms(),
            cgroup: None,
            vm_ip: env.vm_ip.clone(),
            helper_pid: env.helper_pid,
            cpus: env.cpus,
            memory_mib: env.memory_mib,
            shares: env.shares.clone(),
            publish: env.publish.clone(),
            name: Some(name.into()),
            nodes: Some(nodes),
            docker_socket: Some(socket.to_path_buf()),
        }
    }

    async fn start(self: &Arc<Self>, name: &str, port: Option<u16>, nodes: u32) -> Result<u16> {
        let cluster = Cluster::new(name)?;
        if self.clusters.lock().unwrap().contains_key(name) {
            bail!("cluster {name} is running already");
        }
        let chome = cluster.home(&self.home);
        std::fs::create_dir_all(&chome)?;
        let preferred = cluster.meta(&self.home).and_then(|m| m.port).or(cluster.is_default().then_some(DEFAULT_PORT));
        let listener = super::bind_api(port, preferred)?;
        let port = listener.local_addr()?.port();
        let socket = cluster.docker_socket();
        if tokio::net::UnixStream::connect(&socket).await.is_ok() {
            bail!("{} is in use (another litek8s dev?)", socket.display());
        }
        let log = std::fs::File::create(super::log_path(&chome))?;
        self.logs.lock().unwrap().insert(name.into(), log);
        let req = CellRequest::Start { name: name.into(), nodes, data_dir: cluster.guest_data_dir(), docker_alias: socket.to_string_lossy().into() };
        let reply = self.vm.cells(&req).await;
        let failure = match reply {
            Ok(CellReply { error: Some(e) }) => Some(anyhow::anyhow!("{e}")),
            Err(e) => Some(e),
            Ok(_) => None,
        };
        if let Some(e) = failure {
            self.logs.lock().unwrap().remove(name);
            return Err(e);
        }
        let vm = self.vm;
        let cell: Arc<str> = Arc::from(name);
        listener.set_nonblocking(true).ok();
        let tasks = vec![
            tokio::spawn(forward_accepts(listener, vm, Some(cell.clone()), CELL_API_PORT)),
            tokio::spawn(expose_services(vm, cell.clone(), port, self.publish)),
            tokio::spawn(serve_docker_socket(socket.clone(), vm, cell.clone())),
            tokio::spawn(expose_docker_ports(vm, cell.clone(), self.publish)),
            tokio::spawn(forward_fs_events(vm, cell.clone())),
        ];
        write_state(&chome, &self.cluster_state(name, port, nodes, &socket))?;
        self.clusters.lock().unwrap().insert(name.into(), Running { tasks, socket });
        eprintln!("dev-vm: cluster {name} started, API on 127.0.0.1:{port}");
        Ok(port)
    }

    async fn stop(&self, name: &str) -> Result<()> {
        let running = self.clusters.lock().unwrap().remove(name);
        if let Some(r) = &running {
            r.tasks.iter().for_each(|t| t.abort());
        }
        let reply = self.vm.cells(&CellRequest::Stop { name: name.into() }).await;
        if let Ok(c) = Cluster::new(name) {
            let _ = std::fs::remove_file(super::state_path(&c.home(&self.home)));
        }
        if let Some(r) = running {
            let _ = std::fs::remove_file(&r.socket);
        }
        self.logs.lock().unwrap().remove(name);
        eprintln!("dev-vm: cluster {name} stopped");
        match reply? {
            CellReply { error: Some(e) } => bail!("{e}"),
            _ => Ok(()),
        }
    }

    async fn handle(self: Arc<Self>, req: Request) -> Response {
        let result = match req {
            Request::Start { name, port, nodes } => self.start(&name, port, nodes.max(1)).await.map(Some),
            Request::Stop { name } => self.stop(&name).await.map(|_| None),
            Request::Node { name, node, up } => {
                let r = match self.vm.cells(&CellRequest::Node { name, node, up }).await {
                    Ok(CellReply { error: Some(e) }) => Response::error(e),
                    Ok(_) => Response::default(),
                    Err(e) => Response::error(format!("{e:#}")),
                };
                return Response { running: self.names(), ..r };
            }
            Request::Purge { name } => {
                let r = match self.vm.cells(&CellRequest::Purge { name }).await {
                    Ok(CellReply { error: Some(e) }) => Response::error(e),
                    Ok(_) => Response::default(),
                    Err(e) => Response::error(format!("{e:#}")),
                };
                return Response { running: self.names(), ..r };
            }
            Request::Stats => {
                return match self.vm.stats().await {
                    Ok(s) => {
                        let m = self.memory.lock().unwrap();
                        let memory_limit = m.controller.as_ref().map(|c| c.limit());
                        let balloon = m.last.map(|r| r.balloon);
                        drop(m);
                        Response { stats: s.cells, meminfo: s.meminfo, proc_stat: s.proc_stat, memory_limit, balloon, running: self.names(), ..Default::default() }
                    }
                    Err(e) => Response::error(format!("{e:#}")),
                };
            }
            Request::Shutdown => {
                for n in self.names() {
                    let _ = self.stop(&n).await;
                }
                Ok(None)
            }
        };
        let running = self.names();
        if running.is_empty() {
            self.done.notify_one();
        }
        match result {
            Ok(port) => Response { port, running, ..Default::default() },
            Err(e) => Response { running, ..Response::error(format!("{e:#}")) },
        }
    }

    /// A memory report from the guest: move the ceiling if it calls for it.
    fn on_memory(&self, r: &super::balloon::Report) {
        let mut m = self.memory.lock().unwrap();
        let first = !m.applied();
        m.last = Some(*r);
        let Some(c) = m.controller.as_mut() else { return };
        // The first report comes as init starts, before the guest has
        // touched much: the balloon driver then takes the memory above the
        // ceiling without macOS ever charging for it.
        if first && c.limit() < c.vm() {
            self.vm.set_memory_target(c.limit());
            eprintln!("dev-vm: memory ceiling {} of {} MiB (it grows on demand)", c.limit() >> 20, c.vm() >> 20);
            return;
        }
        let before = c.limit();
        c.observe(r);
        if let Some((limit, why)) = c.on_report(r, std::time::Instant::now()) {
            self.vm.set_memory_target(limit);
            let why = match why {
                super::balloon::Why::Pressure => "memory stalls",
                super::balloon::Why::Low => "little available",
                super::balloon::Why::Idle => "idle",
            };
            eprintln!("dev-vm: memory ceiling {} -> {} MiB ({why}; {} MiB available)", before >> 20, limit >> 20, r.available >> 20);
        }
    }

    fn set_ip(&self, ip: Option<String>) {
        let env = {
            let mut env = self.env.lock().unwrap();
            env.vm_ip = ip.clone();
            env.clone()
        };
        let _ = super::env::write_state(&self.home, &env);
        for name in self.names() {
            if let Ok(c) = Cluster::new(&name) {
                let chome = c.home(&self.home);
                if let Some(mut s) = super::read_state(&chome) {
                    s.vm_ip = ip.clone();
                    let _ = write_state(&chome, &s);
                }
            }
        }
    }
}

impl Host {
    async fn run(self, guest_in: OwnedFd, guest_out: OwnedFd) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let control = super::env::control_path(&self.config.home);
        let shared = Arc::new(Shared {
            home: self.config.home.clone(),
            vm: self.vm,
            publish: if self.config.publish_all { super::config::Publish::All } else { super::config::Publish::Localhost },
            env: Mutex::new(EnvState {
                pid: std::process::id(),
                mode: "vm".into(),
                started_unix_ms: self.started,
                control: control.clone(),
                cgroup: None,
                vm_ip: None,
                helper_pid: None,
                cpus: Some(self.config.cpus),
                memory_mib: Some(self.config.memory_mib),
                shares: self.config.share.iter().cloned().chain(self.config.extra_shares.iter().filter(|(t, _)| t.starts_with("share")).map(|(_, p)| p.clone())).collect(),
                publish: Some(if self.config.publish_all { "all" } else { "localhost" }.into()),
            }),
            clusters: Mutex::default(),
            logs: Mutex::default(),
            done: tokio::sync::Notify::new(),
            memory: Mutex::new(Memory { controller: None, last: None }),
        });
        if dynamic_memory() {
            let vm = self.config.memory_mib << 20;
            shared.memory.lock().unwrap().controller = Some(super::balloon::Controller::new(vm, super::balloon::Policy::for_vm(vm), std::time::Instant::now()));
        }
        // The console: clusters' lines to their logs, the rest to ours;
        // pick out the guest's reports.
        let console = shared.clone();
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(std::fs::File::from(guest_out));
            for line in reader.split(b'\n').map_while(Result::ok) {
                let line = String::from_utf8_lossy(&line).trim_end().to_string();
                if let Some(rest) = line.strip_prefix("LITEK8S_LOG ") {
                    let (name, text) = rest.split_once(' ').unwrap_or((rest, ""));
                    if let Some(f) = console.logs.lock().unwrap().get_mut(name) {
                        let _ = writeln!(f, "{text}");
                        continue;
                    }
                }
                if let Some(rest) = line.strip_prefix("LITEK8S_MEM ") {
                    if let Some(r) = super::balloon::Report::parse(rest) {
                        let _ = tx.send(GuestEvent::Memory(r));
                    }
                    continue;
                }
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
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
        let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).expect("signal");
        let mut server: Option<tokio::task::JoinHandle<Result<()>>> = None;
        // Nothing started within a minute (the `dev up` that started us
        // died): do not linger.
        let idle = {
            let shared = shared.clone();
            async move {
                tokio::time::sleep(Duration::from_secs(60)).await;
                while !shared.names().is_empty() {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        };
        tokio::pin!(idle);
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Some(GuestEvent::Vsock) if server.is_none() => {
                        let helper = helper_pids().into_iter().find(|p| !self.before.contains(p));
                        let env = {
                            let mut env = shared.env.lock().unwrap();
                            env.helper_pid = helper;
                            env.clone()
                        };
                        // Purged at boot.
                        let purged = std::fs::read_to_string(purge_file(&self.config.home)).unwrap_or_default();
                        if purged.lines().map(str::to_string).collect::<Vec<_>>() == self.config.purge {
                            let _ = std::fs::remove_file(purge_file(&self.config.home));
                        }
                        let handler = shared.clone();
                        server = Some(tokio::spawn(super::env::serve(control.clone(), move |req| handler.clone().handle(req))));
                        if let Err(e) = super::env::write_state(&self.config.home, &env) {
                            eprintln!("dev-vm: {e:#}");
                        }
                        eprintln!("dev-vm: guest up after {} ms", now_ms() - self.started);
                    }
                    Some(GuestEvent::Vsock) => {}
                    Some(GuestEvent::Up { ip }) => {
                        eprintln!("dev-vm: guest network up at {ip} after {} ms", now_ms() - self.started);
                        shared.set_ip(Some(ip).filter(|ip| !ip.is_empty()));
                    }
                    Some(GuestEvent::Memory(r)) => shared.on_memory(&r),
                    Some(GuestEvent::Halted) | None => break,
                },
                _ = term.recv() => { self.power_off(&shared, &mut console, &mut rx).await; break }
                _ = int.recv() => { self.power_off(&shared, &mut console, &mut rx).await; break }
                _ = shared.done.notified() => { self.power_off(&shared, &mut console, &mut rx).await; break }
                _ = &mut idle => { self.power_off(&shared, &mut console, &mut rx).await; break }
            }
        }
        if let Some(s) = server {
            s.abort();
        }
        let _ = std::fs::remove_file(super::env::state_path(&self.config.home));
        let _ = std::fs::remove_file(&control);
        for name in shared.names() {
            if let Ok(c) = Cluster::new(&name) {
                let _ = std::fs::remove_file(super::state_path(&c.home(&self.config.home)));
                let _ = std::fs::remove_file(c.docker_socket());
            }
        }
        eprintln!("dev-vm: stopped");
    }

    async fn power_off(&self, shared: &Arc<Shared>, console: &mut std::fs::File, rx: &mut tokio::sync::mpsc::UnboundedReceiver<GuestEvent>) {
        // Clusters first, so their API servers stop cleanly.
        for name in shared.names() {
            if let Err(e) = shared.stop(&name).await {
                eprintln!("dev-vm: stopping {name}: {e:#}");
            }
        }
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

/// Whether the VM's memory ceiling moves with demand
/// (`LITEK8S_DEV_DYNAMIC_MEMORY=0` keeps all of it the guest's).
fn dynamic_memory() -> bool {
    !matches!(std::env::var("LITEK8S_DEV_DYNAMIC_MEMORY").as_deref(), Ok("0" | "false" | "off"))
}

/// Relay connections on a localhost listener to `port` inside the VM or a
/// cluster's cell.
async fn forward_accepts(listener: std::net::TcpListener, vm: VmHandle, cell: Option<Arc<str>>, port: u16) {
    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else { return };
    loop {
        let Ok((mut client, _)) = listener.accept().await else { continue };
        let cell = cell.clone();
        tokio::spawn(async move {
            let _ = client.set_nodelay(true);
            match vm.dial(port, cell.as_deref()).await {
                Ok(mut upstream) => {
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                }
                Err(e) => eprintln!("dev-vm: port {port}: {e:#}"),
            }
        });
    }
}

/// A cluster's Docker API on the Mac (`~/.litek8s/docker.sock` for the
/// default one), each connection relayed to the engine in its cell.
async fn serve_docker_socket(path: PathBuf, vm: VmHandle, cell: Arc<str>) {
    use std::os::unix::fs::PermissionsExt;
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
        let cell = cell.clone();
        tokio::spawn(async move {
            match vm.dial(crate::docker::GUEST_PORT, Some(&cell)).await {
                Ok(mut upstream) => {
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                }
                Err(e) => eprintln!("dev-vm: docker: {e:#}"),
            }
        });
    }
}

/// A task that stops when its handle goes (a cluster's forwarders go with
/// the cluster).
struct Task(tokio::task::JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Keep a localhost listener (IPv4 and IPv6) for every port Docker
/// containers publish, following the engine's port list.
async fn expose_docker_ports(vm: VmHandle, cell: Arc<str>, publish: super::config::Publish) {
    use http_body_util::BodyExt;
    let listeners: Arc<Mutex<HashMap<u16, Vec<Task>>>> = Arc::default();
    loop {
        let result = async {
            let stream = vm.dial(crate::docker::GUEST_PORT, Some(&cell)).await?;
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
                    current.retain(|port, _| wanted.contains(port));
                    for port in wanted {
                        if current.contains_key(&port) {
                            continue;
                        }
                        let mut tasks = Vec::new();
                        for addr in [std::net::SocketAddr::from((publish.ipv4(), port)), std::net::SocketAddr::from((publish.ipv6(), port))] {
                            match std::net::TcpListener::bind(addr) {
                                Ok(l) => {
                                    l.set_nonblocking(true).ok();
                                    tasks.push(Task(tokio::spawn(forward_accepts(l, vm, Some(cell.clone()), port))));
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
async fn docker_request(vm: VmHandle, cell: &str, method: hyper::Method, path: &str, body: Vec<u8>) -> Result<hyper::Response<hyper::body::Incoming>> {
    let stream = vm.dial(crate::docker::GUEST_PORT, Some(cell)).await?;
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
async fn forward_fs_events(vm: VmHandle, cell: Arc<str>) {
    use http_body_util::BodyExt;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<(String, u32)>>();
    // Sender of the paths: batches events and posts them to the guest.
    let sender_cell = cell.clone();
    let _sender = Task(tokio::spawn(async move {
        let cell = sender_cell;
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
            if let Err(e) = docker_request(vm, &cell, hyper::Method::POST, "/litek8s/v1/fsevents", body).await {
                eprintln!("dev-vm: file events: {e:#}");
            }
        }
    }));
    loop {
        let result = async {
            let mut body = docker_request(vm, &cell, hyper::Method::GET, "/litek8s/v1/binds?watch=1", Vec::new()).await?.into_body();
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
async fn expose_services(vm: VmHandle, cell: Arc<str>, api_port: u16, publish: super::config::Publish) {
    let api = format!("127.0.0.1:{api_port}");
    let listeners: Arc<Mutex<HashMap<u16, Task>>> = Arc::default();
    loop {
        let list = get(&api, "/api/v1/services").await;
        if let Ok(list) = &list {
            let wanted = wanted_ports(list);
            let mut current = listeners.lock().unwrap();
            current.retain(|port, _| wanted.iter().any(|(h, _)| h == port));
            for (host, node) in wanted {
                if current.contains_key(&host) {
                    continue;
                }
                match std::net::TcpListener::bind((publish.ipv4(), host)) {
                    Ok(l) => {
                        l.set_nonblocking(true).ok();
                        current.insert(host, Task(tokio::spawn(forward_accepts(l, vm, Some(cell.clone()), node))));
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
            root: Some("/r".into()),
            disk: "/d".into(),
            share: Some("/Users/me".into()),
            memory_mib: 2048,
            cpus: 2,
            mac: random_mac(),
            extra_shares: vec![("tmp".into(), "/private/tmp".into()), ("share1".into(), "/Volumes/src".into())],
            publish_all: false,
            purge: vec!["ci".into(), "old".into()],
        };
        let line = kernel_cmdline(&c, true);
        assert!(line.contains(" litek8s.share=/Users/me litek8s.mounts=tmp:/private/tmp,share1:/Volumes/src litek8s.rosetta=1 litek8s.root=/dev/vdb litek8s.purge=ci,old litek8s.seed="), "{line}");
        assert!(line.contains(" init_on_alloc=0 swiotlb=noforce psi=1 "), "{line}");
        assert_eq!(c.mac.len(), 17);
    }
}
