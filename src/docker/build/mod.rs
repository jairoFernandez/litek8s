//! `POST /build`: Docker's classic builder API (what `docker build` and
//! `docker compose build` use with `DOCKER_BUILDKIT=0`), with a builder of
//! our own on the crun runtime's pieces:
//!
//! - every `RUN` is a crun container on an overlay of the image's layers;
//!   its upper directory, already in the layer store's format, becomes the
//!   new layer (no tar round trip);
//! - `COPY`/`ADD` write through an overlay mount the same way, so existing
//!   directories, symlinks and `--chown` names resolve as in the image;
//! - steps are cached by a key chained over the instructions, build
//!   arguments and the content of copied files (`build-cache/<key>` entries
//!   in the image store keep their layers);
//! - multi-stage builds build only the stages the target needs; BuildKit's
//!   Dockerfile syntax (`RUN --mount=type=cache|bind|tmpfs`, `COPY --chmod`,
//!   `--link`, heredocs, `TARGETARCH`...) works too.

pub mod dockerfile;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use hyper::Request;
use hyper::body::{Bytes, Incoming};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use self::dockerfile::{Command, Dockerfile, Instruction, Stage, expand, key_values, parse_mount, shell_words};
use super::api::Body;
use super::engine::{Engine, secure_join};
use super::error::{ApiError, ApiResult};
use super::images;
use crate::kube::agent::crun::image::{self as cimage, IndexEntry, PullOptions, random_hex};
use crate::kube::agent::crun::{spec as cspec, sys};
use crate::kube::agent::runtime::ContainerSpec;

const CACHE_PREFIX: &str = images::BUILD_CACHE_PREFIX;

fn sha(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    hex::encode(h.finalize())
}

/// Every value of a repeated query parameter (`t=a&t=b`).
fn query_all(req: &Request<Incoming>, key: &str) -> Vec<String> {
    req.uri()
        .query()
        .unwrap_or_default()
        .split('&')
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            (super::api::decode(k) == key).then(|| super::api::decode(v))
        })
        .filter(|v| !v.is_empty())
        .collect()
}

pub struct Options {
    pub tags: Vec<String>,
    pub dockerfile: String,
    pub build_args: HashMap<String, Option<String>>,
    pub target: Option<String>,
    pub no_cache: bool,
    pub pull: bool,
    pub labels: BTreeMap<String, String>,
    pub platform: (String, Option<String>),
    pub quiet: bool,
    pub extra_hosts: Vec<String>,
}

pub async fn handle(engine: Arc<Engine>, q: HashMap<String, String>, req: Request<Incoming>) -> ApiResult<hyper::Response<Body>> {
    if q.get("remote").is_some_and(|r| !r.is_empty()) {
        return Err(ApiError::new(501, "building from a remote context is not supported; send the context"));
    }
    let json_map = |k: &str| -> serde_json::Map<String, Value> { q.get(k).and_then(|v| serde_json::from_str::<Value>(v).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default() };
    let host = cimage::host_platform();
    let opts = Options {
        tags: query_all(&req, "t"),
        dockerfile: q.get("dockerfile").cloned().filter(|d| !d.is_empty()).unwrap_or_else(|| "Dockerfile".into()),
        build_args: json_map("buildargs").into_iter().map(|(k, v)| (k, v.as_str().map(str::to_string))).collect(),
        target: q.get("target").cloned().filter(|t| !t.is_empty()),
        no_cache: q.get("nocache").is_some_and(|v| v == "1" || v == "true"),
        pull: q.get("pull").is_some_and(|v| !v.is_empty() && v != "0" && v != "false"),
        labels: json_map("labels").into_iter().map(|(k, v)| (k, v.as_str().unwrap_or_default().to_string())).collect(),
        platform: q.get("platform").and_then(|p| cimage::parse_platform(p)).unwrap_or((host.0.to_string(), host.1.map(str::to_string))),
        quiet: q.get("q").is_some_and(|v| v == "1" || v == "true"),
        extra_hosts: q.get("extrahosts").map(|h| h.split(',').map(str::to_string).collect()).unwrap_or_default(),
    };
    let dir = engine.root.join("build").join(random_hex(8));
    std::fs::create_dir_all(&dir)?;
    // The context, staged on disk.
    let tar_path = dir.join("context.tar");
    let mut file = tokio::fs::File::create(&tar_path).await?;
    let mut body = req.into_body();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| ApiError::bad_request(e.to_string()))?;
        if let Ok(data) = frame.into_data() {
            file.write_all(&data).await?;
        }
    }
    file.flush().await?;
    drop(file);
    let (tx, rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let quiet = opts.quiet;
        let result = run(engine.clone(), &dir, opts, tx.clone()).await;
        match result {
            Ok(id) => {
                if quiet {
                    let _ = tx.send(json!({"stream": format!("{id}\n")}));
                }
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let _ = tx.send(json!({"errorDetail": {"message": msg}, "error": msg}));
            }
        }
        let d = dir.clone();
        let _ = tokio::task::spawn_blocking(move || remove_build_dir(&d)).await;
    });
    let s = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (Bytes::from(format!("{v}\r\n")), rx)) });
    let body = http_body_util::StreamBody::new(s.map(|b| Ok::<_, std::io::Error>(hyper::body::Frame::data(b))));
    Ok(hyper::Response::builder()
        .status(200)
        .header("Content-Type", "application/json")
        .header("Api-Version", super::api::API_VERSION)
        .body(BodyExt::boxed_unsync(body))
        .unwrap())
}

