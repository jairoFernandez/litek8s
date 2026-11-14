//! `docker push`: an image of the store to a registry over the OCI
//! distribution API, for the platform it is stored for (no indexes).
//!
//! Layers are kept unpacked, so each one goes up, in this order, as: a blob
//! the registry already has (HEAD), the blob it was pulled as (mounted from
//! another repository of the same registry, or fetched again from where it
//! was pulled), or a gzip tar of the layer directory made now (its digest is
//! remembered in `l/<id>.push`, so pushing it again is a HEAD). A pulled
//! layer tarred again here rarely has its original diff ID (another tar
//! writer): the config pushed then names the new diff IDs, as `docker save`
//! does, and the image ID changes on the way; its content does not.

use std::io::Write;

use super::*;

const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";
const DOCKER_LAYER: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const OCI_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const MAX_CONCURRENT_UPLOADS: usize = 3;

/// Progress for clients that show it: (short layer ID or "", status,
/// bytes sent of total).
pub(crate) type PushProgress = Arc<dyn Fn(&str, &str, Option<(u64, u64)>) + Send + Sync>;

/// [`PushProgress`] for one layer: (status, bytes sent of total).
type LayerProgress<'a> = dyn Fn(&str, Option<(u64, u64)>) + Sync + 'a;

#[derive(Clone, Default)]
pub(crate) struct PushOptions {
    /// Credentials for the registry (`X-Registry-Auth`); the registry
    /// configuration's otherwise.
    pub auth: Option<Credentials>,
    /// The process's (`registries::current`) if unset.
    pub registries: Option<Arc<Registries>>,
    pub progress: Option<PushProgress>,
}

/// What a push left in the registry.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Pushed {
    pub tag: String,
    pub digest: String,
    pub size: usize,
}

/// A blob already in the registry or uploaded: its descriptor in the
/// manifest, and the diff ID it unpacks to.
struct Sent {
    desc: Value,
    diff_id: String,
}

/// A layer tarred and gzipped here.
struct Made {
    path: PathBuf,
    digest: String,
    size: u64,
    diff_id: String,
}

enum Mount {
    Mounted,
    NotMounted,
}

fn short_id(digest: &str) -> String {
    digest.trim_start_matches("sha256:").chars().take(12).collect()
}

fn is_compressed(media_type: &str) -> bool {
    media_type.ends_with("gzip") || media_type.ends_with("zstd")
}

/// `{"errors": [{"code": "DENIED", "message": ".."}]}` as Docker shows it
/// (`denied: requested access to the resource is denied`).
fn registry_error(what: &str, status: hyper::StatusCode, body: &[u8]) -> anyhow::Error {
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let detail = parsed
        .as_ref()
        .and_then(|v| v["errors"].as_array())
        .and_then(|e| e.first())
        .map(|e| format!("{}: {}", e["code"].as_str().unwrap_or("error").to_lowercase(), e["message"].as_str().unwrap_or_default()));
    match detail {
        Some(d) => anyhow::anyhow!("{d}"),
        None if status == hyper::StatusCode::UNAUTHORIZED => anyhow::anyhow!("unauthorized: authentication required"),
        None => anyhow::anyhow!("{what}: registry answered {status}: {}", String::from_utf8_lossy(&body[..body.len().min(200)])),
    }
}

async fn fail(what: &str, resp: hyper::Response<hyper::body::Incoming>) -> anyhow::Error {
    let status = resp.status();
    let body = small_body(resp).await.unwrap_or_default();
    registry_error(what, status, &body)
}

/// A sha256 of what goes through, counted.
struct Hashing<W> {
    inner: W,
    hash: Sha256,
    len: u64,
}

impl<W: Write> Hashing<W> {
    fn new(inner: W) -> Self {
        Self { inner, hash: Sha256::new(), len: 0 }
    }

    fn finish(self) -> (W, String, u64) {
        (self.inner, format!("sha256:{}", hex::encode(self.hash.finalize())), self.len)
    }
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Tar `layer` (Docker's whiteouts) and gzip it into `out`: (gzip digest,
/// gzip size, diff ID of the tar).
fn make_layer_blob(layer: &Path, out: &Path) -> Result<(String, u64, String)> {
    let file = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(out)?);
    let mut gz = Hashing::new(flate2::write::GzEncoder::new(Hashing::new(file), flate2::Compression::default()));
    write_layer_tar(layer, &mut gz)?;
    let (encoder, diff_id, _) = gz.finish();
    let (file, digest, size) = encoder.finish()?.finish();
    file.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    Ok((digest, size, diff_id))
}

impl Session {
    /// Authenticate once (`GET /v2/`), with the scopes the push needs.
    async fn ping(&mut self) -> Result<()> {
        let url = format!("{}/", self.endpoint.root);
        let resp = self.get(hyper::Method::GET, &url, None).await?;
        if resp.status().is_success() {
            return Ok(());
        }
        Err(fail(&self.endpoint.host, resp).await)
    }

    async fn blob_exists(&mut self, digest: &str) -> Result<bool> {
        let url = self.url("blobs", digest);
        let resp = self.get(hyper::Method::HEAD, &url, None).await?;
        match resp.status() {
            s if s.is_success() => Ok(true),
            hyper::StatusCode::NOT_FOUND => Ok(false),
            _ => Err(fail(&format!("checking blob {digest}"), resp).await),
        }
    }

