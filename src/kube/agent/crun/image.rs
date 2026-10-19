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
pub(crate) struct IndexEntry {
    /// The digest the reference resolved to (an index for multi-arch images).
    #[serde(default)]
    pub top: String,
    pub manifest: String,
    pub config: String,
    /// Layer diff ids, bottom first.
    pub layers: Vec<String>,
    #[serde(default)]
    pub last_used: i64,
    /// Imported from an archive (`litek8s dev load`), not pulled: there is
    /// no registry to check, whatever the pull policy.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub loaded: bool,
    /// Pulled or built through the Docker API: kept until `docker rmi`,
    /// never removed by the kubelet-like image GC.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
}

/// How to pull: the registry check, the platform, credentials.
#[derive(Clone, Default)]
pub(crate) struct PullOptions {
    /// Check the registry for a newer version of the tag.
    pub always: bool,
    /// `(architecture, variant)`; the host's by default.
    pub platform: Option<(String, Option<String>)>,
    /// Registry credentials (user, password), as `X-Registry-Auth` gives them.
    pub auth: Option<(String, String)>,
    /// Keep the image out of the image GC (Docker API pulls).
    pub pinned: bool,
    /// Progress for clients that show it: (layer digest or "", message).
    pub progress: Option<Arc<dyn Fn(&str, &str) + Send + Sync>>,
}

/// An architecture name as image indexes write it (`linux/amd64`,
/// `arm64/v8`, `x86_64`, `aarch64`) -> (architecture, variant).
pub(crate) fn parse_platform(platform: &str) -> Option<(String, Option<String>)> {
    let p = platform.trim();
    if p.is_empty() {
        return None;
    }
    let parts: Vec<&str> = p.split('/').collect();
    let (arch, variant) = match parts.as_slice() {
        [os, arch] if *os == "linux" => (*arch, None),
        [os, arch, variant] if *os == "linux" => (*arch, Some(*variant)),
        [arch] => (*arch, None),
        [arch, variant] if *arch != "linux" => (*arch, Some(*variant)),
        _ => return None,
    };
    let arch = match arch {
        "x86_64" | "x86-64" => "amd64",
        "aarch64" => "arm64",
        a => a,
    };
    Some((arch.to_string(), variant.map(str::to_string)))
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
    /// Pulls, imports and builds in progress: their blobs and layers are not
    /// indexed yet, so sweeping waits.
    busy: Arc<std::sync::atomic::AtomicUsize>,
}

