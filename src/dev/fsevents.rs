//! File change events from the Mac into the dev VM.
//!
//! virtiofs shows the guest a Mac file's new content, but the guest kernel
//! never hears about a change made on the Mac, so inotify-based watchers in
//! containers (uvicorn --reload, nodemon, Vite, webpack) miss edits. The
//! Mac side watches the directories running containers bind-mount with
//! FSEvents and tells the guest which paths changed; the guest re-applies
//! each one's mode, which makes its kernel report IN_ATTRIB to every
//! watcher of the file and its directory (what Docker Desktop does too).

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::PathBuf;

use tokio::sync::mpsc::UnboundedSender;

type CFAllocatorRef = *const c_void;
type CFArrayRef = *const c_void;
type CFStringRef = *const c_void;
type FSEventStreamRef = *mut c_void;

#[repr(C)]
struct FSEventStreamContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

#[repr(C)]
struct CFArrayCallBacks {
    _private: [u8; 0],
}

type Callback = extern "C" fn(FSEventStreamRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: CFAllocatorRef,
        callback: Callback,
        context: *mut FSEventStreamContext,
        paths: CFArrayRef,
        since_when: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, queue: *mut c_void);
    fn FSEventStreamStart(stream: FSEventStreamRef) -> u8;
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: CFArrayCallBacks;
    fn CFStringCreateWithCString(alloc: CFAllocatorRef, s: *const c_char, encoding: u32) -> CFStringRef;
    fn CFArrayCreate(alloc: CFAllocatorRef, values: *const *const c_void, count: isize, callbacks: *const CFArrayCallBacks) -> CFArrayRef;
    fn CFRelease(cf: *const c_void);
}

const UTF8: u32 = 0x0800_0100;
const SINCE_NOW: u64 = u64::MAX;
const FLAG_NO_DEFER: u32 = 0x2;
const FLAG_FILE_EVENTS: u32 = 0x10;

/// Event flags that mean the content or the name changed (not only
/// metadata: the guest's own mode refresh shows up as that).
pub const CHANGE_FLAGS: u32 = 0x100 /* created */ | 0x200 /* removed */ | 0x800 /* renamed */ | 0x1000 /* modified */;

extern "C" fn callback(_: FSEventStreamRef, info: *mut c_void, count: usize, paths: *mut c_void, flags: *const u32, _: *const u64) {
    // SAFETY: info is the sender we boxed; paths is a C array of `count`
    // C strings (no kFSEventStreamCreateFlagUseCFTypes), flags parallel.
    unsafe {
        let tx = &*(info as *const UnboundedSender<Vec<(String, u32)>>);
        let paths = paths as *const *const c_char;
        let mut batch = Vec::with_capacity(count);
        for i in 0..count {
            let p = CStr::from_ptr(*paths.add(i)).to_string_lossy().to_string();
            batch.push((p, *flags.add(i)));
        }
        let _ = tx.send(batch);
    }
}

/// An FSEvents stream on some paths; stops when dropped.
pub struct Watcher {
    stream: FSEventStreamRef,
    _queue: dispatch2::DispatchRetained<dispatch2::DispatchQueue>,
    _tx: Box<UnboundedSender<Vec<(String, u32)>>>,
}

// SAFETY: the stream is only touched on creation and drop.
unsafe impl Send for Watcher {}

impl Watcher {
    pub fn new(paths: &[PathBuf], tx: UnboundedSender<Vec<(String, u32)>>) -> Option<Self> {
        if paths.is_empty() {
            return None;
        }
        let tx = Box::new(tx);
        let queue = dispatch2::DispatchQueue::new("dev.litek8s.fsevents", None);
        // SAFETY: CoreFoundation/CoreServices calls with valid arguments; the
        // strings are released after the array retains them.
        unsafe {
            let strings: Vec<CFStringRef> = paths
                .iter()
                .filter_map(|p| CString::new(p.as_os_str().as_encoded_bytes()).ok())
                .map(|c| CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8))
                .collect();
            let array = CFArrayCreate(std::ptr::null(), strings.as_ptr(), strings.len() as isize, &kCFTypeArrayCallBacks);
            for s in &strings {
                CFRelease(*s);
            }
            let mut ctx = FSEventStreamContext {
                version: 0,
                info: (&*tx as *const UnboundedSender<Vec<(String, u32)>>) as *mut c_void,
                retain: std::ptr::null(),
                release: std::ptr::null(),
                copy_description: std::ptr::null(),
            };
            let stream = FSEventStreamCreate(std::ptr::null(), callback, &mut ctx, array, SINCE_NOW, 0.03, FLAG_NO_DEFER | FLAG_FILE_EVENTS);
            CFRelease(array);
            if stream.is_null() {
                return None;
            }
            FSEventStreamSetDispatchQueue(stream, (&*queue as *const dispatch2::DispatchQueue) as *mut c_void);
            if FSEventStreamStart(stream) == 0 {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                return None;
            }
            Some(Self { stream, _queue: queue, _tx: tx })
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // SAFETY: a started stream we own.
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
        }
    }
}

/// Directories to watch for a set of bind sources: existing directories
/// (a file's parent), without ones nested in others.
pub fn roots(sources: &[String]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = sources
        .iter()
        .map(PathBuf::from)
        .filter_map(|p| if p.is_dir() { Some(p) } else { p.parent().map(PathBuf::from).filter(|d| d.is_dir()) })
        .collect();
    dirs.sort();
    dirs.dedup();
    let mut out: Vec<PathBuf> = Vec::new();
    for d in dirs {
        if !out.iter().any(|o| d.starts_with(o)) {
            out.push(d);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_roots_collapse() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(a.join("b")).unwrap();
        std::fs::write(a.join("f"), "x").unwrap();
        let r = roots(&[a.join("b").display().to_string(), a.display().to_string(), a.join("f").display().to_string(), "/nonexistent/x".into()]);
        assert_eq!(r, vec![a]);
    }

    #[test]
    fn reports_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _w = Watcher::new(&[dir.clone()], tx).expect("stream");
        std::thread::sleep(std::time::Duration::from_millis(300));
        std::fs::write(dir.join("changed.txt"), "x").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = false;
        while std::time::Instant::now() < deadline && !seen {
            while let Ok(batch) = rx.try_recv() {
                seen |= batch.iter().any(|(p, f)| p.ends_with("changed.txt") && f & CHANGE_FLAGS != 0);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(seen);
    }
}
