//! Images without a daemon: pull from OCI/Docker registries for this host's
//! platform into a content-addressed store, and unpack every layer once into
//! its own directory in overlayfs format, so containers share them as
//! lower directories.
//!
//! Ported from litecontainer's `lc` (registry, store, unpack) with three
//! differences: the platform is the host's (lc pins linux/amd64), the HTTP
//! client is hyper + rustls (already in litek8s, instead of oci-client's
//! reqwest stack), and layers are unpacked one directory each with
//! whiteouts turned into overlayfs whiteouts instead of being merged into a
//! single rootfs.
//!
//! Layout under the store root:
//!
//! ```text
//! blobs/sha256/<hex>   image manifests and configs (layers are not kept
//!                      compressed: they are verified while downloading and
//!                      deleted once unpacked)
//! l/<diff id[..16]>/   one unpacked layer
//! refs.json            image reference -> manifest, config, layers
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use hyper::body::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::spec::ImageConfig;

const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.docker.distribution.manifest.v2+json";
const MAX_CONCURRENT_LAYERS: usize = 3;
const USER_AGENT: &str = concat!("litek8s/", env!("CARGO_PKG_VERSION"));

// ---------------------------------------------------------------------------
// References

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Reference {
    /// As in image names (`docker.io`, `ghcr.io`, `localhost:5000`).
    pub registry: String,
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

impl Reference {
    pub fn parse(image: &str) -> Result<Self> {
        let image = image.trim();
        if image.is_empty() || image.contains(char::is_whitespace) {
            bail!("invalid image reference \"{image}\"");
        }
        let (rest, digest) = match image.split_once('@') {
            Some((r, d)) => (r, Some(d.to_string())),
            None => (image, None),
        };
        let (registry, path) = match rest.split_once('/') {
            Some((first, path)) if first.contains('.') || first.contains(':') || first == "localhost" => {
                (first.to_string(), path.to_string())
            }
            _ => ("docker.io".to_string(), rest.to_string()),
        };
        let (repository, tag) = match path.rsplit_once(':') {
            Some((repo, tag)) if !tag.contains('/') => (repo.to_string(), Some(tag.to_string())),
            _ => (path, None),
        };
        let registry = if registry == "index.docker.io" { "docker.io".to_string() } else { registry };
        let repository = if registry == "docker.io" && !repository.contains('/') {
            format!("library/{repository}")
        } else {
            repository
        };
        if repository.is_empty() || repository.chars().any(|c| c.is_ascii_uppercase()) {
            bail!("invalid image reference \"{image}\"");
        }
        if let Some(d) = &digest {
            if !d.starts_with("sha256:") || d.len() != 71 {
                bail!("invalid image digest in \"{image}\"");
            }
        }
        let tag = if tag.is_none() && digest.is_none() { Some("latest".to_string()) } else { tag };
        Ok(Self { registry, repository, tag, digest })
    }

    /// The fully qualified name the store indexes images under.
    pub fn canonical(&self) -> String {
        let mut s = format!("{}/{}", self.registry, self.repository);
        if let Some(t) = &self.tag {
            s.push(':');
            s.push_str(t);
        }
        if let Some(d) = &self.digest {
            s.push('@');
            s.push_str(d);
        }
        s
    }

    fn api_host(&self) -> &str {
        if self.registry == "docker.io" { "registry-1.docker.io" } else { &self.registry }
    }

    fn scheme(&self) -> &'static str {
        let host = self.registry.split(':').next().unwrap_or_default();
        let insecure = std::env::var("LITEK8S_INSECURE_REGISTRIES").unwrap_or_default();
        if host == "localhost" || host == "127.0.0.1" || insecure.split(',').any(|r| r.trim() == self.registry) {
            "http"
        } else {
            "https"
        }
    }

    fn manifest_ref(&self) -> &str {
        self.digest.as_deref().or(self.tag.as_deref()).unwrap_or("latest")
    }
}

/// This host's platform as image indexes name it.
pub(crate) fn host_platform() -> (&'static str, Option<&'static str>) {
    match std::env::consts::ARCH {
        "aarch64" => ("arm64", Some("v8")),
        "x86_64" => ("amd64", None),
        "arm" => ("arm", Some("v7")),
        "riscv64" => ("riscv64", None),
        "powerpc64" => ("ppc64le", None),
        "s390x" => ("s390x", None),
        other => (other, None),
    }
}