fn remove_build_dir(dir: &Path) {
    // Mounts left by a failed step first.
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            sys::unmount(&e.path().join("rootfs"));
            sys::unmount(&e.path().join("src"));
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

pub fn prune(engine: &Engine) -> Value {
    let mut deleted = Vec::new();
    let mut space = 0u64;
    for (key, entry) in engine.images.entries() {
        if key.starts_with(CACHE_PREFIX) {
            space += entry.layers.iter().map(|l| engine.images.layer_size(l)).sum::<u64>();
            let _ = engine.images.set_key(&key, None);
            deleted.push(key.trim_start_matches(CACHE_PREFIX).to_string());
        }
    }
    let _ = std::fs::remove_dir_all(engine.root.join("build-cache-mounts"));
    json!({"CachesDeleted": deleted, "SpaceReclaimed": space})
}

/// The image being built in one stage.
#[derive(Clone)]
struct State {
    /// The full image config (`architecture`, `config`, `rootfs`, `history`).
    image: Value,
    layers: Vec<String>,
    key: String,
    /// ARG values in scope (not persisted in the image).
    args: BTreeMap<String, String>,
    created: String,
}

impl State {
    fn config(&mut self) -> &mut Value {
        if !self.image["config"].is_object() {
            self.image["config"] = json!({});
        }
        &mut self.image["config"]
    }

    fn env(&self) -> Vec<(String, String)> {
        self.image["config"]["Env"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e.as_str())
            .map(|e| {
                let (k, v) = e.split_once('=').unwrap_or((e, ""));
                (k.to_string(), v.to_string())
            })
            .collect()
    }

    fn lookup(&self, name: &str) -> Option<String> {
        self.env().into_iter().rev().find(|(k, _)| k == name).map(|(_, v)| v).or_else(|| self.args.get(name).cloned())
    }

    fn set_env(&mut self, key: &str, value: &str) {
        let mut env: Vec<String> = self.image["config"]["Env"].as_array().cloned().unwrap_or_default().into_iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
        env.retain(|e| e.split('=').next() != Some(key));
        env.push(format!("{key}={value}"));
        self.config()["Env"] = json!(env);
    }

    fn history(&mut self, created_by: &str, empty: bool) {
        let mut h = json!({"created": self.created, "created_by": created_by, "comment": "litek8s build"});
        if empty {
            h["empty_layer"] = json!(true);
        }
        if !self.image["history"].is_array() {
            self.image["history"] = json!([]);
        }
        self.image["history"].as_array_mut().unwrap().push(h);
    }

    fn workdir(&self) -> String {
        self.image["config"]["WorkingDir"].as_str().filter(|w| !w.is_empty()).unwrap_or("/").to_string()
    }

    fn shell(&self) -> Vec<String> {
        let s: Vec<String> = self.image["config"]["Shell"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect();
        if s.is_empty() { vec!["/bin/sh".into(), "-c".into()] } else { s }
    }
}

struct Builder {
    engine: Arc<Engine>,
    dir: PathBuf,
    context: PathBuf,
    df: Dockerfile,
    opts: Options,
    tx: mpsc::UnboundedSender<Value>,
    step: usize,
    total: usize,
    used_args: HashSet<String>,
    global_args: BTreeMap<String, String>,
    done: HashMap<usize, State>,
}

async fn run(engine: Arc<Engine>, dir: &Path, opts: Options, tx: mpsc::UnboundedSender<Value>) -> Result<String> {
    let _busy = engine.images.busy();
    let context = dir.join("context");
    let tar = dir.join("context.tar");
    let ctx = context.clone();
    tokio::task::spawn_blocking(move || extract_context(&tar, &ctx)).await??;
    let _ = std::fs::remove_file(dir.join("context.tar"));
    let df_path = secure_join(&context, &opts.dockerfile);
    let text = std::fs::read_to_string(&df_path)
        .or_else(|_| std::fs::read_to_string(context.join(opts.dockerfile.to_lowercase())))
        .with_context(|| format!("Cannot locate specified Dockerfile: {}", opts.dockerfile))?;
    let df = dockerfile::parse(&text)?;
    let mut b = Builder {
        engine,
        dir: dir.to_path_buf(),
        context,
        df,
        opts,
        tx,
        step: 0,
        total: 0,
        used_args: HashSet::new(),
        global_args: BTreeMap::new(),
        done: HashMap::new(),
    };
    b.build().await
}

fn extract_context(tar_path: &Path, dest: &Path) -> Result<()> {
    use std::io::Read;
    std::fs::create_dir_all(dest)?;
    let mut f = std::fs::File::open(tar_path)?;
    let mut magic = [0u8; 2];
    let n = f.read(&mut magic)?;
    if n == 0 {
        return Ok(());
    }
    let raw = std::io::Cursor::new(magic[..n].to_vec()).chain(f);
    let reader: Box<dyn Read> = if magic == [0x1f, 0x8b] { Box::new(flate2::read::MultiGzDecoder::new(raw)) } else { Box::new(raw) };
    // The CLI may end the tar right after the last file's data (when the
    // Dockerfile is outside the context): zeros complete the last block and
    // add the end-of-archive marker.
    let reader = reader.chain(std::io::repeat(0).take(1536));
    let mut a = tar::Archive::new(reader);
    a.set_preserve_permissions(true);
    a.set_preserve_mtime(true);
    a.set_unpack_xattrs(false);
    a.unpack(dest).context("reading the build context")?;
    Ok(())
}

impl Builder {
    fn say(&self, text: impl Into<String>) {
        if !self.opts.quiet {
            let _ = self.tx.send(json!({"stream": text.into()}));
        }
    }

    fn platform_args(&self) -> BTreeMap<String, String> {
        let (arch, variant) = &self.opts.platform;
        let host = cimage::host_platform();
        let plat = |a: &str, v: Option<&str>| match v {
            Some(v) => format!("linux/{a}/{v}"),
            None => format!("linux/{a}"),
        };
        BTreeMap::from([
            ("TARGETPLATFORM".to_string(), plat(arch, variant.as_deref())),
            ("TARGETOS".to_string(), "linux".to_string()),
            ("TARGETARCH".to_string(), arch.clone()),
            ("TARGETVARIANT".to_string(), variant.clone().unwrap_or_default()),
            ("BUILDPLATFORM".to_string(), plat(host.0, host.1)),
            ("BUILDOS".to_string(), "linux".to_string()),
            ("BUILDARCH".to_string(), host.0.to_string()),
            ("BUILDVARIANT".to_string(), host.1.unwrap_or_default().to_string()),
        ])
    }

    /// Stages the target needs, in order.
    fn needed_stages(&self, target: usize) -> Result<Vec<usize>> {
        let mut needed = HashSet::new();
        let mut todo = vec![target];
        let globals = |s: &str| expand(s, self.df.escape, &|n| self.global_args.get(n).cloned());
        while let Some(i) = todo.pop() {
            if !needed.insert(i) {
                continue;
            }
            let st = &self.df.stages[i];
            let mut deps = vec![globals(&st.from)];
            for ins in &st.instructions {
                if let Some(f) = ins.flag("from") {
                    deps.push(f.to_string());
                }
                for m in ins.flags_named("mount") {
                    if let Ok(m) = parse_mount(m) {
                        if let Some(f) = m.from {
                            deps.push(f);
                        }
                    }
                }
            }
            for d in deps {
                if let Some(j) = self.df.stage_index(&d).filter(|j| *j < i) {
                    todo.push(j);
                }
            }
        }
        let mut v: Vec<usize> = needed.into_iter().collect();
        v.sort();
        Ok(v)
    }

    async fn build(&mut self) -> Result<String> {
        // Global ARGs: defaults, overridden by --build-arg.
        let platform = self.platform_args();
        for ins in self.df.global_args.clone() {
            for (name, default) in arg_decls(&ins.args) {
                let value = match self.opts.build_args.get(&name) {
                    Some(Some(v)) => Some(v.clone()),
                    _ => default.map(|d| expand(&d, self.df.escape, &|n| self.global_args.get(n).cloned().or_else(|| platform.get(n).cloned()))),
                };
                self.used_args.insert(name.clone());
                if let Some(v) = value {
                    self.global_args.insert(name, v);
                }
            }
        }
        for (k, v) in &platform {
            self.global_args.entry(k.clone()).or_insert(v.clone());
        }
        let target = match &self.opts.target {
            Some(t) => self.df.stage_index(t).with_context(|| format!("failed to reach build target {t} in Dockerfile"))?,
            None => self.df.stages.len() - 1,
        };
        let stages = self.needed_stages(target)?;
        self.total = stages.iter().map(|i| self.df.stages[*i].instructions.len() + 1).sum();
        let mut last = None;
        for i in stages {
            let stage = self.df.stages[i].clone();
            let state = self.build_stage(&stage).await?;
            self.done.insert(i, state.clone());
            last = Some(state);
        }
        let mut state = last.context("nothing to build")?;
        for (k, v) in &self.opts.labels {
            let labels = state.config()["Labels"].as_object().cloned().unwrap_or_default();
            let mut labels = labels;
            labels.insert(k.clone(), json!(v));
            state.config()["Labels"] = Value::Object(labels);
        }
        let unused: Vec<&String> = self.opts.build_args.keys().filter(|k| !self.used_args.contains(*k) && !self.platform_args().contains_key(*k)).collect();
        if !unused.is_empty() {
            let list: Vec<String> = unused.iter().map(|s| s.to_string()).collect();
            self.say(format!("[Warning] One or more build-args [{}] were not consumed\n", list.join(" ")));
        }
        // The image.
        let (arch, variant) = self.opts.platform.clone();
        state.image["architecture"] = json!(arch);
        if let Some(v) = variant {
            state.image["variant"] = json!(v);
        }
        state.image["os"] = json!("linux");
        state.image["created"] = json!(state.created);
        state.image["rootfs"] = json!({"type": "layers", "diff_ids": state.layers});
        let bytes = serde_json::to_vec(&state.image)?;
        let names: Vec<String> = self.opts.tags.iter().filter_map(|t| images::key_for(t)).collect();
        if names.len() != self.opts.tags.len() {
            bail!("invalid tag in {:?}", self.opts.tags);
        }
        // Earlier holders of these names keep their image under its ID if a
        // container uses it.
        let users: Vec<String> = self.engine.list().iter().map(|c| c.record().image_id).collect();
        for n in &names {
            if let Some(old) = images::resolve(&self.engine.images, n) {
                if users.contains(&old.id) && old.keys.len() == 1 {
                    let mut e = old.entry.clone();
                    e.pinned = true;
                    self.engine.images.set_key(&old.id, Some(e))?;
                }
            }
        }
        let entry = self.engine.images.register(&names, &bytes)?;
        let id = entry.config.clone();
        self.say(format!(" ---> {}\n", short(&id)));
        let _ = self.tx.send(json!({"aux": {"ID": id}}));
        self.say(format!("Successfully built {}\n", short(&id)));
        for t in &self.opts.tags {
            let shown = images::key_for(t).and_then(|k| images::short_name(&k)).unwrap_or(t.clone());
            self.say(format!("Successfully tagged {shown}\n"));
            self.engine.emit("image", "tag", &id, BTreeMap::from([("name".to_string(), shown)]));
        }
        Ok(id)
    }

    fn step_header(&mut self, ins: &Instruction) {
        self.step += 1;
        let text = ins.original.lines().map(str::trim).collect::<Vec<_>>().join(" ");
        self.say(format!("Step {}/{} : {}\n", self.step, self.total, text));
    }

    async fn base_state(&mut self, stage: &Stage) -> Result<State> {
        let from = expand(&stage.from, self.df.escape, &|n| self.global_args.get(n).cloned());
        let platform = stage
            .platform
            .as_ref()
            .map(|p| expand(p, self.df.escape, &|n| self.global_args.get(n).cloned()))
            .and_then(|p| cimage::parse_platform(&p))
            .unwrap_or(self.opts.platform.clone());
        if let Some(i) = self.df.stage_index(&from).filter(|i| *i < stage.index) {
            let mut s = self.done.get(&i).cloned().with_context(|| format!("stage {from} was not built"))?;
            s.args.clear();
            self.say(format!(" ---> {}\n", short(&s.key)));
            return Ok(s);
        }
        if from == "scratch" {
            let key = sha(&["FROM scratch"]);
            return Ok(State { image: json!({"config": {"Env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"]}, "history": []}), layers: Vec::new(), key, args: BTreeMap::new(), created: super::model::now_rfc3339() });
        }
        let host = cimage::host_platform();
        let wants_other_platform = platform.0 != host.0;
        let mut info = images::resolve(&self.engine.images, &from);
        let arch_ok = |info: &images::ImageInfo| self.engine.images.config_blob(&info.entry.config).and_then(|c| c["architecture"].as_str().map(|a| a == platform.0)).unwrap_or(true);
        if self.opts.pull || info.as_ref().is_none_or(|i| !arch_ok(i)) {
            let tx = self.tx.clone();
            let opts = PullOptions {
                always: self.opts.pull,
                platform: wants_other_platform.then(|| platform.clone()).or(Some(platform.clone())),
                auth: None,
                pinned: true,
                progress: Some(Arc::new(move |id: &str, msg: &str| {
                    let v = if id.is_empty() { json!({"status": msg}) } else { json!({"status": msg, "id": id, "progressDetail": {}}) };
                    let _ = tx.send(v);
                })),
            };
            self.engine.images.pull_with(&from, &opts).await.with_context(|| format!("pulling {from}"))?;
            info = images::resolve(&self.engine.images, &from);
        }
        let info = info.with_context(|| format!("No such image: {from}"))?;
        let config = self.engine.images.config_blob(&info.entry.config).context("image config missing")?;
        let created = config["created"].as_str().unwrap_or(super::model::ZERO_TIME).to_string();
        let mut image = json!({"config": config.get("config").cloned().unwrap_or(json!({})), "history": config.get("history").cloned().unwrap_or(json!([]))});
        if image["config"].is_null() {
            image["config"] = json!({});
        }
        self.say(format!(" ---> {}\n", short(&info.id)));
        Ok(State { image, layers: info.entry.layers.clone(), key: sha(&["FROM", &info.id]), args: BTreeMap::new(), created })
    }

    async fn build_stage(&mut self, stage: &Stage) -> Result<State> {
        self.step_header(&stage.from_line.clone());
        let mut st = self.base_state(stage).await?;
        for ins in stage.instructions.clone() {
            self.step_header(&ins);
            self.instruction(&mut st, &ins).await.with_context(|| format!("{} (Dockerfile line {})", ins.original.lines().next().unwrap_or_default(), ins.line))?;
        }
        Ok(st)
    }

    fn expand_with(&self, st: &State, s: &str) -> String {
        expand(s, self.df.escape, &|n| st.lookup(n))
    }

    async fn instruction(&mut self, st: &mut State, ins: &Instruction) -> Result<()> {
        let kw = ins.keyword.as_str();
        let created_by = |s: &str| format!("/bin/sh -c #(nop) {s}");
        match kw {
            "ARG" => {
                let platform = self.platform_args();
                for (name, default) in arg_decls(&ins.args) {
                    self.used_args.insert(name.clone());
                    let value = match self.opts.build_args.get(&name) {
                        Some(Some(v)) => Some(v.clone()),
                        Some(None) => std::env::var(&name).ok(),
                        None => default
                            .map(|d| self.expand_with(st, &d))
                            .or_else(|| self.global_args.get(&name).cloned())
                            .or_else(|| platform.get(&name).cloned()),
                    };
                    if let Some(v) = value {
                        st.args.insert(name.clone(), v.clone());
                        st.key = sha(&[&st.key, "ARG", &name, &v]);
                    }
                }
                st.history(&created_by(&format!("ARG {}", ins.args)), true);
            }
            "ENV" => {
                for (k, v) in key_values(&ins.args)? {
                    let v = self.expand_with(st, &v);
                    st.set_env(&k, &v);
                    st.key = sha(&[&st.key, "ENV", &k, &v]);
                }
                st.history(&created_by(&format!("ENV {}", ins.args)), true);
            }
            "LABEL" => {
                let mut labels = st.image["config"]["Labels"].as_object().cloned().unwrap_or_default();
                for (k, v) in key_values(&ins.args)? {
                    let k = self.expand_with(st, &k);
                    let v = self.expand_with(st, &v);
                    st.key = sha(&[&st.key, "LABEL", &k, &v]);
                    labels.insert(k, json!(v));
                }
                st.config()["Labels"] = Value::Object(labels);
                st.history(&created_by(&format!("LABEL {}", ins.args)), true);
            }
            "WORKDIR" => {
                let w = self.expand_with(st, ins.args.trim());
                let w = if w.starts_with('/') { w } else { format!("{}/{}", st.workdir().trim_end_matches('/'), w) };
                st.config()["WorkingDir"] = json!(w);
                st.key = sha(&[&st.key, "WORKDIR", &w]);
                // The directory exists in the image (a layer only if new).
                self.layer_step(st, &format!("WORKDIR {w}"), StepKind::Mkdir(w.clone())).await?;
            }
            "USER" => {
                let u = self.expand_with(st, ins.args.trim());
                st.config()["User"] = json!(u);
                st.key = sha(&[&st.key, "USER", &u]);
                st.history(&created_by(&format!("USER {u}")), true);
            }
            "EXPOSE" => {
                let mut ports = st.image["config"]["ExposedPorts"].as_object().cloned().unwrap_or_default();
                for p in self.expand_with(st, &ins.args).split_whitespace() {
                    let p = if p.contains('/') { p.to_lowercase() } else { format!("{p}/tcp") };
                    ports.insert(p, json!({}));
                }
                st.config()["ExposedPorts"] = Value::Object(ports);
                st.key = sha(&[&st.key, "EXPOSE", &ins.args]);
                st.history(&created_by(&format!("EXPOSE {}", ins.args)), true);
            }
            "VOLUME" => {
                let mut vols = st.image["config"]["Volumes"].as_object().cloned().unwrap_or_default();
                let list = dockerfile::parse_json_array(&ins.args).unwrap_or_else(|| ins.args.split_whitespace().map(str::to_string).collect());
                for v in list {
                    vols.insert(self.expand_with(st, &v), json!({}));
                }
                st.config()["Volumes"] = Value::Object(vols);
                st.key = sha(&[&st.key, "VOLUME", &ins.args]);
                st.history(&created_by(&format!("VOLUME {}", ins.args)), true);
            }
            "CMD" | "ENTRYPOINT" => {
                let argv = match ins.command() {
                    Command::Exec(v) => v,
                    Command::Shell(s) => {
                        let mut v = st.shell();
                        v.push(s);
                        v
                    }
                };
                let field = if kw == "CMD" { "Cmd" } else { "Entrypoint" };
                st.config()[field] = json!(argv);
                if kw == "ENTRYPOINT" && st.image["config"]["Cmd"].is_array() {
                    // Docker drops an inherited CMD when ENTRYPOINT is set.
                    st.config()["Cmd"] = Value::Null;
                }
                st.key = sha(&[&st.key, kw, &serde_json::to_string(&argv)?]);
                st.history(&created_by(&format!("{kw} {}", ins.args)), true);
            }
            "SHELL" => {
                let argv = dockerfile::parse_json_array(&ins.args).context("SHELL requires the JSON form")?;
                st.config()["Shell"] = json!(argv);
                st.key = sha(&[&st.key, "SHELL", &ins.args]);
                st.history(&created_by(&format!("SHELL {}", ins.args)), true);
            }
            "STOPSIGNAL" => {
                let s = self.expand_with(st, ins.args.trim());
                st.config()["StopSignal"] = json!(s);
                st.history(&created_by(&format!("STOPSIGNAL {s}")), true);
            }
            "HEALTHCHECK" => {
                let hc = parse_healthcheck(ins, &st.shell())?;
                st.config()["Healthcheck"] = hc.clone();
                st.key = sha(&[&st.key, "HEALTHCHECK", &hc.to_string()]);
                st.history(&created_by(&format!("HEALTHCHECK {}", ins.args)), true);
            }
            "ONBUILD" => {
                let mut list: Vec<Value> = st.image["config"]["OnBuild"].as_array().cloned().unwrap_or_default();
                list.push(json!(ins.args));
                st.config()["OnBuild"] = json!(list);
                st.history(&created_by(&format!("ONBUILD {}", ins.args)), true);
            }
            "MAINTAINER" => {
                st.image["author"] = json!(ins.args);
                st.history(&created_by(&format!("MAINTAINER {}", ins.args)), true);
            }
            "RUN" => self.run_step(st, ins).await?,
            "COPY" | "ADD" => self.copy_step(st, ins).await?,
            other => bail!("unknown instruction: {other}"),
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Layers

    fn cache_get(&self, key: &str) -> Option<IndexEntry> {
        if self.opts.no_cache {
            return None;
        }
        let (_, e) = self.engine.images.entries().into_iter().find(|(k, _)| k == &format!("{CACHE_PREFIX}{key}"))?;
        e.layers.iter().all(|l| self.engine.images.layer_dir(l).is_dir()).then_some(e)
    }

    fn cache_put(&self, key: &str, layer: Option<&str>, created: &str) -> Result<()> {
        let entry = IndexEntry {
            top: created.to_string(),
            manifest: format!("sha256:{}", "0".repeat(64)),
            config: format!("sha256:{}", "0".repeat(64)),
            layers: layer.map(|l| vec![l.to_string()]).unwrap_or_default(),
            last_used: chrono::Utc::now().timestamp(),
            loaded: true,
            pinned: true,
        };
        self.engine.images.set_key(&format!("{CACHE_PREFIX}{key}"), Some(entry))
    }

    /// A step that may produce a layer: from the cache, or by running it.
    async fn layer_step(&mut self, st: &mut State, created_by: &str, kind: StepKind) -> Result<()> {
        if let Some(e) = self.cache_get(&st.key) {
            self.say(" ---> Using cache\n");
            st.created = if e.top.is_empty() { st.created.clone() } else { e.top.clone() };
            let empty = e.layers.is_empty();
            st.layers.extend(e.layers);
            st.history(created_by, empty);
            self.say(format!(" ---> {}\n", short(&st.key)));
            return Ok(());
        }
        let work = self.dir.join(format!("step-{}", random_hex(4)));
        for d in ["upper", "work", "rootfs"] {
            std::fs::create_dir_all(work.join(d))?;
        }
        let lowers: Vec<PathBuf> = if st.layers.is_empty() {
            let empty = work.join("empty");
            std::fs::create_dir_all(&empty)?;
            vec![empty]
        } else {
            st.layers.iter().rev().map(|l| self.engine.images.layer_dir(l)).collect()
        };
        let mut options = sys::overlay_options(&lowers, &work.join("upper"), &work.join("work"))?;
        if sys::in_user_namespace() {
            options.push_str(",userxattr");
        }
        let rootfs = work.join("rootfs");
        sys::mount_overlay(&rootfs, &options)?;
        let result = match &kind {
            StepKind::Mkdir(dir) => {
                let p = secure_join(&rootfs, dir);
                if !p.exists() {
                    std::fs::create_dir_all(&p)?;
                }
                Ok(())
            }
            StepKind::Run(plan) => self.exec_run(st, &work, plan).await,
            StepKind::Copy(plan) => self.exec_copy(st, &rootfs, plan).await,
        };
        sys::unmount(&rootfs);
        result?;
        let upper = work.join("upper");
        let empty = std::fs::read_dir(&upper)?.next().is_none();
        let layer = if empty {
            None
        } else {
            let u = upper.clone();
            let diff_id = tokio::task::spawn_blocking(move || cimage::layer_diff_id(&u)).await??;
            self.engine.images.install_layer(&upper, &diff_id)?;
            Some(diff_id)
        };
        let _ = std::fs::remove_dir_all(&work);
        st.created = super::model::now_rfc3339();
        self.cache_put(&st.key, layer.as_deref(), &st.created)?;
        if let Some(l) = &layer {
            st.layers.push(l.clone());
        }
        st.history(created_by, layer.is_none());
        self.say(format!(" ---> {}\n", short(&st.key)));
        Ok(())
    }

    async fn run_step(&mut self, st: &mut State, ins: &Instruction) -> Result<()> {
        let mut argv = match ins.command() {
            Command::Exec(v) => v,
            Command::Shell(s) => {
                let mut v = st.shell();
                v.push(s);
                v
            }
        };
        let mut files = Vec::new();
        if let Some((_, content)) = ins.heredocs.first() {
            // `RUN <<EOF`: the heredoc is the script (its own interpreter
            // with a #! line).
            if content.starts_with("#!") {
                files.push(("/dev/pipes/script".to_string(), content.clone()));
                argv = vec!["/dev/pipes/script".into()];
            } else {
                let mut v = st.shell();
                v.push(content.clone());
                argv = v;
            }
        }
        let mounts: Vec<dockerfile::RunMount> = ins.flags_named("mount").iter().map(|m| parse_mount(&self.expand_with(st, m))).collect::<Result<_>>()?;
        // Mount sources that change the result go into the key.
        let mut mount_keys = Vec::new();
        for m in &mounts {
            if m.kind == "bind" {
                let src = m.source.clone().unwrap_or_default();
                let key = match &m.from {
                    Some(f) => format!("{}:{src}", self.from_key(f).await?),
                    None => hash_paths(&self.context, &[src])?,
                };
                mount_keys.push(key);
            }
        }
        let args_env: Vec<String> = st.args.iter().map(|(k, v)| format!("{k}={v}")).collect();
        st.key = sha(&[&st.key, "RUN", &serde_json::to_string(&argv)?, &args_env.join("\n"), &format!("{mounts:?}"), &mount_keys.join("\n"), &files.iter().map(|f| f.1.clone()).collect::<Vec<_>>().join("")]);
        let created_by = format!("RUN {}", ins.args.lines().next().unwrap_or_default());
        self.layer_step(st, &created_by, StepKind::Run(RunPlan { argv, mounts, files })).await
    }

    /// The cache key of a `--from` source (a stage or an image).
    async fn from_key(&mut self, from: &str) -> Result<String> {
        if let Some(i) = self.df.stage_index(from) {
            return Ok(self.done.get(&i).map(|s| s.key.clone()).unwrap_or_default());
        }
        let root = self.source_root(from).await?;
        Ok(root.1)
    }

    /// The root filesystem of a `--from` source, mounted read-only:
    /// (path, key).
    async fn source_root(&mut self, from: &str) -> Result<(PathBuf, String)> {
        let (layers, key) = match self.df.stage_index(from) {
            Some(i) => {
                let s = self.done.get(&i).with_context(|| format!("stage {from} is not built"))?;
                (s.layers.clone(), s.key.clone())
            }
            None => {
                let mut info = images::resolve(&self.engine.images, from);
                if info.is_none() {
                    self.engine.images.pull_with(from, &PullOptions { pinned: true, platform: Some(self.opts.platform.clone()), ..Default::default() }).await?;
                    info = images::resolve(&self.engine.images, from);
                }
                let info = info.with_context(|| format!("No such image: {from}"))?;
                (info.entry.layers.clone(), info.id.clone())
            }
        };
        let dir = self.dir.join(format!("src-{}", &sha(&[&key])[..12]));
        let root = dir.join("src");
        if root.is_dir() && std::fs::read_dir(&root).map(|mut d| d.next().is_some()).unwrap_or(false) {
            return Ok((root, key));
        }
        std::fs::create_dir_all(&root)?;
        match layers.len() {
            0 => {}
            1 => {
                let src = self.engine.images.layer_dir(&layers[0]);
                bind_ro(&src, &root)?;
            }
            _ => {
                let lower: Vec<String> = layers.iter().rev().map(|l| self.engine.images.layer_dir(l).to_string_lossy().to_string()).collect();
                let mut opts = format!("lowerdir={}", lower.join(":"));
                if sys::in_user_namespace() {
                    opts.push_str(",userxattr");
                }
                sys::mount_overlay(&root, &opts)?;
            }
        }
        Ok((root, key))
    }

    async fn exec_run(&mut self, st: &State, work: &Path, plan: &RunPlan) -> Result<()> {
        let rootfs = work.join("rootfs");
        let etc = |name: &str| std::fs::read_to_string(secure_join(&rootfs, &format!("/etc/{name}"))).unwrap_or_default();
        let user_str = st.image["config"]["User"].as_str().unwrap_or_default().to_string();
        let user = cspec::resolve_user(&user_str, &etc("passwd"), &etc("group"))?;
        let wd = st.workdir();
        let _ = std::fs::create_dir_all(secure_join(&rootfs, &wd));
        // /etc files.
        let resolv = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
        std::fs::write(work.join("resolv.conf"), resolv)?;
        let mut hosts = String::from("127.0.0.1\tlocalhost buildkitsandbox\n::1\tlocalhost ip6-localhost ip6-loopback\n");
        for h in &self.opts.extra_hosts {
            if let Some((name, ip)) = h.split_once(':') {
                hosts.push_str(&format!("{ip}\t{name}\n"));
            }
        }
        std::fs::write(work.join("hosts"), hosts)?;
        std::fs::write(work.join("hostname"), "buildkitsandbox\n")?;
        // Mounts.
        let mut resolved = Vec::new();
        for (i, m) in plan.mounts.iter().enumerate() {
            match m.kind.as_str() {
                "cache" => {
                    let id = m.id.clone().unwrap_or_else(|| m.target.clone());
                    let dir = self.engine.root.join("build-cache-mounts").join(&sha(&[&id])[..16]);
                    if !dir.exists() {
                        std::fs::create_dir_all(&dir)?;
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(m.mode.unwrap_or(0o755)));
                        if let (Some(uid), gid) = (m.uid, m.gid) {
                            if let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) {
                                // SAFETY: valid string.
                                unsafe { libc::chown(c.as_ptr(), uid, gid.unwrap_or(uid)) };
                            }
                        }
                    }
                    resolved.push(cspec::ResolvedMount { source: dir.to_string_lossy().to_string(), target: m.target.clone(), kind: cspec::MountKind::Bind, read_only: false });
                }
                "bind" => {
                    let src = match &m.from {
                        Some(f) => {
                            let (root, _) = self.source_root(f).await?;
                            secure_join(&root, m.source.as_deref().unwrap_or("/"))
                        }
                        None => secure_join(&self.context, m.source.as_deref().unwrap_or("/")),
                    };
                    // A writable bind gets a scratch copy (changes are
                    // discarded, as with BuildKit).
                    let source = if m.read_only {
                        src
                    } else {
                        let copy = work.join(format!("bind-{i}"));
                        super::engine::copy_tree(&src, &copy)?;
                        copy
                    };
                    resolved.push(cspec::ResolvedMount { source: source.to_string_lossy().to_string(), target: m.target.clone(), kind: cspec::MountKind::Bind, read_only: m.read_only });
                }
                "tmpfs" => resolved.push(cspec::ResolvedMount { source: String::new(), target: m.target.clone(), kind: cspec::MountKind::Tmpfs, read_only: false }),
                other => self.say(format!("[Warning] RUN --mount=type={other} is not supported; skipped\n")),
            }
        }
        if !plan.files.is_empty() {
            let dir = work.join("pipes");
            std::fs::create_dir_all(&dir)?;
            for (target, content) in &plan.files {
                let name = Path::new(target).file_name().unwrap_or_default();
                std::fs::write(dir.join(name), content)?;
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o755))?;
            }
            resolved.push(cspec::ResolvedMount { source: dir.to_string_lossy().to_string(), target: "/dev/pipes".into(), kind: cspec::MountKind::Bind, read_only: true });
        }
        let mut env: Vec<String> = st.image["config"]["Env"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect();
        for (k, v) in &st.args {
            if !env.iter().any(|e| e.split('=').next() == Some(k.as_str())) {
                env.push(format!("{k}={v}"));
            }
        }
        let spec = ContainerSpec {
            name: "build".into(),
            entrypoint: Some(plan.argv.clone()),
            cmd: Some(Vec::new()),
            env,
            working_dir: Some(wd),
            ..Default::default()
        };
        let image = cspec::ImageConfig::default();
        let id = format!("build-{}", random_hex(6));
        let params = cspec::BundleParams {
            spec: &spec,
            image: &image,
            user,
            hostname: "buildkitsandbox",
            netns: None,
            ipcns: None,
            own_uts: true,
            pid: cspec::PidNamespace::New,
            etc_files: ["hosts", "resolv.conf", "hostname"].iter().map(|f| (format!("/etc/{f}"), work.join(f).to_string_lossy().to_string())).collect(),
            shm: None,
            mounts: resolved,
            cgroups_path: format!("/litek8s/build/{id}"),
        };
        let config = cspec::config(&params)?;
        std::fs::write(work.join("config.json"), serde_json::to_vec(&config)?)?;
        self.say(format!(" ---> Running in {}\n", &id[6..]));
        let crun_root = self.engine.crun_root.with_file_name("build-crun");
        std::fs::create_dir_all(&crun_root)?;
        let mut child = tokio::process::Command::new(&self.engine.crun)
            .arg("--root")
            .arg(&crun_root)
            .args(["run", "--bundle"])
            .arg(work)
            .arg(&id)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to run crun")?;
        let out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        let tx = self.tx.clone();
        let quiet = self.opts.quiet;
        let pump = |r: tokio::process::ChildStdout| {
            let tx = tx.clone();
            async move {
                use tokio::io::AsyncReadExt;
                let mut r = r;
                let mut buf = vec![0u8; 16384];
                while let Ok(n) = r.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    if !quiet {
                        let _ = tx.send(json!({"stream": String::from_utf8_lossy(&buf[..n])}));
                    }
                }
            }
        };
        let tx2 = tx.clone();
        let pump_err = async move {
            use tokio::io::AsyncReadExt;
            let mut r = err;
            let mut buf = vec![0u8; 16384];
            while let Ok(n) = r.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                if !quiet {
                    let _ = tx2.send(json!({"stream": String::from_utf8_lossy(&buf[..n])}));
                }
            }
        };
        let (_, _, status) = tokio::join!(pump(out), pump_err, child.wait());
        let _ = tokio::process::Command::new(&self.engine.crun).arg("--root").arg(&crun_root).args(["delete", "-f", &id]).output().await;
        let _ = std::fs::remove_dir(Path::new("/sys/fs/cgroup/litek8s/build").join(&id));
        let code = status?.code().unwrap_or(-1);
        if code != 0 {
            bail!("The command '{}' returned a non-zero code: {code}", plan.argv.join(" "));
        }
        Ok(())
    }

    async fn copy_step(&mut self, st: &mut State, ins: &Instruction) -> Result<()> {
        let add = ins.keyword == "ADD";
        let mut words: Vec<String> = match dockerfile::parse_json_array(&ins.args) {
            Some(v) => v,
            None => shell_words(&ins.args).into_iter().map(|(w, _)| w).collect(),
        };
        let mut heredoc_files = Vec::new();
        for (name, content) in &ins.heredocs {
            if let Some(pos) = words.iter().position(|w| w.trim_start_matches("<<").trim_start_matches('-').trim_matches(['"', '\'']) == name) {
                words.remove(pos);
                heredoc_files.push(content.clone());
            }
        }
        let dest_raw = words.pop().context("COPY needs a destination")?;
        let dest = self.expand_with(st, &dest_raw);
        let sources: Vec<String> = words.iter().map(|w| self.expand_with(st, w)).collect();
        if sources.is_empty() && heredoc_files.is_empty() {
            bail!("COPY needs at least one source");
        }
        let from = ins.flag("from").map(|f| self.expand_with(st, f));
        let chown = ins.flag("chown").map(|c| self.expand_with(st, c)).filter(|c| !c.is_empty());
        let chmod = ins.flag("chmod").and_then(|m| u32::from_str_radix(m, 8).ok());
        let (src_root, from_key) = match &from {
            Some(f) => {
                let (root, key) = self.source_root(f).await?;
                (root, key)
            }
            None => (self.context.clone(), String::new()),
        };
        // Expand wildcards; URLs stay as they are (ADD).
        let mut matched: Vec<Source> = Vec::new();
        for s in &sources {
            if add && (s.starts_with("http://") || s.starts_with("https://")) {
                matched.push(Source::Url(s.clone()));
                continue;
            }
            if add && s.starts_with("git@") {
                bail!("ADD from git is not supported");
            }
            let found = glob_paths(&src_root, s, from.is_none())?;
            if found.is_empty() {
                bail!("COPY failed: file not found in build context or excluded by .dockerignore: stat {s}: file does not exist");
            }
            matched.extend(found.into_iter().map(Source::Path));
        }
        for c in &heredoc_files {
            matched.push(Source::Inline(c.clone()));
        }
        // The key: what is copied, how and where.
        let content = match &from {
            Some(_) => format!("{from_key}:{}", sources.join(",")),
            None => {
                let rel: Vec<String> = matched
                    .iter()
                    .filter_map(|m| match m {
                        Source::Path(p) => Some(p.strip_prefix(&src_root).unwrap_or(p).to_string_lossy().to_string()),
                        _ => None,
                    })
                    .collect();
                hash_paths(&src_root, &rel)?
            }
        };
        let inline: String = heredoc_files.join("\u{0}");
        st.key = sha(&[&st.key, &ins.keyword, &format!("{:?}", ins.flags), &dest, &sources.join(","), &content, &inline, &st.workdir()]);
        let workdir = st.workdir();
        let dest_abs = if dest.starts_with('/') { dest.clone() } else { format!("{}/{dest}", workdir.trim_end_matches('/')) };
        let dir_dest = dest.ends_with('/') || matched.len() > 1 || dest == "." || dest.ends_with("/.");
        let created_by = format!("{} {}", ins.keyword, ins.args.lines().next().unwrap_or_default());
        let plan = CopyPlan { sources: matched, dest: dest_abs, dir_dest, chown, chmod, preserve_owner: from.is_some(), extract: add, workdir };
        self.layer_step(st, &created_by, StepKind::Copy(plan)).await
    }

    async fn exec_copy(&mut self, st: &State, rootfs: &Path, plan: &CopyPlan) -> Result<()> {
        let _ = st;
        let etc = |name: &str| std::fs::read_to_string(secure_join(rootfs, &format!("/etc/{name}"))).unwrap_or_default();
        let owner: Option<(u32, u32)> = match &plan.chown {
            Some(spec) => {
                let (u, g) = spec.split_once(':').map(|(u, g)| (u.to_string(), Some(g.to_string()))).unwrap_or((spec.clone(), None));
                let passwd = etc("passwd");
                let group = etc("group");
                let uid = u.parse::<u32>().ok().or_else(|| passwd.lines().find_map(|l| {
                    let f: Vec<&str> = l.split(':').collect();
                    (f.len() > 3 && f[0] == u).then(|| f[2].parse().ok()).flatten()
                })).with_context(|| format!("unable to find user {u}: no matching entries in passwd file"))?;
                let gid = match g {
                    Some(g) => g.parse::<u32>().ok().or_else(|| group.lines().find_map(|l| {
                        let f: Vec<&str> = l.split(':').collect();
                        (f.len() > 2 && f[0] == g).then(|| f[2].parse().ok()).flatten()
                    })).with_context(|| format!("unable to find group {g}: no matching entries in group file"))?,
                    None => u.parse::<u32>().ok().map(|_| uid).or_else(|| passwd.lines().find_map(|l| {
                        let f: Vec<&str> = l.split(':').collect();
                        (f.len() > 3 && f[0] == u).then(|| f[3].parse().ok()).flatten()
                    })).unwrap_or(uid),
                };
                Some((uid, gid))
            }
            None if plan.preserve_owner => None,
            None => Some((0, 0)),
        };
        let dest = secure_join(rootfs, &plan.dest);
        let dest_is_dir = plan.dir_dest || dest.is_dir();
        // Parent directories, owned like the copied files (Docker: root).
        let mkdirs = |p: &Path| -> Result<()> {
            if !p.exists() {
                std::fs::create_dir_all(p)?;
            }
            Ok(())
        };
        for src in &plan.sources {
            match src {
                Source::Url(url) => {
                    let name = url.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("download").split('?').next().unwrap_or("download").to_string();
                    let target = if dest_is_dir { dest.join(&name) } else { dest.clone() };
                    if let Some(p) = target.parent() {
                        mkdirs(p)?;
                    }
                    let bytes = download(url).await?;
                    std::fs::write(&target, bytes)?;
                    set_owner_mode(&target, owner, Some(plan.chmod.unwrap_or(0o600)))?;
                }
                Source::Inline(content) => {
                    let target = dest.clone();
                    if let Some(p) = target.parent() {
                        mkdirs(p)?;
                    }
                    std::fs::write(&target, content)?;
                    set_owner_mode(&target, owner, Some(plan.chmod.unwrap_or(0o644)))?;
                }
                Source::Path(path) => {
                    let meta = std::fs::symlink_metadata(path)?;
                    if meta.is_dir() {
                        mkdirs(&dest)?;
                        copy_contents(path, &dest, owner, plan.chmod)?;
                    } else if plan.extract && is_archive(path) {
                        mkdirs(&dest)?;
                        extract_archive_into(path, &dest)?;
                    } else {
                        let target = if dest_is_dir { dest.join(path.file_name().unwrap_or_default()) } else { dest.clone() };
                        if let Some(p) = target.parent() {
                            mkdirs(p)?;
                        }
                        copy_one(path, &target, owner, plan.chmod)?;
                    }
                }
            }
        }
        let _ = &plan.workdir;
        Ok(())
    }
}