/// Holds off [`ImageStore::sweep`] while alive.
pub(crate) struct Busy(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
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
        Ok(Self { root: root.to_path_buf(), index: Mutex::new(()), layer_locks: Mutex::new(HashMap::new()), owners: is_root(), busy: Arc::default() })
    }

    /// Mark work that adds blobs or layers before indexing them.
    pub fn busy(&self) -> Busy {
        self.busy.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Busy(self.busy.clone())
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

    /// Every index key with its entry (Docker's image list).
    pub fn entries(&self) -> Vec<(String, IndexEntry)> {
        self.load_index().images.into_iter().collect()
    }

    /// The image config blob.
    pub fn config_blob(&self, digest: &str) -> Option<Value> {
        serde_json::from_slice(&std::fs::read(self.blob_path(digest)).ok()?).ok()
    }

    /// The image an index key (a canonical name or an untagged image's
    /// config digest) refers to, if all of it is in the store.
    pub fn image_by_key(&self, key: &str) -> Option<Image> {
        let entry = self.load_index().images.get(key).cloned()?;
        let reference = Reference::parse(key).unwrap_or_else(|_| Reference {
            registry: "docker.io".into(),
            repository: "library/none".into(),
            tag: None,
            digest: None,
        });
        self.image_of(&reference, &entry)
    }

    /// Point `key` at `entry` (tag), or drop it (`None`, untag). Layers no
    /// key refers to any more are removed.
    pub fn set_key(&self, key: &str, entry: Option<IndexEntry>) -> Result<()> {
        self.update_index(|i| match entry {
            Some(e) => {
                i.images.insert(key.to_string(), e);
            }
            None => {
                i.images.remove(key);
            }
        })?;
        self.sweep()
    }

    /// Size on disk of a layer (cached next to it).
    pub fn layer_size(&self, diff_id: &str) -> u64 {
        let dir = self.layer_dir(diff_id);
        let cache = dir.with_extension("size");
        if let Some(n) = std::fs::read_to_string(&cache).ok().and_then(|s| s.trim().parse().ok()) {
            return n;
        }
        let n = dir_bytes(&dir);
        let _ = std::fs::write(&cache, n.to_string());
        n
    }

    /// Move an unpacked layer (overlay format) into the store as
    /// `diff_id`; a layer already there wins.
    pub fn install_layer(&self, dir: &Path, diff_id: &str) -> Result<()> {
        let dest = self.layer_dir(diff_id);
        if dest.is_dir() {
            let _ = std::fs::remove_dir_all(dir);
            return Ok(());
        }
        if let Err(e) = std::fs::rename(dir, &dest) {
            if !dest.is_dir() {
                return Err(e).context("failed to store the layer");
            }
            let _ = std::fs::remove_dir_all(dir);
        }
        Ok(())
    }

    /// Store an image built here (its layers already installed) under
    /// `names`; returns the index entry (its `config` is the image ID).
    pub fn register(&self, names: &[String], config_bytes: &[u8]) -> Result<IndexEntry> {
        let config_digest = format!("sha256:{}", sha256_hex(config_bytes));
        let config: Value = serde_json::from_slice(config_bytes).context("invalid image config")?;
        let diff_ids: Vec<String> = config
            .pointer("/rootfs/diff_ids")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        for d in &diff_ids {
            if !self.layer_dir(d).is_dir() {
                bail!("layer {d} is not in the store");
            }
        }
        self.put_blob(config_bytes, &config_digest)?;
        let layer_descs: Vec<Value> = diff_ids
            .iter()
            .map(|d| serde_json::json!({"mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": d, "size": self.layer_size(d)}))
            .collect();
        let manifest_bytes = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest, "size": config_bytes.len()},
            "layers": layer_descs,
        }))?;
        let manifest_digest = format!("sha256:{}", sha256_hex(&manifest_bytes));
        self.put_blob(&manifest_bytes, &manifest_digest)?;
        let entry = IndexEntry {
            top: manifest_digest.clone(),
            manifest: manifest_digest,
            config: config_digest.clone(),
            layers: diff_ids,
            last_used: chrono::Utc::now().timestamp(),
            loaded: true,
            pinned: true,
        };
        self.update_index(|i| {
            for n in names {
                i.images.insert(n.clone(), entry.clone());
            }
            if names.is_empty() {
                i.images.insert(config_digest.clone(), entry.clone());
            }
        })?;
        Ok(entry)
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
        self.pull_with(image, &PullOptions { always, ..Default::default() }).await
    }

    pub async fn pull_with(&self, image: &str, opts: &PullOptions) -> Result<Image> {
        let _busy = self.busy();
        let reference = Reference::parse(image)?;
        let key = reference.canonical();
        let always = opts.always;
        let progress = |id: &str, msg: &str| {
            if let Some(p) = &opts.progress {
                p(id, msg);
            }
        };
        let existing = self.load_index().images.get(&key).cloned();
        let platform_matches = |entry: &IndexEntry| match &opts.platform {
            None => true,
            Some((arch, _)) => self
                .config_blob(&entry.config)
                .and_then(|c| c["architecture"].as_str().map(|a| a == arch))
                .unwrap_or(true),
        };
        if let Some(entry) = &existing {
            if let Some(found) = self.image_of(&reference, entry).filter(|_| platform_matches(entry)) {
                if !always || reference.digest.is_some() || entry.loaded {
                    if opts.pinned && !entry.pinned {
                        let _ = self.update_index(|i| {
                            if let Some(e) = i.images.get_mut(&key) {
                                e.pinned = true;
                            }
                        });
                    }
                    return Ok(found);
                }
                let mut session = Session::new(&reference);
                session.auth = opts.auth.clone();
                if let Ok(Some(digest)) = session.head_manifest().await {
                    if digest == entry.top {
                        progress("", "Image is up to date");
                        return Ok(found);
                    }
                }
            }
        }
        let mut session = Session::new(&reference);
        session.auth = opts.auth.clone();
        session.platform = opts.platform.clone();
        let (top, manifest_digest, manifest) = session.resolve_manifest().await?;
        progress("", &format!("Pulling from {}", reference.repository.trim_start_matches("library/")));
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
                let short = digest.trim_start_matches("sha256:").chars().take(12).collect::<String>();
                async move {
                    if self.layer_dir(&diff_id).is_dir() {
                        progress(&short, "Already exists");
                        return Ok(());
                    }
                    progress(&short, "Pulling fs layer");
                    let r = self.fetch_layer(session, &digest, &diff_id).await;
                    if r.is_ok() {
                        progress(&short, "Pull complete");
                    }
                    r
                }
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
        progress("", &format!("Digest: {top}"));
        let entry = IndexEntry {
            top,
            manifest: manifest_digest,
            config: config_digest,
            layers: diff_ids,
            last_used: chrono::Utc::now().timestamp(),
            loaded: false,
            pinned: opts.pinned || existing.as_ref().is_some_and(|e| e.pinned),
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
            let unpacked = tokio::task::spawn_blocking(move || unpack_layer(&src, &dst, &expected, owners)).await?;
            if let Err(e) = unpacked {
                // A failed unpack is retried with the pod's backoff: do not
                // leave a partial tree behind each time.
                let _ = std::fs::remove_dir_all(&tmp_dir);
                return Err(e.context(format!("failed to unpack layer {digest}")));
            }
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

    /// Where archives being imported are staged (same filesystem as the
    /// layers).
    pub fn staging_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Import the images of a `docker save` / `podman save` archive (Docker
    /// or OCI layout, plain or gzip tar) for this host's platform. Returns
    /// the names imported. Images keep their tags (`myapp:dev` is then
    /// `docker.io/library/myapp:dev`, which is what a pod's `myapp:dev`
    /// means) and are never pulled over afterwards.
    pub fn import_archive(&self, archive: &Path) -> Result<Vec<String>> {
        let _busy = self.busy();
        let dir = self.staging_dir().join(format!("import-{}", random_hex(8)));
        let result = (|| {
            extract_archive(archive, &dir)?;
            let images = if dir.join("manifest.json").is_file() {
                docker_archive_images(&dir)?
            } else if dir.join("index.json").is_file() {
                oci_layout_images(&dir)?
            } else {
                bail!("not an image archive (no manifest.json or index.json; use `docker save` or `podman save`)");
            };
            if images.is_empty() {
                bail!("the archive has no tagged image for linux/{}", host_platform().0);
            }
            let mut names = Vec::new();
            for img in images {
                names.extend(self.import_one(&dir, img)?);
            }
            Ok(names)
        })();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    fn import_one(&self, dir: &Path, img: ArchiveImage) -> Result<Vec<String>> {
        let config_bytes = std::fs::read(dir.join(&img.config)).with_context(|| format!("archive has no {}", img.config))?;
        let config_digest = format!("sha256:{}", sha256_hex(&config_bytes));
        let config: Value = serde_json::from_slice(&config_bytes).context("invalid image config")?;
        let diff_ids: Vec<String> = config
            .pointer("/rootfs/diff_ids")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        if diff_ids.len() != img.layers.len() {
            bail!("image config and archive disagree on the layers ({} vs {})", diff_ids.len(), img.layers.len());
        }
        let mut layer_descs = Vec::new();
        for (file, diff_id) in img.layers.iter().zip(&diff_ids) {
            let path = dir.join(file);
            let size = std::fs::metadata(&path).with_context(|| format!("archive has no layer {file}"))?.len();
            let digest = match &img.layer_digests {
                Some(d) => d[layer_descs.len()].clone(),
                None => format!("sha256:{}", sha256_file(&path)?),
            };
            layer_descs.push(serde_json::json!({"mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": digest, "size": size}));
            let dest = self.layer_dir(diff_id);
            if dest.is_dir() {
                continue;
            }
            let tmp = self.staging_dir().join(random_hex(8));
            if let Err(e) = unpack_layer(&path, &tmp, diff_id, self.owners) {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(e.context(format!("failed to unpack layer {file}")));
            }
            if let Err(e) = std::fs::rename(&tmp, &dest) {
                let _ = std::fs::remove_dir_all(&tmp);
                if !dest.is_dir() {
                    return Err(e.into());
                }
            }
        }
        self.put_blob(&config_bytes, &config_digest)?;
        let manifest_bytes = match img.manifest {
            Some(bytes) => bytes,
            None => serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest, "size": config_bytes.len()},
                "layers": layer_descs,
            }))?,
        };
        let manifest_digest = format!("sha256:{}", sha256_hex(&manifest_bytes));
        self.put_blob(&manifest_bytes, &manifest_digest)?;
        let mut names = Vec::new();
        for tag in &img.names {
            let reference = Reference::parse(tag).with_context(|| format!("archive names an invalid image {tag:?}"))?;
            names.push(reference.canonical());
        }
        let entry = IndexEntry {
            top: manifest_digest.clone(),
            manifest: manifest_digest,
            config: config_digest,
            layers: diff_ids,
            last_used: chrono::Utc::now().timestamp(),
            loaded: true,
            pinned: false,
        };
        self.update_index(|i| {
            for n in &names {
                i.images.insert(n.clone(), entry.clone());
            }
        })?;
        Ok(names)
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
                .filter(|(_, e)| !in_use.contains(&e.manifest) && !e.pinned)
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
    pub fn sweep(&self) -> Result<()> {
        if self.busy.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            return Ok(());
        }
        // Anything changed (written, or renamed into place) in the last ten
        // minutes may belong to work another process has not indexed yet.
        let recent = |p: &Path| {
            use std::os::unix::fs::MetadataExt;
            std::fs::symlink_metadata(p).is_ok_and(|m| chrono::Utc::now().timestamp() - m.ctime() < 600)
        };
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
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "size") {
                if !layers.contains(&path.with_extension("")) {
                    let _ = std::fs::remove_file(&path);
                }
                continue;
            }
            if !layers.contains(&path) && !recent(&path) {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
        for entry in std::fs::read_dir(self.root.join("blobs/sha256"))?.flatten() {
            if !blobs.contains(&entry.path()) && !recent(&entry.path()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(())
    }
}

/// One image found in an archive: file paths relative to the extracted
/// archive.
struct ArchiveImage {
    names: Vec<String>,
    config: String,
    layers: Vec<String>,
    /// Compressed-layer digests, when the archive has a real manifest.
    layer_digests: Option<Vec<String>>,
    manifest: Option<Vec<u8>>,
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(hex::encode(h.finalize()))
}

/// Unpack an image archive (regular files and directories only).
fn extract_archive(archive: &Path, dest: &Path) -> Result<()> {
    let mut file = std::fs::File::open(archive).with_context(|| format!("failed to open {}", archive.display()))?;
    let mut magic = [0u8; 2];
    let n = file.read(&mut magic)?;
    let raw = std::io::Cursor::new(magic[..n].to_vec()).chain(file);
    let reader: Box<dyn Read> = if magic == [0x1f, 0x8b] { Box::new(flate2::read::MultiGzDecoder::new(raw)) } else { Box::new(raw) };
    std::fs::create_dir_all(dest)?;
    let mut tar = tar::Archive::new(reader);
    for entry in tar.entries().context("reading the archive")? {
        let mut entry = entry.context("reading the archive")?;
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            continue;
        }
        let path = entry.path()?.into_owned();
        let rel = relative(&path).with_context(|| format!("unsafe path in archive: {}", path.display()))?;
        let target = dest.join(&rel);
        if kind.is_dir() {
            std::fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&target)?;
            std::io::copy(&mut entry, &mut out)?;
        }
    }
    Ok(())
}