    fn absolute(&self, location: &str) -> Result<String> {
        if location.starts_with("http://") || location.starts_with("https://") {
            return Ok(location.to_string());
        }
        let u = parse_url(&self.endpoint.root)?;
        Ok(format!("{}://{}:{}{location}", if u.https { "https" } else { "http" }, u.host, u.port))
    }

    /// Start an upload (`POST .../blobs/uploads/`), or mount `digest` from
    /// another repository; the upload's location if there is one.
    async fn start_upload(&mut self, mount: Option<(&str, &str)>) -> Result<(Mount, Option<String>)> {
        let mut url = format!("{}/{}/blobs/uploads/", self.endpoint.root, self.endpoint.repository);
        if let Some((digest, from)) = mount {
            url.push('?');
            url.push_str(&url_form(&[("mount", digest), ("from", from)]));
        }
        let headers = [("content-length", "0".to_string())];
        let resp = self.request(hyper::Method::POST, &url, &headers, &Payload::Empty).await?;
        let status = resp.status();
        if mount.is_some() && status == hyper::StatusCode::CREATED {
            return Ok((Mount::Mounted, None));
        }
        if status != hyper::StatusCode::ACCEPTED {
            return Err(fail("starting an upload", resp).await);
        }
        let location = resp.headers().get(hyper::header::LOCATION).and_then(|l| l.to_str().ok()).map(str::to_string);
        Ok((Mount::NotMounted, location))
    }

    /// Finish the upload at `location` with the whole blob (`PUT ?digest=`).
    async fn put_upload(&mut self, location: &str, digest: &str, payload: &Payload) -> Result<()> {
        let mut url = self.absolute(location)?;
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&url_form(&[("digest", digest)]));
        let headers = [("content-type", "application/octet-stream".to_string())];
        let resp = self.request(hyper::Method::PUT, &url, &headers, payload).await?;
        if !resp.status().is_success() {
            return Err(fail(&format!("uploading blob {digest}"), resp).await);
        }
        Ok(())
    }

    async fn upload(&mut self, digest: &str, payload: &Payload) -> Result<()> {
        let (_, location) = self.start_upload(None).await?;
        let location = location.context("the registry started an upload without a location")?;
        self.put_upload(&location, digest, payload).await
    }

    async fn put_manifest(&mut self, tag: &str, media_type: &str, bytes: &[u8]) -> Result<()> {
        let url = self.url("manifests", tag);
        let headers = [("content-type", media_type.to_string())];
        let resp = self.request(hyper::Method::PUT, &url, &headers, &Payload::Bytes(Bytes::copy_from_slice(bytes))).await?;
        if !resp.status().is_success() {
            return Err(fail("pushing the manifest", resp).await);
        }
        Ok(())
    }
}

impl ImageStore {
    /// The image's manifest as pulled (or last pushed), if it describes
    /// compressed blobs a registry has (not the stand-in a build or an
    /// archive import writes); with its bytes when they are the originals.
    fn origin_manifest(&self, entry: &IndexEntry) -> Option<(Value, Option<Vec<u8>>)> {
        let bytes = std::fs::read(self.blob_path(&entry.manifest)).ok()?;
        let m: Value = serde_json::from_slice(&bytes).ok()?;
        let layers = m["layers"].as_array()?;
        let usable = layers.len() == entry.layers.len()
            && m["config"]["digest"].as_str() == Some(entry.config.as_str())
            && layers.iter().all(|l| l["digest"].is_string() && l["size"].is_u64() && is_compressed(l["mediaType"].as_str().unwrap_or_default()));
        if !usable {
            return None;
        }
        let exact = format!("sha256:{}", sha256_hex(&bytes)) == entry.manifest;
        Some((m, exact.then_some(bytes)))
    }

    /// The gzip blob made of a layer by an earlier push: (digest, size,
    /// diff ID of its tar).
    fn made_before(&self, diff_id: &str) -> Option<(String, u64, String)> {
        let v: Value = serde_json::from_slice(&std::fs::read(self.layer_dir(diff_id).with_extension("push")).ok()?).ok()?;
        Some((v["digest"].as_str()?.to_string(), v["size"].as_u64()?, v["diffID"].as_str()?.to_string()))
    }