/// `ARG a=1 b` -> [(a, Some(1)), (b, None)]
fn arg_decls(args: &str) -> Vec<(String, Option<String>)> {
    shell_words(args)
        .into_iter()
        .map(|(w, _)| match w.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (w, None),
        })
        .collect()
}

fn short(id: &str) -> String {
    id.trim_start_matches("sha256:").chars().take(12).collect()
}

fn parse_healthcheck(ins: &Instruction, shell: &[String]) -> Result<Value> {
    let dur = |s: &str| -> Option<i64> {
        let s = s.trim();
        let (num, unit) = s.split_at(s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len()));
        let n: f64 = num.parse().ok()?;
        let mult = match unit {
            "ns" => 1.0,
            "us" | "µs" => 1e3,
            "ms" => 1e6,
            "s" | "" => 1e9,
            "m" => 60e9,
            "h" => 3600e9,
            _ => return None,
        };
        Some((n * mult) as i64)
    };
    let rest = ins.args.trim();
    if rest.eq_ignore_ascii_case("NONE") {
        return Ok(json!({"Test": ["NONE"]}));
    }
    let cmd = rest.strip_prefix("CMD").or_else(|| rest.strip_prefix("cmd")).context("HEALTHCHECK needs CMD or NONE")?.trim();
    let test = match dockerfile::parse_json_array(cmd) {
        Some(v) => std::iter::once("CMD".to_string()).chain(v).collect::<Vec<_>>(),
        None => {
            let _ = shell;
            vec!["CMD-SHELL".to_string(), cmd.to_string()]
        }
    };
    let mut hc = json!({"Test": test});
    for (k, v) in &ins.flags {
        match k.as_str() {
            "interval" => hc["Interval"] = json!(dur(v)),
            "timeout" => hc["Timeout"] = json!(dur(v)),
            "start-period" => hc["StartPeriod"] = json!(dur(v)),
            "start-interval" => hc["StartInterval"] = json!(dur(v)),
            "retries" => hc["Retries"] = json!(v.parse::<i64>().unwrap_or(3)),
            _ => {}
        }
    }
    Ok(hc)
}