/// `docker save`'s manifest.json (also written by Docker 25+ next to its
/// OCI layout, and by `podman save` in docker-archive format).
fn docker_archive_images(dir: &Path) -> Result<Vec<ArchiveImage>> {
    let manifest: Value = serde_json::from_slice(&std::fs::read(dir.join("manifest.json"))?).context("invalid manifest.json")?;
    let mut out = Vec::new();
    for m in manifest.as_array().context("manifest.json is not a list")? {
        let names: Vec<String> = m["RepoTags"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
        let config = m["Config"].as_str().context("manifest.json entry without Config")?.to_string();
        let layers: Vec<String> = m["Layers"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
        if names.is_empty() {
            continue;
        }
        out.push(ArchiveImage { names, config, layers, layer_digests: None, manifest: None });
    }
    Ok(out)
}

/// An OCI image layout (`podman save --format oci-archive`, `skopeo`,
/// `docker buildx --output type=oci`).
fn oci_layout_images(dir: &Path) -> Result<Vec<ArchiveImage>> {
    let blob = |digest: &str| -> Result<Vec<u8>> {
        let hex = digest.strip_prefix("sha256:").context("unsupported digest")?;
        std::fs::read(dir.join("blobs/sha256").join(hex)).with_context(|| format!("archive has no blob {digest}"))
    };
    let index: Value = serde_json::from_slice(&std::fs::read(dir.join("index.json"))?).context("invalid index.json")?;
    let (arch, variant) = host_platform();
    let mut out = Vec::new();
    for desc in index["manifests"].as_array().into_iter().flatten() {
        let ann = &desc["annotations"];
        let name = ann["io.containerd.image.name"]
            .as_str()
            .or_else(|| ann["org.opencontainers.image.ref.name"].as_str().filter(|n| n.contains('/') || n.contains(':')));
        let Some(name) = name else { continue };
        let mut desc = desc.clone();
        let mut bytes = blob(desc["digest"].as_str().unwrap_or_default())?;
        let mut doc: Value = serde_json::from_slice(&bytes)?;
        // An index: the manifest for this platform (if the archive has it).
        if doc.get("manifests").is_some() {
            let Some(m) = select_platform(&doc, arch, variant) else { continue };
            desc = m.clone();
            let Ok(b) = blob(desc["digest"].as_str().unwrap_or_default()) else { continue };
            bytes = b;
            doc = serde_json::from_slice(&bytes)?;
        }
        let config = doc["config"]["digest"].as_str().context("manifest without config")?;
        let digests: Vec<String> = doc["layers"].as_array().into_iter().flatten().filter_map(|l| l["digest"].as_str()).map(str::to_string).collect();
        let path = |d: &str| format!("blobs/sha256/{}", d.trim_start_matches("sha256:"));
        out.push(ArchiveImage {
            names: vec![name.to_string()],
            config: path(config),
            layers: digests.iter().map(|d| path(d)).collect(),
            layer_digests: Some(digests),
            manifest: Some(bytes),
        });
    }
    Ok(out)
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

/// Write an unpacked layer (overlay format: 0/0 character devices for
/// whiteouts, an `overlay.opaque` xattr on opaque directories) as a Docker
/// layer tar (`.wh.` files), entries sorted, hard links kept.
pub(crate) fn write_layer_tar(dir: &Path, out: impl std::io::Write) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let mut b = tar::Builder::new(out);
    b.follow_symlinks(false);
    let mut links: HashMap<(u64, u64), PathBuf> = HashMap::new();
    let mut stack: Vec<PathBuf> = vec![PathBuf::new()];
    let empty_file = |name: &Path, mtime: u64| -> tar::Header {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(0);
        h.set_mode(0o644);
        h.set_mtime(mtime);
        let _ = h.set_path(name);
        h.set_cksum();
        h
    };
    while let Some(rel) = stack.pop() {
        let full = dir.join(&rel);
        let mut entries: Vec<_> = std::fs::read_dir(&full)?.flatten().map(|e| e.file_name()).collect();
        entries.sort();
        // Depth-first in sorted order: push in reverse.
        let mut subdirs = Vec::new();
        if !rel.as_os_str().is_empty() && is_opaque(&full) {
            let meta = std::fs::symlink_metadata(&full)?;
            let mut h = empty_file(&rel.join(OPAQUE_MARKER), meta.mtime() as u64);
            b.append(&mut h, std::io::empty())?;
        }
        for name in entries {
            let path = rel.join(&name);
            let abs = dir.join(&path);
            let meta = std::fs::symlink_metadata(&abs)?;
            let ft = meta.file_type();
            if ft.is_char_device() && meta.rdev() == 0 {
                let mut h = empty_file(&rel.join(format!("{WHITEOUT_PREFIX}{}", name.to_string_lossy())), meta.mtime() as u64);
                b.append(&mut h, std::io::empty())?;
                continue;
            }
            if ft.is_file() && meta.nlink() > 1 {
                let key = (meta.dev(), meta.ino());
                if let Some(first) = links.get(&key) {
                    let mut h = tar::Header::new_gnu();
                    h.set_entry_type(tar::EntryType::Link);
                    h.set_size(0);
                    h.set_metadata(&meta);
                    h.set_entry_type(tar::EntryType::Link);
                    h.set_size(0);
                    b.append_link(&mut h, &path, first)?;
                    continue;
                }
                links.insert(key, path.clone());
            }
            b.append_path_with_name(&abs, &path)?;
            if ft.is_dir() {
                subdirs.push(path);
            }
        }
        for d in subdirs.into_iter().rev() {
            stack.push(d);
        }
    }
    b.into_inner()?.flush()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn is_opaque(dir: &Path) -> bool {
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) else { return false };
    ["trusted.overlay.opaque", "user.overlay.opaque"].iter().any(|attr| {
        let a = std::ffi::CString::new(*attr).unwrap();
        let mut buf = [0u8; 4];
        // SAFETY: valid strings and buffer.
        let n = unsafe { libc::lgetxattr(c.as_ptr(), a.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        n == 1 && buf[0] == b'y'
    })
}

#[cfg(not(target_os = "linux"))]
fn is_opaque(_: &Path) -> bool {
    false
}

/// The diff ID (sha256 of the uncompressed tar) of an unpacked layer.
pub(crate) fn layer_diff_id(dir: &Path) -> Result<String> {
    struct HashWriter(Sha256);
    impl std::io::Write for HashWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.update(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut w = HashWriter(Sha256::new());
    write_layer_tar(dir, &mut w)?;
    Ok(format!("sha256:{}", hex::encode(w.0.finalize())))
}

/// Bytes of the files under `dir` (hard links once).
pub(crate) fn dir_bytes(dir: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut total = 0;
    let mut seen = HashSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            match e.metadata() {
                Ok(m) if m.is_dir() => stack.push(e.path()),
                Ok(m) => {
                    if m.nlink() < 2 || seen.insert((m.dev(), m.ino())) {
                        total += m.len();
                    }
                }
                Err(_) => {}
            }
        }
    }
    total
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
    let name = if super::sys::in_user_namespace() { c"user.overlay.opaque" } else { c"trusted.overlay.opaque" };
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

pub(crate) async fn send(method: hyper::Method, url: &str, headers: &[(&str, String)]) -> Result<hyper::Response<hyper::body::Incoming>> {
    let u = parse_url(url)?;
    let tcp = tokio::time::timeout(Duration::from_secs(30), tokio::net::TcpStream::connect((dial_host(&u.host), u.port)))
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

/// Where to connect for `host`. In the dev sandbox (and the dev VM) the
/// host machine's loopback is at another address (`LITEK8S_HOST_LOOPBACK`),
/// so a registry at `localhost:5000` on the developer's machine still works.
/// The variable may also name a file holding the address (the dev VM
/// learns it by DHCP after its API server started).
fn dial_host(host: &str) -> String {
    if !matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") {
        return host.to_string();
    }
    let alias = std::env::var("LITEK8S_HOST_LOOPBACK").unwrap_or_default();
    let alias = if alias.starts_with('/') { std::fs::read_to_string(&alias).unwrap_or_default().trim().to_string() } else { alias };
    if alias.is_empty() { host.to_string() } else { alias }
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
    /// Credentials for the registry (Basic auth, or to get a token).
    auth: Option<(String, String)>,
    /// The registry asked for Basic auth: send it with every request.
    basic: bool,
    /// `(architecture, variant)` to pick from an index; the host's if unset.
    platform: Option<(String, Option<String>)>,
}

impl Session {
    fn new(reference: &Reference) -> Self {
        Self { reference: reference.clone(), token: None, auth: None, basic: false, platform: None }
    }

    fn basic_header(&self) -> Option<String> {
        use base64::Engine;
        let (u, p) = self.auth.as_ref()?;
        Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"))))
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
            } else if let (Some(b), true, true) = (self.basic_header(), self.basic, same_host) {
                headers.push(("authorization", b));
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
                    Some((scheme, _)) if scheme == "basic" && self.auth.is_some() => {
                        self.basic = true;
                        authed = true;
                        continue;
                    }
                    _ => bail!("{} requires authentication (log in with `docker login`, or pull secrets for pods)", self.reference.registry),
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
        let headers: Vec<(&str, String)> = self.basic_header().map(|b| ("authorization", b)).into_iter().collect();
        let resp = send(hyper::Method::GET, &url, &headers).await?;
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
            let host = host_platform();
            let (arch, variant) = match &self.platform {
                Some((a, v)) => (a.as_str(), v.as_deref()),
                None => host,
            };
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