/// Pick the manifest for `arch` (and preferably `variant`) from an index.
pub(crate) fn select_platform<'a>(index: &'a Value, arch: &str, variant: Option<&str>) -> Option<&'a Value> {
    let candidates: Vec<&Value> = index
        .get("manifests")?
        .as_array()?
        .iter()
        .filter(|m| {
            let p = &m["platform"];
            p["os"].as_str() == Some("linux") && p["architecture"].as_str() == Some(arch)
        })
        .collect();
    candidates
        .iter()
        .find(|m| m["platform"]["variant"].as_str() == variant)
        .or_else(|| candidates.iter().find(|m| m["platform"]["variant"].as_str().is_none()))
        .or_else(|| candidates.first())
        .copied()
}

// ---------------------------------------------------------------------------
// The store

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct IndexEntry {
    /// The digest the reference resolved to (an index for multi-arch images).
    #[serde(default)]
    top: String,
    manifest: String,
    config: String,
    /// Layer diff ids, bottom first.
    layers: Vec<String>,
    #[serde(default)]
    last_used: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    images: BTreeMap<String, IndexEntry>,
}

#[derive(Debug, Clone)]
pub(crate) struct Image {
    /// `registry/repository@manifest-digest`, what Kubernetes shows as
    /// `imageID`.
    pub id: String,
    /// Layer directories, bottom first.
    pub layers: Vec<PathBuf>,
    pub config: ImageConfig,
}

pub(crate) struct ImageStore {
    root: PathBuf,
    /// Serializes index writes.
    index: Mutex<()>,
    /// One unpack per layer at a time (images share layers).
    layer_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    owners: bool,
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

impl ImageStore {
    pub fn open(root: &Path) -> Result<Self> {
        for dir in ["blobs/sha256", "l", "tmp"] {
            std::fs::create_dir_all(root.join(dir)).with_context(|| format!("failed to create {}", root.join(dir).display()))?;
        }
        // Leftovers of interrupted pulls.
        if let Ok(entries) = std::fs::read_dir(root.join("tmp")) {
            for e in entries.flatten() {
                let _ = std::fs::remove_dir_all(e.path()).or_else(|_| std::fs::remove_file(e.path()));
            }
        }
        Ok(Self { root: root.to_path_buf(), index: Mutex::new(()), layer_locks: Mutex::new(HashMap::new()), owners: is_root() })
    }

    pub fn layer_dir(&self, diff_id: &str) -> PathBuf {
        let hex = diff_id.strip_prefix("sha256:").unwrap_or(diff_id);
        self.root.join("l").join(&hex[..16.min(hex.len())])
    }

    fn blob_path(&self, digest: &str) -> PathBuf {
        self.root.join("blobs/sha256").join(digest.strip_prefix("sha256:").unwrap_or(digest))
    }

    fn put_blob(&self, bytes: &[u8], digest: &str) -> Result<()> {
        let actual = format!("sha256:{}", sha256_hex(bytes));
        if actual != digest {
            bail!("digest mismatch: expected {digest}, got {actual}");
        }
        let path = self.blob_path(digest);
        if !path.exists() {
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &path)?;
        }
        Ok(())
    }