enum StepKind {
    Mkdir(String),
    Run(RunPlan),
    Copy(CopyPlan),
}

struct RunPlan {
    argv: Vec<String>,
    mounts: Vec<dockerfile::RunMount>,
    /// (target, content) of heredoc scripts.
    files: Vec<(String, String)>,
}

enum Source {
    Path(PathBuf),
    Url(String),
    Inline(String),
}

struct CopyPlan {
    sources: Vec<Source>,
    dest: String,
    dir_dest: bool,
    chown: Option<String>,
    chmod: Option<u32>,
    preserve_owner: bool,
    extract: bool,
    workdir: String,
}

/// Paths under `root` matching `pattern` (`*`, `?`, `[...]` per path
/// component). Context paths may not leave the context.
fn glob_paths(root: &Path, pattern: &str, is_context: bool) -> Result<Vec<PathBuf>> {
    let pattern = pattern.trim_start_matches("./");
    let comps: Vec<&str> = pattern.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    if comps.iter().any(|c| *c == "..") && is_context {
        bail!("forbidden path outside the build context: {pattern}");
    }
    let mut current = vec![root.to_path_buf()];
    for c in &comps {
        let mut next = Vec::new();
        for dir in &current {
            if c.contains(['*', '?', '[']) {
                let mut names: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
                names.sort();
                for n in names {
                    if wildcard(c, &n) {
                        next.push(dir.join(n));
                    }
                }
            } else {
                let p = if is_context { dir.join(c) } else { secure_join(root, &dir.strip_prefix(root).unwrap_or(dir).join(c).to_string_lossy()) };
                if std::fs::symlink_metadata(&p).is_ok() {
                    next.push(p);
                }
            }
        }
        current = next;
    }
    if comps.is_empty() {
        return Ok(vec![root.to_path_buf()]);
    }
    Ok(current)
}