    async fn make_blob(&self, diff_id: &str) -> Result<Made> {
        let layer = self.layer_dir(diff_id);
        let path = self.staging_dir().join(format!("push-{}.tar.gz", random_hex(8)));
        let out = path.clone();
        let made = tokio::task::spawn_blocking(move || make_layer_blob(&layer, &out)).await?;
        let (digest, size, made_diff) = match made {
            Ok(m) => m,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(e.context(format!("failed to pack layer {diff_id}")));
            }
        };
        let record = serde_json::json!({"digest": digest, "size": size, "diffID": made_diff});
        let _ = std::fs::write(self.layer_dir(diff_id).with_extension("push"), record.to_string());
        Ok(Made { path, digest, size, diff_id: made_diff })
    }

    /// Push the image `image` (a tagged name) is in the store as.
    pub async fn push(&self, image: &str, opts: &PushOptions) -> Result<Pushed> {
        let _busy = self.busy();
        let reference = Reference::parse(image)?;
        let tag = match (&reference.tag, &reference.digest) {
            (Some(t), None) => t.clone(),
            _ => bail!("cannot push {image}: push a tag, not a digest"),
        };
        let key = reference.canonical();
        let index = self.load_index();
        let entry = index.images.get(&key).cloned().with_context(|| format!("An image does not exist locally with the tag: {image}"))?;
        let config_bytes = std::fs::read(self.blob_path(&entry.config)).context("the image config is not in the store")?;
        let mut config: Value = serde_json::from_slice(&config_bytes).context("invalid image config")?;
        for d in &entry.layers {
            if !self.layer_dir(d).is_dir() {
                bail!("layer {d} is not in the store");
            }
        }
        let origin = self.origin_manifest(&entry);
        let (origin_manifest, origin_bytes) = match &origin {
            Some((m, b)) => (Some(m), b.clone()),
            None => (None, None),
        };
        // Each layer's blob as an image of the store was pulled or pushed
        // (this one first, then the base images of a build), with the names
        // to mount or fetch it from.
        let mut blobs: HashMap<String, (Value, Vec<Reference>)> = HashMap::new();
        let mut manifests: HashMap<String, Option<Value>> = HashMap::new();
        for (k, e) in std::iter::once((&key, &entry)).chain(index.images.iter().filter(|(k, _)| **k != key)) {
            if !e.layers.iter().any(|d| entry.layers.contains(d)) {
                continue;
            }
            let Some(m) = manifests.entry(e.manifest.clone()).or_insert_with(|| self.origin_manifest(e).map(|(m, _)| m)).as_ref() else { continue };
            let name = Reference::parse(k)
                .ok()
                .filter(|_| !k.starts_with("sha256:") && !k.starts_with("build-cache/"))
                .filter(|r| r.registry != reference.registry || r.repository != reference.repository);
            for (i, d) in e.layers.iter().enumerate().filter(|(_, d)| entry.layers.contains(d)) {
                let desc = &m["layers"][i];
                let (known, sources) = blobs.entry(d.clone()).or_insert_with(|| (desc.clone(), Vec::new()));
                if let Some(r) = &name
                    && known["digest"] == desc["digest"]
                    && !sources.iter().any(|s| s.registry == r.registry && s.repository == r.repository)
                {
                    sources.push(r.clone());
                }
            }
        }
        // Docker's manifest format unless the image has another, or a blob
        // only OCI can name (zstd).
        let oci = match origin_manifest {
            Some(m) => m["mediaType"].as_str() != Some(DOCKER_MANIFEST),
            None => blobs.values().any(|(d, _)| d["mediaType"].as_str().is_some_and(|t| t.ends_with("zstd"))),
        };
        let layer_type = if oci { OCI_LAYER } else { DOCKER_LAYER };
        let progress = |id: &str, msg: &str, of: Option<(u64, u64)>| {
            if let Some(p) = &opts.progress {
                p(id, msg, of);
            }
        };

        let registries = opts.registries.clone().unwrap_or_else(crate::registries::current);
        let ep = registries.endpoints(&reference.registry, &reference.repository).pop().context("no endpoint")?;
        let creds = opts.auth.clone().or_else(|| registries.credentials(&ep));
        let mut session = Session::new(&reference, ep, registries.clone(), creds);
        session.scopes.push(format!("repository:{}:pull,push", reference.repository));
        for (_, sources) in blobs.values() {
            for s in sources.iter().filter(|s| s.registry == reference.registry) {
                let scope = format!("repository:{}:pull", s.repository);
                if !session.scopes.contains(&scope) {
                    session.scopes.push(scope);
                }
            }
        }
        progress("", &format!("The push refers to repository [{}/{}]", reference.registry, reference.repository), None);
        session.ping().await?;

        let mut unique: Vec<String> = Vec::new();
        for d in &entry.layers {
            if !unique.contains(d) {
                unique.push(d.clone());
            }
        }
        for d in unique.iter().rev() {
            progress(&short_id(d), "Preparing", None);
        }
        for d in unique.iter().rev() {
            progress(&short_id(d), "Waiting", None);
        }
        let results: Vec<Result<Sent>> = futures_util::stream::iter(unique.iter().cloned())
            .map(|diff_id| {
                let session = session.clone();
                let (origin, sources) = blobs.get(&diff_id).cloned().map_or((None, Vec::new()), |(d, s)| (Some(d), s));
                let (registries, progress) = (registries.clone(), &progress);
                async move {
                    let short = short_id(&diff_id);
                    let r = self.push_layer(session, &diff_id, origin, &sources, registries, &|msg: &str, of| progress(&short, msg, of), layer_type).await;
                    r.with_context(|| format!("failed to push layer {short}"))
                }
            })
            .buffered(MAX_CONCURRENT_UPLOADS)
            .collect()
            .await;
        let mut sent: HashMap<String, Sent> = HashMap::new();
        for (d, r) in unique.iter().zip(results) {
            sent.insert(d.clone(), r?);
        }
        // gzip blobs are the same bytes under either format's media type.
        let descs: Vec<Value> = entry
            .layers
            .iter()
            .map(|d| {
                let mut desc = sent[d].desc.clone();
                if desc["mediaType"].as_str().is_some_and(|t| t.ends_with("gzip")) {
                    desc["mediaType"] = layer_type.into();
                }
                desc
            })
            .collect();
        let diff_ids: Vec<String> = entry.layers.iter().map(|d| sent[d].diff_id.clone()).collect();

        // The config names the diff IDs of the blobs pushed.
        let rewritten = diff_ids != entry.layers;
        let config_bytes = if rewritten {
            config["rootfs"] = serde_json::json!({"type": "layers", "diff_ids": diff_ids});
            serde_json::to_vec(&config)?
        } else {
            config_bytes
        };
        let config_digest = format!("sha256:{}", sha256_hex(&config_bytes));
        if !session.blob_exists(&config_digest).await? {
            session.upload(&config_digest, &Payload::Bytes(Bytes::from(config_bytes.clone()))).await.context("failed to push the image config")?;
        }

        let same_layers = origin_manifest.is_some_and(|m| m["layers"].as_array().is_some_and(|l| *l == descs));
        let (media_type, manifest_bytes) = match (origin_manifest, origin_bytes) {
            (Some(m), Some(bytes)) if same_layers && !rewritten => (m["mediaType"].as_str().unwrap_or(OCI_MANIFEST).to_string(), bytes),
            (Some(m), _) => {
                let mut m = m.clone();
                m["config"]["digest"] = config_digest.clone().into();
                m["config"]["size"] = config_bytes.len().into();
                m["layers"] = Value::Array(descs);
                if m["mediaType"].is_null() {
                    m["mediaType"] = OCI_MANIFEST.into();
                }
                (m["mediaType"].as_str().unwrap_or(OCI_MANIFEST).to_string(), serde_json::to_vec(&m)?)
            }
            (None, _) => {
                let (manifest_type, config_type) = if oci { (OCI_MANIFEST, OCI_CONFIG) } else { (DOCKER_MANIFEST, DOCKER_CONFIG) };
                let m = serde_json::json!({
                    "schemaVersion": 2,
                    "mediaType": manifest_type,
                    "config": {"mediaType": config_type, "size": config_bytes.len(), "digest": config_digest},
                    "layers": descs,
                });
                (manifest_type.to_string(), serde_json::to_vec(&m)?)
            }
        };
        let digest = format!("sha256:{}", sha256_hex(&manifest_bytes));
        session.put_manifest(&tag, &media_type, &manifest_bytes).await?;
        progress("", &format!("{tag}: digest: {digest} size: {}", manifest_bytes.len()), None);

        // The name now stands for what the registry has: its digest (Docker's
        // RepoDigests), and the blobs to mount the next push from.
        if !rewritten {
            self.put_blob(&manifest_bytes, &digest)?;
            self.update_index(|i| {
                if let Some(e) = i.images.get_mut(&key).filter(|e| e.config == entry.config) {
                    e.top = digest.clone();
                    e.manifest = digest.clone();
                    e.loaded = false;
                }
            })?;
        }
        Ok(Pushed { tag, digest, size: manifest_bytes.len() })
    }

    #[allow(clippy::too_many_arguments)]
    async fn push_layer(
        &self,
        mut target: Session,
        diff_id: &str,
        origin: Option<Value>,
        sources: &[Reference],
        registries: Arc<Registries>,
        progress: &LayerProgress<'_>,
        layer_type: &str,
    ) -> Result<Sent> {
        let exists = |desc: Value, diff_id: &str| -> Result<Sent> {
            progress("Layer already exists", None);
            Ok(Sent { desc, diff_id: diff_id.to_string() })
        };
        let origin_digest = origin.as_ref().and_then(|o| o["digest"].as_str()).map(str::to_string);
        if let (Some(o), Some(d)) = (&origin, &origin_digest)
            && target.blob_exists(d).await?
        {
            return exists(o.clone(), diff_id);
        }
        let made_desc = |digest: &str, size: u64| serde_json::json!({"mediaType": layer_type, "size": size, "digest": digest});
        if let Some((digest, size, made_diff)) = self.made_before(diff_id)
            && target.blob_exists(&digest).await?
        {
            return exists(made_desc(&digest, size), &made_diff);
        }
        if let (Some(o), Some(d)) = (&origin, &origin_digest) {
            let registry = target.reference.registry.clone();
            for s in sources.iter().filter(|s| s.registry == registry) {
                if let Ok((Mount::Mounted, _)) = target.start_upload(Some((d, &s.repository))).await {
                    progress(&format!("Mounted from {}", s.repository), None);
                    return Ok(Sent { desc: o.clone(), diff_id: diff_id.to_string() });
                }
            }
            // Fetched again from where it was pulled.
            let size = o["size"].as_u64().unwrap_or_default();
            for s in sources {
                let path = self.staging_dir().join(format!("push-{}.blob", random_hex(8)));
                let mut fetched = false;
                for mut from in Session::all(s, &PullOptions { registries: Some(registries.clone()), ..Default::default() }) {
                    if let Ok(resp) = from.blob(d).await
                        && download(resp, &path, d).await.is_ok()
                    {
                        fetched = true;
                        break;
                    }
                }
                if fetched {
                    let r = self.send_file(&mut target, d, &path, size, progress).await;
                    let _ = std::fs::remove_file(&path);
                    r?;
                    return Ok(Sent { desc: o.clone(), diff_id: diff_id.to_string() });
                }
                let _ = std::fs::remove_file(&path);
            }
        }
        let made = self.make_blob(diff_id).await?;
        let r = async {
            if target.blob_exists(&made.digest).await? {
                progress("Layer already exists", None);
                return Ok(());
            }
            self.send_file(&mut target, &made.digest, &made.path, made.size, progress).await
        }
        .await;
        let _ = std::fs::remove_file(&made.path);
        r?;
        Ok(Sent { desc: made_desc(&made.digest, made.size), diff_id: made.diff_id })
    }

    async fn send_file(&self, target: &mut Session, digest: &str, path: &Path, size: u64, progress: &LayerProgress<'_>) -> Result<()> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
        let payload = Payload::File {
            path: path.to_path_buf(),
            len: size,
            progress: Some(Arc::new(move |n| {
                let _ = tx.send(n);
            })),
        };
        progress("Pushing", Some((0, size)));
        let upload = target.upload(digest, &payload);
        tokio::pin!(upload);
        let r = loop {
            tokio::select! {
                r = &mut upload => break r,
                Some(n) = rx.recv() => progress("Pushing", Some((n, size))),
            }
        };
        r?;
        progress("Pushed", None);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A registry in memory, plain HTTP on 127.0.0.1: blobs per repository,
    /// uploads (and mounts when `mounts`), manifests by tag and digest. `token`: Bearer
    /// tokens for `user:s3cret` with the scopes asked for, and a push needs
    /// `push` in them.
    #[derive(Default)]
    struct Registry {
        blobs: Mutex<HashMap<String, Vec<u8>>>,
        manifests: Mutex<HashMap<String, (String, Vec<u8>)>>,
        uploads: Mutex<HashMap<String, Vec<u8>>>,
        log: Mutex<Vec<String>>,
        mounts: bool,
        token: bool,
    }

    async fn serve(reg: Arc<Registry>) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else { return };
                let reg = reg.clone();
                tokio::spawn(async move {
                    let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                        let reg = reg.clone();
                        async move { Ok::<_, std::convert::Infallible>(handle(&reg, port, req).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(tcp), svc).await;
                });
            }
        });
        port
    }

    async fn handle(reg: &Registry, port: u16, req: hyper::Request<hyper::body::Incoming>) -> hyper::Response<http_body_util::Full<Bytes>> {
        let reply = |status: u16, body: Vec<u8>| hyper::Response::builder().status(status).body(http_body_util::Full::new(Bytes::from(body))).unwrap();
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        let query: HashMap<String, String> = req
            .uri()
            .query()
            .unwrap_or_default()
            .split('&')
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k.to_string(), v.replace("%3A", ":").replace("%2F", "/")))
            .collect();
        let authz = req.headers().get("authorization").and_then(|h| h.to_str().ok()).unwrap_or_default().to_string();
        let ctype = req.headers().get("content-type").and_then(|h| h.to_str().ok()).unwrap_or_default().to_string();
        reg.log.lock().unwrap().push(format!("{method} {path}"));
        if path == "/token" {
            let scopes: Vec<&str> = req.uri().query().unwrap_or_default().split('&').filter_map(|p| p.strip_prefix("scope=")).collect();
            if authz != "Basic dXNlcjpzM2NyZXQ=" {
                return reply(401, vec![]);
            }
            return reply(200, json!({"token": format!("tok {}", scopes.join(" "))}).to_string().into_bytes());
        }
        let body = req.into_body().collect().await.unwrap().to_bytes().to_vec();
        let Some(rest) = path.strip_prefix("/v2/") else { return reply(404, vec![]) };
        if reg.token {
            let push = method == hyper::Method::POST || method == hyper::Method::PUT || method == hyper::Method::PATCH;
            let repo = rest.split("/blobs/").next().unwrap().split("/manifests/").next().unwrap();
            let need = format!("repository:{repo}:pull{}", if push { ",push" } else { "" });
            let granted = authz.strip_prefix("Bearer tok ").is_some_and(|s| rest.is_empty() || s.split(' ').any(|g| g == need || g == format!("repository:{repo}:pull,push")));
            if !granted {
                let mut r = reply(401, br#"{"errors":[{"code":"UNAUTHORIZED","message":"authentication required"}]}"#.to_vec());
                let scope = if rest.is_empty() { String::new() } else { format!(r#",scope="{need}""#) };
                r.headers_mut().insert("www-authenticate", format!(r#"Bearer realm="http://127.0.0.1:{port}/token",service="test"{scope}"#).parse().unwrap());
                return r;
            }
        }
        if rest.is_empty() {
            return reply(200, b"{}".to_vec());
        }
        if let Some((repo, tail)) = rest.split_once("/blobs/") {
            if let Some(id) = tail.strip_prefix("uploads/") {
                if method == hyper::Method::POST {
                    if let (true, Some(d), Some(from)) = (reg.mounts, query.get("mount"), query.get("from")) {
                        let mut blobs = reg.blobs.lock().unwrap();
                        if let Some(b) = blobs.get(&format!("{from}@{d}")).cloned() {
                            blobs.insert(format!("{repo}@{d}"), b);
                            return reply(201, vec![]);
                        }
                    }
                    let id = random_hex(4);
                    reg.uploads.lock().unwrap().insert(id.clone(), Vec::new());
                    let mut r = reply(202, vec![]);
                    r.headers_mut().insert("location", format!("/v2/{repo}/blobs/uploads/{id}?_state=x").parse().unwrap());
                    return r;
                }
                if method == hyper::Method::PUT {
                    let Some(mut data) = reg.uploads.lock().unwrap().remove(id) else { return reply(404, vec![]) };
                    data.extend_from_slice(&body);
                    let d = query.get("digest").cloned().unwrap_or_default();
                    if format!("sha256:{}", sha256_hex(&data)) != d {
                        return reply(400, br#"{"errors":[{"code":"DIGEST_INVALID","message":"digest did not match"}]}"#.to_vec());
                    }
                    reg.blobs.lock().unwrap().insert(format!("{repo}@{d}"), data);
                    return reply(201, vec![]);
                }
            }
            return match reg.blobs.lock().unwrap().get(&format!("{repo}@{tail}")) {
                Some(b) if method == hyper::Method::HEAD => {
                    let mut r = reply(200, vec![]);
                    r.headers_mut().insert("content-length", b.len().into());
                    r
                }
                Some(b) => reply(200, b.clone()),
                None => reply(404, vec![]),
            };
        }
        if let Some((repo, r)) = rest.split_once("/manifests/") {
            if method == hyper::Method::PUT {
                let d = format!("sha256:{}", sha256_hex(&body));
                let mut m = reg.manifests.lock().unwrap();
                m.insert(format!("{repo}:{r}"), (ctype.clone(), body.clone()));
                m.insert(format!("{repo}:{d}"), (ctype, body));
                return reply(201, vec![]);
            }
            return match reg.manifests.lock().unwrap().get(&format!("{repo}:{r}")) {
                Some((t, b)) => {
                    let mut resp = reply(200, if method == hyper::Method::HEAD { vec![] } else { b.clone() });
                    resp.headers_mut().insert("content-type", t.parse().unwrap());
                    resp.headers_mut().insert("docker-content-digest", format!("sha256:{}", sha256_hex(b)).parse().unwrap());
                    resp
                }
                None => reply(404, vec![]),
            };
        }
        reply(404, vec![])
    }

    /// An image built here: two layers and a config, as `docker build`
    /// registers them.
    fn built(store: &ImageStore, root: &Path, name: &str) -> IndexEntry {
        let mut diff_ids = Vec::new();
        for (i, files) in [[("etc/os", "base")], [("app/run.sh", "echo hi")]].iter().enumerate() {
            let dir = root.join(format!("src{i}"));
            for (p, data) in files {
                std::fs::create_dir_all(dir.join(p).parent().unwrap()).unwrap();
                std::fs::write(dir.join(p), data).unwrap();
            }
            let d = layer_diff_id(&dir).unwrap();
            store.install_layer(&dir, &d).unwrap();
            diff_ids.push(d);
        }
        let (arch, _) = host_platform();
        let config = serde_json::to_vec(&json!({"architecture": arch, "os": "linux", "config": {"Cmd": ["/app/run.sh"]}, "rootfs": {"type": "layers", "diff_ids": diff_ids}})).unwrap();
        store.register(&[Reference::parse(name).unwrap().canonical()], &config).unwrap()
    }

    fn recorder() -> (PushOptions, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let l = lines.clone();
        let opts = PushOptions {
            progress: Some(Arc::new(move |id: &str, msg: &str, _| l.lock().unwrap().push(if id.is_empty() { msg.to_string() } else { format!("{id} {msg}") }))),
            ..Default::default()
        };
        (opts, lines)
    }

    fn gunzip(b: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(b).read_to_end(&mut out).unwrap();
        out
    }

    #[tokio::test]
    async fn pushes_a_built_image_and_pulls_it_back() {
        let reg = Arc::new(Registry::default());
        let port = serve(reg.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let store = ImageStore::open(&tmp.path().join("store")).unwrap();
        let name = format!("localhost:{port}/team/app:v1");
        let entry = built(&store, tmp.path(), &name);
        let (opts, lines) = recorder();
        let pushed = store.push(&name, &opts).await.unwrap();

        let (ctype, manifest) = reg.manifests.lock().unwrap().get("team/app:v1").cloned().unwrap();
        assert_eq!(ctype, DOCKER_MANIFEST);
        assert_eq!(pushed.digest, format!("sha256:{}", sha256_hex(&manifest)));
        let m: Value = serde_json::from_slice(&manifest).unwrap();
        // The image ID survives: the config is the one built, and every
        // layer blob unpacks to the diff ID it names.
        assert_eq!(m["config"]["digest"], entry.config);
        let blobs = reg.blobs.lock().unwrap().clone();
        for (l, d) in m["layers"].as_array().unwrap().iter().zip(&entry.layers) {
            assert_eq!(l["mediaType"], DOCKER_LAYER);
            let blob = &blobs[&format!("team/app@{}", l["digest"].as_str().unwrap())];
            assert_eq!(l["size"].as_u64().unwrap(), blob.len() as u64);
            assert_eq!(format!("sha256:{}", sha256_hex(&gunzip(blob))), *d);
        }
        let out = lines.lock().unwrap().clone();
        assert_eq!(out[0], format!("The push refers to repository [localhost:{port}/team/app]"));
        assert!(out.iter().filter(|l| l.ends_with(" Pushed")).count() == 2, "{out:?}");
        assert_eq!(out.last().unwrap(), &format!("v1: digest: {} size: {}", pushed.digest, manifest.len()));
        // The name now carries the registry's digest.
        let key = Reference::parse(&name).unwrap().canonical();
        let now = store.entries().into_iter().find(|(k, _)| *k == key).unwrap().1;
        assert_eq!((now.top.as_str(), now.config.as_str(), now.loaded), (pushed.digest.as_str(), entry.config.as_str(), false));

        // Again: nothing is uploaded, nothing is even tarred.
        reg.log.lock().unwrap().clear();
        let (opts, lines) = recorder();
        assert_eq!(store.push(&name, &opts).await.unwrap().digest, pushed.digest);
        assert!(!reg.log.lock().unwrap().iter().any(|l| l.starts_with("POST")), "{:?}", reg.log.lock().unwrap());
        assert_eq!(lines.lock().unwrap().iter().filter(|l| l.ends_with("Layer already exists")).count(), 2);

        // Pulled back into another store: the same image, the same files.
        let other = ImageStore::open(&tmp.path().join("other")).unwrap();
        let image = other.pull(&name, false).await.unwrap();
        assert_eq!(image.id, format!("localhost:{port}/team/app@{}", pushed.digest));
        assert_eq!(other.entries()[0].1.config, entry.config);
        assert_eq!(std::fs::read_to_string(image.layers[1].join("app/run.sh")).unwrap(), "echo hi");
    }

    #[tokio::test]
    async fn pushes_a_pulled_image_by_mount_or_from_its_registry() {
        let src = Arc::new(Registry::default());
        let src_port = serve(src.clone()).await;
        let dst = Arc::new(Registry { mounts: true, ..Default::default() });
        let dst_port = serve(dst.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let store = ImageStore::open(&tmp.path().join("store")).unwrap();
        // An image that came from a registry, with the gzip blobs it pulled.
        let first = format!("localhost:{src_port}/base:1");
        built(&store, tmp.path(), &first);
        let pushed = store.push(&first, &PushOptions::default()).await.unwrap();
        let other = ImageStore::open(&tmp.path().join("other")).unwrap();
        other.pull(&first, false).await.unwrap();
        let src_manifest = src.manifests.lock().unwrap()["base:1"].1.clone();

        // To another registry: the blobs come from the first one, and the
        // manifest is the same bytes (the same digest).
        let target = format!("localhost:{dst_port}/copy/base:1");
        tag(&other, &first, &target);
        let (opts, lines) = recorder();
        let copied = other.push(&target, &opts).await.unwrap();
        assert_eq!(copied.digest, pushed.digest);
        assert_eq!(dst.manifests.lock().unwrap()["copy/base:1"].1, src_manifest);
        assert_eq!(lines.lock().unwrap().iter().filter(|l| l.ends_with(" Pushed")).count(), 2);

        // To another repository of that registry: mounted.
        let mounted = format!("localhost:{dst_port}/again:1");
        tag(&other, &target, &mounted);
        let (opts, lines) = recorder();
        assert_eq!(other.push(&mounted, &opts).await.unwrap().digest, pushed.digest);
        let out = lines.lock().unwrap().clone();
        assert_eq!(out.iter().filter(|l| l.ends_with("Mounted from copy/base")).count(), 2, "{out:?}");
    }

    #[tokio::test]
    async fn a_build_on_a_pulled_base_keeps_its_id() {
        let src = Arc::new(Registry { mounts: true, ..Default::default() });
        let src_port = serve(src.clone()).await;
        let dst = Arc::new(Registry::default());
        let dst_port = serve(dst.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let base = format!("localhost:{src_port}/base:1");
        let a = ImageStore::open(&tmp.path().join("a")).unwrap();
        built(&a, tmp.path(), &base);
        a.push(&base, &PushOptions::default()).await.unwrap();
        let store = ImageStore::open(&tmp.path().join("store")).unwrap();
        store.pull(&base, false).await.unwrap();
        // FROM base, one more layer.
        let base_entry = store.entries()[0].1.clone();
        let dir = tmp.path().join("run");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("out"), "built").unwrap();
        let top = layer_diff_id(&dir).unwrap();
        store.install_layer(&dir, &top).unwrap();
        let mut diff_ids = base_entry.layers.clone();
        diff_ids.push(top);
        let config = serde_json::to_vec(&json!({"architecture": "amd64", "os": "linux", "rootfs": {"type": "layers", "diff_ids": diff_ids}})).unwrap();
        let mounted = format!("localhost:{src_port}/app:v1");
        let copied = format!("localhost:{dst_port}/app:v1");
        let entry = store.register(&[mounted.clone(), copied.clone()].map(|n| Reference::parse(&n).unwrap().canonical()), &config).unwrap();

        // Same registry as the base: its layers are mounted.
        let (opts, lines) = recorder();
        store.push(&mounted, &opts).await.unwrap();
        let out = lines.lock().unwrap().clone();
        assert_eq!(out.iter().filter(|l| l.ends_with("Mounted from base")).count(), 2, "{out:?}");
        let m: Value = serde_json::from_slice(&src.manifests.lock().unwrap()["app:v1"].1).unwrap();
        assert_eq!(m["config"]["digest"], entry.config);
        // Another registry: fetched from the base's, pushed; same image ID.
        let (opts, lines) = recorder();
        store.push(&copied, &opts).await.unwrap();
        assert_eq!(lines.lock().unwrap().iter().filter(|l| l.ends_with(" Pushed")).count(), 3);
        let m: Value = serde_json::from_slice(&dst.manifests.lock().unwrap()["app:v1"].1).unwrap();
        assert_eq!(m["config"]["digest"], entry.config);
    }

    fn tag(store: &ImageStore, from: &str, to: &str) {
        let e = store.entries().into_iter().find(|(k, _)| *k == Reference::parse(from).unwrap().canonical()).unwrap().1;
        store.set_key(&Reference::parse(to).unwrap().canonical(), Some(e)).unwrap();
    }

    #[tokio::test]
    async fn token_auth_asks_for_push() {
        let reg = Arc::new(Registry { token: true, ..Default::default() });
        let port = serve(reg.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let store = ImageStore::open(&tmp.path().join("store")).unwrap();
        let name = format!("localhost:{port}/app:v1");
        built(&store, tmp.path(), &name);
        let e = format!("{:#}", store.push(&name, &PushOptions::default()).await.unwrap_err());
        assert!(e.to_lowercase().contains("unauthorized"), "{e}");
        let opts = PushOptions { auth: Some(Credentials::Basic { username: "user".into(), password: "s3cret".into() }), ..Default::default() };
        store.push(&name, &opts).await.unwrap();
        assert!(reg.manifests.lock().unwrap().contains_key("app:v1"));
        let e = format!("{:#}", store.push(&format!("localhost:{port}/app:nope"), &opts).await.unwrap_err());
        assert!(e.contains("does not exist locally"), "{e}");
    }

    #[tokio::test]
    async fn a_layer_whose_tar_differs_gets_a_new_diff_id() {
        let reg = Arc::new(Registry::default());
        let port = serve(reg.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let store = ImageStore::open(&tmp.path().join("store")).unwrap();
        // A layer unpacked from a tar another writer made (its diff ID is
        // not what tarring the directory here gives), with nowhere to fetch
        // the original blob from.
        let dir = tmp.path().join("l");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f"), "x").unwrap();
        let theirs = format!("sha256:{}", "e".repeat(64));
        store.install_layer(&dir, &theirs).unwrap();
        let config = serde_json::to_vec(&json!({"architecture": "amd64", "os": "linux", "rootfs": {"type": "layers", "diff_ids": [theirs]}})).unwrap();
        let name = format!("localhost:{port}/app:v1");
        let entry = store.register(&[Reference::parse(&name).unwrap().canonical()], &config).unwrap();
        let pushed = store.push(&name, &PushOptions::default()).await.unwrap();
        let m: Value = serde_json::from_slice(&reg.manifests.lock().unwrap()["app:v1"].1).unwrap();
        let blobs = reg.blobs.lock().unwrap().clone();
        let cfg: Value = serde_json::from_slice(&blobs[&format!("app@{}", m["config"]["digest"].as_str().unwrap())]).unwrap();
        let layer = &blobs[&format!("app@{}", m["layers"][0]["digest"].as_str().unwrap())];
        assert_eq!(cfg["rootfs"]["diff_ids"][0].as_str().unwrap(), format!("sha256:{}", sha256_hex(&gunzip(layer))));
        assert_ne!(m["config"]["digest"], entry.config);
        // The local image keeps its ID; pushing again reuses the blob made.
        let key = Reference::parse(&name).unwrap().canonical();
        assert!(store.entries().into_iter().any(|(k, e)| k == key && e.manifest == entry.manifest));
        reg.log.lock().unwrap().clear();
        assert_eq!(store.push(&name, &PushOptions::default()).await.unwrap().digest, pushed.digest);
        assert!(!reg.log.lock().unwrap().iter().any(|l| l.starts_with("POST")));
    }

    #[test]
    fn registry_errors_read_like_dockers() {
        let e = registry_error("x", hyper::StatusCode::FORBIDDEN, br#"{"errors":[{"code":"DENIED","message":"requested access to the resource is denied"}]}"#);
        assert_eq!(e.to_string(), "denied: requested access to the resource is denied");
        assert_eq!(registry_error("x", hyper::StatusCode::UNAUTHORIZED, b"").to_string(), "unauthorized: authentication required");
    }
}