    fn load_index(&self) -> Index {
        std::fs::read(self.root.join("refs.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn update_index(&self, f: impl FnOnce(&mut Index)) -> Result<()> {
        let _guard = self.index.lock().unwrap();
        let mut index = self.load_index();
        f(&mut index);
        let path = self.root.join("refs.json");
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(&index)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn image_of(&self, reference: &Reference, entry: &IndexEntry) -> Option<Image> {
        let layers: Vec<PathBuf> = entry.layers.iter().map(|d| self.layer_dir(d)).collect();
        if !layers.iter().all(|l| l.is_dir()) {
            return None;
        }
        let config: Value = serde_json::from_slice(&std::fs::read(self.blob_path(&entry.config)).ok()?).ok()?;
        Some(Image {
            id: format!("{}/{}@{}", reference.registry, reference.repository, entry.manifest),
            layers,
            config: ImageConfig::from_blob(&config),
        })
    }

    /// The image if it is fully in the store.
    pub fn get(&self, image: &str) -> Option<Image> {
        let reference = Reference::parse(image).ok()?;
        let entry = self.load_index().images.get(&reference.canonical()).cloned()?;
        let found = self.image_of(&reference, &entry)?;
        let now = chrono::Utc::now().timestamp();
        if now - entry.last_used > 60 {
            let key = reference.canonical();
            let _ = self.update_index(|i| {
                if let Some(e) = i.images.get_mut(&key) {
                    e.last_used = now;
                }
            });
        }
        Some(found)
    }

    /// Pull `image` unless it is here (`always`: check the registry for a
    /// newer version of the tag, cheaply with a HEAD).
    pub async fn pull(&self, image: &str, always: bool) -> Result<Image> {
        let reference = Reference::parse(image)?;
        let key = reference.canonical();
        let existing = self.load_index().images.get(&key).cloned();
        if let Some(entry) = &existing {
            if let Some(found) = self.image_of(&reference, entry) {
                if !always || reference.digest.is_some() {
                    return Ok(found);
                }
                let mut session = Session::new(&reference);
                if let Ok(Some(digest)) = session.head_manifest().await {
                    if digest == entry.top {
                        return Ok(found);
                    }
                }
            }
        }
        let mut session = Session::new(&reference);
        let (top, manifest_digest, manifest) = session.resolve_manifest().await?;
        let config_desc = &manifest["config"];
        let config_digest = config_desc["digest"].as_str().context("manifest has no config")?.to_string();
        if !self.blob_path(&config_digest).exists() {
            let bytes = session.blob_bytes(&config_digest).await?;
            self.put_blob(&bytes, &config_digest)?;
        }
        let config: Value = serde_json::from_slice(&std::fs::read(self.blob_path(&config_digest))?)?;
        let diff_ids: Vec<String> = config
            .pointer("/rootfs/diff_ids")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        let layers: Vec<(String, String)> = manifest["layers"]
            .as_array()
            .context("manifest has no layers")?
            .iter()
            .filter_map(|l| l["digest"].as_str().map(str::to_string))
            .zip(diff_ids.iter().cloned())
            .collect();
        if layers.len() != diff_ids.len() || layers.len() != manifest["layers"].as_array().map_or(0, Vec::len) {
            bail!("image {image}: manifest and config disagree on the layers");
        }
        let session = Arc::new(tokio::sync::Mutex::new(session));
        let results: Vec<Result<()>> = futures_util::stream::iter(layers.iter().cloned())
            .map(|(digest, diff_id)| {
                let session = session.clone();
                async move { self.fetch_layer(session, &digest, &diff_id).await }
            })
            .buffer_unordered(MAX_CONCURRENT_LAYERS)
            .collect()
            .await;
        for r in results {
            r.with_context(|| format!("failed to pull image \"{image}\""))?;
        }
        let manifest_bytes = serde_json::to_vec(&manifest)?;
        let path = self.blob_path(&manifest_digest);
        if !path.exists() {
            std::fs::write(&path, &manifest_bytes)?;
        }
        let entry = IndexEntry {
            top,
            manifest: manifest_digest,
            config: config_digest,
            layers: diff_ids,
            last_used: chrono::Utc::now().timestamp(),
        };
        self.update_index(|i| {
            i.images.insert(key.clone(), entry.clone());
        })?;
        self.image_of(&reference, &entry).context("pulled image is incomplete")
    }

    async fn fetch_layer(&self, session: Arc<tokio::sync::Mutex<Session>>, digest: &str, diff_id: &str) -> Result<()> {
        let dir = self.layer_dir(diff_id);
        if dir.is_dir() {
            return Ok(());
        }
        let lock = self.layer_locks.lock().unwrap().entry(diff_id.to_string()).or_default().clone();
        let _held = lock.lock().await;
        if dir.is_dir() {
            return Ok(());
        }
        let tmp_file = self.root.join("tmp").join(format!("{}.blob", random_hex(8)));
        let result = async {
            // The token may need refreshing; take the session only to open
            // the stream so layers download in parallel.
            let response = session.lock().await.blob(digest).await?;
            download(response, &tmp_file, digest).await?;
            let tmp_dir = self.root.join("tmp").join(random_hex(8));
            let (src, dst, expected, owners) = (tmp_file.clone(), tmp_dir.clone(), diff_id.to_string(), self.owners);
            tokio::task::spawn_blocking(move || unpack_layer(&src, &dst, &expected, owners))
                .await?
                .with_context(|| format!("failed to unpack layer {digest}"))?;
            if let Err(e) = std::fs::rename(&tmp_dir, &dir) {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                if !dir.is_dir() {
                    return Err(e.into());
                }
            }
            Ok(())
        }
        .await;
        let _ = std::fs::remove_file(&tmp_file);
        result
    }

    /// Drop layers no image uses and, when the disk is above 85 % full,
    /// images no container uses (least recently used first) until it is
    /// below 80 %, like the kubelet's image GC thresholds.
    pub fn gc(&self, in_use: &HashSet<String>) -> Result<()> {
        let mut removed = Vec::new();
        if disk_usage(&self.root).is_some_and(|u| u > 0.85) {
            let index = self.load_index();
            let mut candidates: Vec<(&String, &IndexEntry)> = index
                .images
                .iter()
                .filter(|(_, e)| !in_use.contains(&e.manifest))
                .collect();
            candidates.sort_by_key(|(_, e)| e.last_used);
            for (key, _) in candidates {
                removed.push(key.clone());
                self.update_index(|i| {
                    i.images.remove(key);
                })?;
                self.sweep()?;
                if disk_usage(&self.root).is_none_or(|u| u < 0.80) {
                    break;
                }
            }
        }
        self.sweep()?;
        if !removed.is_empty() {
            eprintln!("node-agent: image GC removed {}", removed.join(", "));
        }
        Ok(())
    }

    /// Remove layers and blobs that no indexed image refers to.
    fn sweep(&self) -> Result<()> {
        let index = self.load_index();
        let mut layers = HashSet::new();
        let mut blobs = HashSet::new();
        for e in index.images.values() {
            for l in &e.layers {
                layers.insert(self.layer_dir(l));
            }
            blobs.insert(self.blob_path(&e.manifest));
            blobs.insert(self.blob_path(&e.config));
        }
        for entry in std::fs::read_dir(self.root.join("l"))?.flatten() {
            if !layers.contains(&entry.path()) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        for entry in std::fs::read_dir(self.root.join("blobs/sha256"))?.flatten() {
            if !blobs.contains(&entry.path()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(())
    }
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut buf);
    hex::encode(buf)
}

fn disk_usage(path: &Path) -> Option<f64> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: statvfs writes into the zeroed struct we own.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 || s.f_blocks == 0 {
        return None;
    }
    Some(1.0 - (s.f_bavail as f64 / s.f_blocks as f64))
}

/// Stream a blob to `path`, checking its digest.
async fn download(response: hyper::Response<hyper::body::Incoming>, path: &Path, digest: &str) -> Result<()> {
    let mut body = response.into_body();
    let mut file = tokio::fs::File::create(path).await?;
    let mut hasher = Sha256::new();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(60), body.frame())
            .await
            .context("registry stopped sending")?;
        let Some(frame) = frame else { break };
        if let Ok(data) = frame?.into_data() {
            hasher.update(&data);
            file.write_all(&data).await?;
        }
    }
    file.flush().await?;
    let actual = format!("sha256:{}", hex::encode(hasher.finalize()));
    if actual != digest {
        bail!("digest mismatch: expected {digest}, got {actual}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Unpacking

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
/// Skippable zstd frames: 0x184D2A50..=0x184D2A5F (zstd:chunked layers
/// carry their table of contents in one).
const SKIPPABLE_MAGIC: u32 = 0x184d_2a50;

/// Decompresses a zstd stream of any number of frames (`tar+zstd` layers
/// may hold several, plus skippable ones), in pure Rust.
struct ZstdFrames<R: std::io::BufRead> {
    source: R,
    decoder: ruzstd::decoding::FrameDecoder,
    in_frame: bool,
}

impl<R: std::io::BufRead> ZstdFrames<R> {
    fn new(source: R) -> Self {
        Self { source, decoder: ruzstd::decoding::FrameDecoder::new(), in_frame: false }
    }
}

impl<R: std::io::BufRead> Read for ZstdFrames<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use ruzstd::decoding::BlockDecodingStrategy;
        use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
        loop {
            if self.in_frame {
                while self.decoder.can_collect() == 0 && !self.decoder.is_finished() {
                    self.decoder
                        .decode_blocks(&mut self.source, BlockDecodingStrategy::UptoBlocks(1))
                        .map_err(std::io::Error::other)?;
                }
                let n = self.decoder.read(buf)?;
                if n > 0 || buf.is_empty() {
                    return Ok(n);
                }
                self.in_frame = false;
            }
            if self.source.fill_buf()?.is_empty() {
                return Ok(0);
            }
            match self.decoder.reset(&mut self.source) {
                Ok(()) => self.in_frame = true,
                Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame { length, .. })) => {
                    std::io::copy(&mut (&mut self.source).take(u64::from(length)), &mut std::io::sink())?;
                }
                Err(e) => return Err(std::io::Error::other(e)),
            }
        }
    }
}

const WHITEOUT_PREFIX: &str = ".wh.";
const OPAQUE_MARKER: &str = ".wh..wh..opq";

struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

/// Unpack one layer blob (gzip or plain tar) into `dest` in overlayfs
/// format: `.wh.<name>` becomes a 0/0 character device and an opaque
/// marker the `trusted.overlay.opaque` xattr on its directory. The
/// uncompressed stream must hash to `diff_id`. `owners`: keep the archive's
/// file owners (needs root).
pub(crate) fn unpack_layer(blob: &Path, dest: &Path, diff_id: &str, owners: bool) -> Result<()> {
    let mut file = std::fs::File::open(blob)?;
    let mut magic = [0u8; 4];
    let n = file.read(&mut magic)?;
    let head = std::io::Cursor::new(magic[..n].to_vec());
    let raw = head.chain(file);
    let reader: Box<dyn Read> = if magic[..2] == [0x1f, 0x8b] {
        Box::new(flate2::read::MultiGzDecoder::new(raw))
    } else if magic == ZSTD_MAGIC || u32::from_le_bytes(magic) & 0xffff_fff0 == SKIPPABLE_MAGIC {
        Box::new(ZstdFrames::new(std::io::BufReader::new(raw)))
    } else {
        Box::new(raw)
    };
    let mut hashing = HashingReader { inner: reader, hasher: Sha256::new() };
    std::fs::create_dir_all(dest)?;
    {
        let mut archive = tar::Archive::new(&mut hashing);
        archive.set_preserve_permissions(true);
        archive.set_preserve_mtime(true);
        archive.set_preserve_ownerships(owners);
        archive.set_unpack_xattrs(owners);
        archive.set_overwrite(true);
        for entry in archive.entries().context("reading layer")? {
            let mut entry = entry.context("reading layer entry")?;
            let path = entry.path()?.into_owned();
            let rel = relative(&path).with_context(|| format!("unsafe path in layer: {}", path.display()))?;
            let name = rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let parent = dest.join(rel.parent().unwrap_or(Path::new("")));
            if name == OPAQUE_MARKER {
                std::fs::create_dir_all(&parent)?;
                if owners {
                    set_opaque(&parent)?;
                } else {
                    // Without root there are no overlay xattrs (tests).
                    std::fs::write(parent.join(".opaque"), b"y")?;
                }
                continue;
            }
            if let Some(hidden) = name.strip_prefix(WHITEOUT_PREFIX) {
                std::fs::create_dir_all(&parent)?;
                let target = parent.join(hidden);
                match std::fs::symlink_metadata(&target) {
                    Ok(m) if m.is_dir() => std::fs::remove_dir_all(&target)?,
                    Ok(_) => std::fs::remove_file(&target)?,
                    Err(_) => {}
                }
                if owners {
                    make_whiteout(&target)?;
                } else {
                    // Without root there are no device nodes (tests).
                    std::fs::write(&target, b"")?;
                }
                continue;
            }
            if let Err(e) = entry.unpack_in(dest) {
                // A hard link to a file of a lower layer has nothing to
                // link to here; the file it names is still visible.
                if entry.header().entry_type().is_hard_link() {
                    continue;
                }
                return Err(e).with_context(|| format!("failed to unpack {}", rel.display()));
            }
        }
    }
    std::io::copy(&mut hashing, &mut std::io::sink())?;
    let actual = format!("sha256:{}", hex::encode(hashing.hasher.finalize()));
    if actual != diff_id {
        bail!("layer content does not match its diff id: expected {diff_id}, got {actual}");
    }
    Ok(())
}

fn relative(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => bail!("path traversal"),
        }
    }
    Ok(out)
}