fn wildcard(pattern: &str, name: &str) -> bool {
    fn m(p: &[char], n: &[char]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some('*'), _) => m(&p[1..], n) || (!n.is_empty() && m(p, &n[1..])),
            (Some('?'), Some(_)) => m(&p[1..], &n[1..]),
            (Some('['), Some(c)) => {
                let Some(end) = p.iter().position(|x| *x == ']') else { return false };
                let set = &p[1..end];
                let (neg, set) = if set.first() == Some(&'!') || set.first() == Some(&'^') { (true, &set[1..]) } else { (false, set) };
                let mut hit = false;
                let mut i = 0;
                while i < set.len() {
                    if i + 2 < set.len() && set[i + 1] == '-' {
                        hit |= set[i] <= *c && *c <= set[i + 2];
                        i += 3;
                    } else {
                        hit |= set[i] == *c;
                        i += 1;
                    }
                }
                hit != neg && m(&p[end + 1..], &n[1..])
            }
            (Some(a), Some(b)) => a == b && m(&p[1..], &n[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    m(&p, &n)
}

/// A hash of files (paths relative to `root`): names, modes, link targets
/// and contents, recursively.
fn hash_paths(root: &Path, rels: &[String]) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    let mut h = Sha256::new();
    let mut stack: Vec<PathBuf> = rels.iter().map(|r| secure_join(root, r)).collect();
    stack.sort();
    stack.reverse();
    while let Some(p) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
        h.update(p.strip_prefix(root).unwrap_or(&p).to_string_lossy().as_bytes());
        h.update(meta.mode().to_le_bytes());
        if meta.file_type().is_symlink() {
            h.update(std::fs::read_link(&p)?.to_string_lossy().as_bytes());
        } else if meta.is_dir() {
            let mut children: Vec<PathBuf> = std::fs::read_dir(&p)?.flatten().map(|e| e.path()).collect();
            children.sort();
            children.reverse();
            stack.extend(children);
        } else {
            let mut f = std::fs::File::open(&p)?;
            std::io::copy(&mut f, &mut h)?;
        }
    }
    Ok(hex::encode(h.finalize()))
}