fn make_whiteout(path: &Path) -> Result<()> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    // SAFETY: c is a valid NUL-terminated path.
    if unsafe { libc::mknod(c.as_ptr(), libc::S_IFCHR, 0) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("failed to create whiteout {}", path.display()));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_opaque(dir: &Path) -> Result<()> {
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes())?;
    let name = c"trusted.overlay.opaque";
    // SAFETY: valid path, name and a 1-byte value.
    if unsafe { libc::setxattr(c.as_ptr(), name.as_ptr(), b"y".as_ptr().cast(), 1, 0) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("failed to mark {} opaque", dir.display()));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn set_opaque(_dir: &Path) -> Result<()> {
    bail!("overlayfs needs Linux")
}

// ---------------------------------------------------------------------------
// Registry client

trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .expect("TLS versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            Arc::new(config)
        })
        .clone()
}

struct Url {
    https: bool,
    host: String,
    port: u16,
    path: String,
}

fn parse_url(url: &str) -> Result<Url> {
    let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        bail!("unsupported URL {url}");
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h.to_string(), p.parse()?),
        _ => (authority.to_string(), if https { 443 } else { 80 }),
    };
    Ok(Url { https, host, port, path: path.to_string() })
}

async fn send(method: hyper::Method, url: &str, headers: &[(&str, String)]) -> Result<hyper::Response<hyper::body::Incoming>> {
    let u = parse_url(url)?;
    let tcp = tokio::time::timeout(Duration::from_secs(30), tokio::net::TcpStream::connect((u.host.as_str(), u.port)))
        .await
        .with_context(|| format!("timed out connecting to {}", u.host))?
        .with_context(|| format!("failed to connect to {}", u.host))?;
    let _ = tcp.set_nodelay(true);
    let io: Box<dyn Io> = if u.https {
        let name = rustls::pki_types::ServerName::try_from(u.host.clone())?;
        let tls = tokio_rustls::TlsConnector::from(tls_config()).connect(name, tcp).await.with_context(|| format!("TLS handshake with {} failed", u.host))?;
        Box::new(tls)
    } else {
        Box::new(tcp)
    };
    let (mut sender, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(io)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let authority = if (u.https && u.port == 443) || (!u.https && u.port == 80) { u.host.clone() } else { format!("{}:{}", u.host, u.port) };
    let mut req = hyper::Request::builder()
        .method(method)
        .uri(&u.path)
        .header(hyper::header::HOST, authority)
        .header(hyper::header::USER_AGENT, USER_AGENT);
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let req = req.body(http_body_util::Empty::<Bytes>::new())?;
    tokio::time::timeout(Duration::from_secs(60), sender.send_request(req))
        .await
        .context("registry did not answer")?
        .map_err(Into::into)
}

/// Parse `Bearer realm="..",service="..",scope=".."`.
pub(crate) fn parse_challenge(header: &str) -> Option<(String, HashMap<String, String>)> {
    let (scheme, rest) = header.trim().split_once(' ')?;
    let mut params = HashMap::new();
    let mut rest = rest.trim();
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        let after = after.trim_start();
        let (value, tail) = if let Some(quoted) = after.strip_prefix('"') {
            let end = quoted.find('"')?;
            (&quoted[..end], &quoted[end + 1..])
        } else {
            match after.find(',') {
                Some(i) => (&after[..i], &after[i..]),
                None => (after, ""),
            }
        };
        params.insert(key.trim().to_lowercase(), value.to_string());
        rest = tail.trim_start_matches([',', ' ']);
    }
    Some((scheme.to_lowercase(), params))
}