fn set_owner_mode(p: &Path, owner: Option<(u32, u32)>, mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some((u, g)) = owner {
        let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes())?;
        // SAFETY: valid string.
        unsafe { libc::lchown(c.as_ptr(), u, g) };
    }
    if let Some(m) = mode {
        if !std::fs::symlink_metadata(p)?.file_type().is_symlink() {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m))?;
        }
    }
    Ok(())
}

fn copy_one(src: &Path, dst: &Path, owner: Option<(u32, u32)>, chmod: Option<u32>) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = std::fs::symlink_metadata(src)?;
    if let Ok(existing) = std::fs::symlink_metadata(dst) {
        if existing.is_dir() && !meta.is_dir() {
            std::fs::remove_dir_all(dst)?;
        } else if !existing.is_dir() {
            std::fs::remove_file(dst)?;
        }
    }
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
    } else if meta.is_file() {
        std::fs::copy(src, dst)?;
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(chmod.unwrap_or(meta.mode() & 0o7777)))?;
        let mtime = libc::timespec { tv_sec: meta.mtime(), tv_nsec: meta.mtime_nsec() };
        let c = std::ffi::CString::new(dst.as_os_str().as_encoded_bytes())?;
        // SAFETY: valid string and timespecs.
        unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), [mtime, mtime].as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
    } else {
        // Devices, fifos: skipped.
        return Ok(());
    }
    let owner = owner.unwrap_or((meta.uid(), meta.gid()));
    set_owner_mode(dst, Some(owner), None)
}