struct Session {
    reference: Reference,
    token: Option<String>,
}

impl Session {
    fn new(reference: &Reference) -> Self {
        Self { reference: reference.clone(), token: None }
    }

    fn url(&self, kind: &str, r: &str) -> String {
        format!("{}://{}/v2/{}/{kind}/{r}", self.reference.scheme(), self.reference.api_host(), self.reference.repository)
    }

    /// A request to the registry, authenticating (anonymously) when it asks,
    /// following redirects (blobs live on a CDN; the token stays home).
    async fn get(&mut self, method: hyper::Method, url: &str, accept: Option<&str>) -> Result<hyper::Response<hyper::body::Incoming>> {
        let home = parse_url(url)?.host;
        let mut url = url.to_string();
        let mut authed = false;
        for _ in 0..8 {
            let mut headers = Vec::new();
            if let Some(a) = accept {
                headers.push(("accept", a.to_string()));
            }
            let same_host = parse_url(&url)?.host == home;
            if let (Some(t), true) = (&self.token, same_host) {
                headers.push(("authorization", format!("Bearer {t}")));
            }
            let resp = send(method.clone(), &url, &headers).await?;
            let status = resp.status();
            if status.is_redirection() {
                let location = resp
                    .headers()
                    .get(hyper::header::LOCATION)
                    .and_then(|l| l.to_str().ok())
                    .context("redirect without a location")?;
                url = if location.starts_with("http") {
                    location.to_string()
                } else {
                    let u = parse_url(&url)?;
                    format!("{}://{}:{}{location}", if u.https { "https" } else { "http" }, u.host, u.port)
                };
                continue;
            }
            if status == hyper::StatusCode::UNAUTHORIZED && !authed && same_host {
                let challenge = resp
                    .headers()
                    .get(hyper::header::WWW_AUTHENTICATE)
                    .and_then(|h| h.to_str().ok())
                    .and_then(parse_challenge);
                match challenge {
                    Some((scheme, params)) if scheme == "bearer" => {
                        self.token = Some(self.fetch_token(&params).await?);
                        authed = true;
                        continue;
                    }
                    _ => bail!("{} requires authentication (pull secrets are not supported yet)", self.reference.registry),
                }
            }
            return Ok(resp);
        }
        bail!("too many redirects fetching {url}")
    }

    async fn fetch_token(&self, params: &HashMap<String, String>) -> Result<String> {
        let realm = params.get("realm").context("auth challenge without a realm")?;
        let mut query = Vec::new();
        if let Some(s) = params.get("service") {
            query.push(format!("service={s}"));
        }
        let scope = params.get("scope").cloned().unwrap_or_else(|| format!("repository:{}:pull", self.reference.repository));
        query.push(format!("scope={scope}"));
        let url = format!("{realm}{}{}", if realm.contains('?') { "&" } else { "?" }, query.join("&"));
        let resp = send(hyper::Method::GET, &url, &[]).await?;
        if !resp.status().is_success() {
            bail!("token request to {realm} failed: {}", resp.status());
        }
        let body = resp.into_body().collect().await?.to_bytes();
        let json: Value = serde_json::from_slice(&body)?;
        json.get("token")
            .or_else(|| json.get("access_token"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("token response without a token")
    }

    async fn fetch_manifest(&mut self, r: &str) -> Result<(String, Value)> {
        let url = self.url("manifests", r);
        let resp = self.get(hyper::Method::GET, &url, Some(MANIFEST_TYPES)).await?;
        let status = resp.status();
        let body = resp.into_body().collect().await?.to_bytes();
        if status == hyper::StatusCode::NOT_FOUND {
            bail!("{}: not found", self.reference.canonical());
        }
        if !status.is_success() {
            bail!("{}: registry answered {status}: {}", self.reference.canonical(), String::from_utf8_lossy(&body[..body.len().min(200)]));
        }
        let digest = format!("sha256:{}", sha256_hex(&body));
        if r.starts_with("sha256:") && digest != r {
            bail!("manifest digest mismatch: expected {r}, got {digest}");
        }
        Ok((digest, serde_json::from_slice(&body).context("invalid manifest")?))
    }

    /// (digest the reference resolved to, image manifest digest, manifest)
    async fn resolve_manifest(&mut self) -> Result<(String, String, Value)> {
        let r = self.reference.manifest_ref().to_string();
        let (top, manifest) = self.fetch_manifest(&r).await?;
        if manifest.get("manifests").is_some() {
            let (arch, variant) = host_platform();
            let chosen = select_platform(&manifest, arch, variant)
                .and_then(|m| m["digest"].as_str())
                .with_context(|| format!("no linux/{arch} image for {}", self.reference.canonical()))?
                .to_string();
            let (digest, image) = self.fetch_manifest(&chosen).await?;
            return Ok((top, digest, image));
        }
        if manifest.get("schemaVersion").and_then(Value::as_i64) == Some(1) {
            bail!("{}: schema 1 manifests are not supported", self.reference.canonical());
        }
        Ok((top.clone(), top, manifest))
    }

    /// The digest a tag points at now, without downloading the manifest.
    async fn head_manifest(&mut self) -> Result<Option<String>> {
        let url = self.url("manifests", self.reference.manifest_ref().to_string().as_str());
        let resp = self.get(hyper::Method::HEAD, &url, Some(MANIFEST_TYPES)).await?;
        Ok(resp
            .headers()
            .get("docker-content-digest")
            .and_then(|h| h.to_str().ok())
            .map(str::to_string)
            .filter(|_| resp.status().is_success()))
    }

    async fn blob(&mut self, digest: &str) -> Result<hyper::Response<hyper::body::Incoming>> {
        let url = self.url("blobs", digest);
        let resp = self.get(hyper::Method::GET, &url, None).await?;
        if !resp.status().is_success() {
            bail!("fetching blob {digest}: registry answered {}", resp.status());
        }
        Ok(resp)
    }

    async fn blob_bytes(&mut self, digest: &str) -> Result<Bytes> {
        let body = self.blob(digest).await?.into_body().collect().await?.to_bytes();
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    #[test]
    fn parses_references() {
        let r = Reference::parse("nginx:alpine").unwrap();
        assert_eq!((r.registry.as_str(), r.repository.as_str(), r.tag.as_deref()), ("docker.io", "library/nginx", Some("alpine")));
        assert_eq!(r.canonical(), "docker.io/library/nginx:alpine");
        assert_eq!(r.api_host(), "registry-1.docker.io");
        assert_eq!(Reference::parse("busybox").unwrap().canonical(), "docker.io/library/busybox:latest");
        let r = Reference::parse("ghcr.io/org/app/sub:v1").unwrap();
        assert_eq!((r.registry.as_str(), r.repository.as_str()), ("ghcr.io", "org/app/sub"));
        let r = Reference::parse("localhost:5000/app").unwrap();
        assert_eq!((r.registry.as_str(), r.tag.as_deref(), r.scheme()), ("localhost:5000", Some("latest"), "http"));
        let d = format!("sha256:{}", "a".repeat(64));
        let r = Reference::parse(&format!("registry.k8s.io/pause:3.9@{d}")).unwrap();
        assert_eq!((r.tag.as_deref(), r.digest.as_deref(), r.manifest_ref()), (Some("3.9"), Some(d.as_str()), d.as_str()));
        assert_eq!(Reference::parse("bitnami/redis").unwrap().repository, "bitnami/redis");
        assert!(Reference::parse("Nginx").is_err());
        assert!(Reference::parse("nginx@sha256:abc").is_err());
    }

    #[test]
    fn picks_the_host_platform() {
        let index = json!({"manifests": [
            {"digest": "sha256:amd", "platform": {"os": "linux", "architecture": "amd64"}},
            {"digest": "sha256:v7", "platform": {"os": "linux", "architecture": "arm", "variant": "v7"}},
            {"digest": "sha256:v6", "platform": {"os": "linux", "architecture": "arm", "variant": "v6"}},
            {"digest": "sha256:arm64", "platform": {"os": "linux", "architecture": "arm64", "variant": "v8"}},
            {"digest": "sha256:att", "platform": {"os": "unknown", "architecture": "unknown"}},
        ]});
        let pick = |a, v| select_platform(&index, a, v).map(|m| m["digest"].as_str().unwrap().to_string());
        assert_eq!(pick("arm64", Some("v8")).as_deref(), Some("sha256:arm64"));
        assert_eq!(pick("arm", Some("v7")).as_deref(), Some("sha256:v7"));
        assert_eq!(pick("amd64", None).as_deref(), Some("sha256:amd"));
        assert_eq!(pick("riscv64", None), None);
    }

    #[test]
    fn parses_auth_challenges() {
        let (scheme, p) = parse_challenge(r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/nginx:pull""#).unwrap();
        assert_eq!(scheme, "bearer");
        assert_eq!(p["realm"], "https://auth.docker.io/token");
        assert_eq!(p["scope"], "repository:library/nginx:pull");
    }

    fn tar_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, path, &data[..]).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn unpacks_multi_frame_zstd_layers() {
        use ruzstd::encoding::{CompressionLevel, compress_to_vec};
        let tmp = tempfile::tempdir().unwrap();
        let tar = tar_of(&[("a", b"first"), ("dir/b", &[7u8; 70_000][..])]);
        // Two frames with a skippable frame between them, as zstd:chunked
        // layers have.
        let (head, tail) = tar.split_at(1024);
        let mut blob = compress_to_vec(head, CompressionLevel::Fastest);
        blob.extend_from_slice(&0x184d_2a5eu32.to_le_bytes());
        blob.extend_from_slice(&3u32.to_le_bytes());
        blob.extend_from_slice(b"toc");
        blob.extend_from_slice(&compress_to_vec(tail, CompressionLevel::Fastest));
        let path = tmp.path().join("blob");
        std::fs::write(&path, &blob).unwrap();
        let diff_id = format!("sha256:{}", sha256_hex(&tar));
        let dest = tmp.path().join("layer");
        unpack_layer(&path, &dest, &diff_id, false).unwrap();
        assert_eq!(std::fs::read(dest.join("a")).unwrap(), b"first");
        assert_eq!(std::fs::read(dest.join("dir/b")).unwrap(), vec![7u8; 70_000]);
    }

    #[test]
    fn unpacks_layers_with_overlay_whiteouts() {
        let tmp = tempfile::tempdir().unwrap();
        let tar = tar_of(&[("etc/keep", b"1"), ("etc/.wh.gone", b""), ("data/.wh..wh..opq", b""), ("data/new", b"n")]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar).unwrap();
        let blob = tmp.path().join("blob");
        std::fs::write(&blob, gz.finish().unwrap()).unwrap();
        let diff_id = format!("sha256:{}", sha256_hex(&tar));
        let dest = tmp.path().join("layer");
        unpack_layer(&blob, &dest, &diff_id, false).unwrap();
        assert_eq!(std::fs::read(dest.join("etc/keep")).unwrap(), b"1");
        // As root these are a 0/0 device and an xattr; as a user, plain
        // files stand in for them.
        assert!(!dest.join("etc/.wh.gone").exists() && dest.join("etc/gone").exists());
        assert!(dest.join("data/.opaque").exists());
        assert_eq!(std::fs::read(dest.join("data/new")).unwrap(), b"n");
        assert!(unpack_layer(&blob, &tmp.path().join("other"), "sha256:00", false).is_err_and(|e| e.to_string().contains("diff id")));
    }
}