fn copy_contents(src: &Path, dst: &Path, owner: Option<(u32, u32)>, chmod: Option<u32>) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let s = e.path();
        let d = dst.join(e.file_name());
        let meta = std::fs::symlink_metadata(&s)?;
        if meta.is_dir() {
            if std::fs::symlink_metadata(&d).is_ok_and(|m| !m.is_dir()) {
                std::fs::remove_file(&d)?;
            }
            if !d.exists() {
                std::fs::create_dir(&d)?;
            }
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(chmod.unwrap_or(meta.mode() & 0o7777)))?;
            set_owner_mode(&d, Some(owner.unwrap_or((meta.uid(), meta.gid()))), None)?;
            copy_contents(&s, &d, owner, chmod)?;
        } else {
            copy_one(&s, &d, owner, chmod)?;
        }
    }
    Ok(())
}

fn is_archive(p: &Path) -> bool {
    use std::io::Read;
    let mut buf = [0u8; 512];
    let Ok(mut f) = std::fs::File::open(p) else { return false };
    let n = f.read(&mut buf).unwrap_or(0);
    (n >= 2 && buf[0] == 0x1f && buf[1] == 0x8b) || (n >= 262 && &buf[257..262] == b"ustar")
}

fn extract_archive_into(p: &Path, dest: &Path) -> Result<()> {
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
    let mut magic = [0u8; 2];
    let n = f.read(&mut magic)?;
    let raw = std::io::Cursor::new(magic[..n].to_vec()).chain(f);
    let reader: Box<dyn Read> = if magic == [0x1f, 0x8b] { Box::new(flate2::read::MultiGzDecoder::new(raw)) } else { Box::new(raw) };
    let mut a = tar::Archive::new(reader);
    a.set_preserve_permissions(true);
    a.set_preserve_ownerships(true);
    a.set_overwrite(true);
    a.unpack(dest)?;
    Ok(())
}

async fn download(url: &str) -> Result<Vec<u8>> {
    let mut url = url.to_string();
    for _ in 0..8 {
        let resp = cimage::send(hyper::Method::GET, &url, &[]).await?;
        if resp.status().is_redirection() {
            let loc = resp.headers().get(hyper::header::LOCATION).and_then(|l| l.to_str().ok()).context("redirect without location")?.to_string();
            url = if loc.starts_with("http") {
                loc
            } else {
                let base: Vec<&str> = url.splitn(4, '/').collect();
                format!("{}//{}{loc}", base[0], base.get(2).copied().unwrap_or_default())
            };
            continue;
        }
        if !resp.status().is_success() {
            bail!("ADD {url}: {}", resp.status());
        }
        return Ok(resp.into_body().collect().await?.to_bytes().to_vec());
    }
    bail!("ADD {url}: too many redirects")
}

fn bind_ro(src: &Path, dst: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let (s, d) = (std::ffi::CString::new(src.as_os_str().as_encoded_bytes())?, std::ffi::CString::new(dst.as_os_str().as_encoded_bytes())?);
        // SAFETY: valid strings.
        let rc = unsafe { libc::mount(s.as_ptr(), d.as_ptr(), std::ptr::null(), libc::MS_BIND, std::ptr::null()) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("bind mount");
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (src, dst);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards_and_args() {
        assert!(wildcard("*.txt", "a.txt") && !wildcard("*.txt", "a.md"));
        assert!(wildcard("file[0-9]", "file3") && !wildcard("file[!0-9]", "file3"));
        assert!(wildcard("?ack*", "package.json") && !wildcard("?zz*", "package.json"));
        assert_eq!(arg_decls("A=1 B"), vec![("A".to_string(), Some("1".to_string())), ("B".to_string(), None)]);
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "x").unwrap();
        std::fs::write(root.path().join("b.txt"), "y").unwrap();
        std::fs::create_dir(root.path().join("d")).unwrap();
        assert_eq!(glob_paths(root.path(), "*.txt", true).unwrap().len(), 2);
        assert_eq!(glob_paths(root.path(), ".", true).unwrap(), vec![root.path().to_path_buf()]);
        assert!(glob_paths(root.path(), "../x", true).is_err());
        let h1 = hash_paths(root.path(), &["a.txt".into()]).unwrap();
        std::fs::write(root.path().join("a.txt"), "z").unwrap();
        assert_ne!(h1, hash_paths(root.path(), &["a.txt".into()]).unwrap());
    }

    #[test]
    fn healthcheck_flags() {
        let ins = dockerfile::parse("FROM x\nHEALTHCHECK --interval=5s --retries=2 CMD curl -f http://localhost/").unwrap().stages[0].instructions[0].clone();
        let hc = parse_healthcheck(&ins, &[]).unwrap();
        assert_eq!(hc, json!({"Test": ["CMD-SHELL", "curl -f http://localhost/"], "Interval": 5_000_000_000i64, "Retries": 2}));
    }
}
