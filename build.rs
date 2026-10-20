//! Compile the vendored Kubernetes .proto files (proto/) into a descriptor set
//! the API server embeds to decode protobuf request bodies, which kubectl
//! sends for built-in types. protox is pure Rust, so no protoc is needed.
//!
//! Also sets `LITEK8S_VERSION`: the plain version for a release build
//! (`LITEK8S_RELEASE=1`, what the release workflow sets, or a build without
//! git, as from crates.io), else `<version> DEV <commit>` so a build from a
//! checkout never passes for the release.

use std::path::{Path, PathBuf};

fn collect(dir: &Path, root: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read proto dir") {
        let path = entry.expect("proto dir entry").path();
        if path.is_dir() {
            collect(&path, root, out);
        } else if path.extension().is_some_and(|e| e == "proto") {
            out.push(path.strip_prefix(root).unwrap().to_path_buf());
        }
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

fn version() {
    println!("cargo:rerun-if-env-changed=LITEK8S_RELEASE");
    println!("cargo:rerun-if-env-changed=LITEK8S_GIT_COMMIT");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let release = std::env::var("LITEK8S_RELEASE").is_ok_and(|v| v == "1");
    // Builds without the repository (in a container that only has the
    // sources) pass the commit in LITEK8S_GIT_COMMIT.
    let mut commit = std::env::var("LITEK8S_GIT_COMMIT").ok().filter(|c| !c.is_empty());
    if commit.is_none() && Path::new(".git").exists() {
        commit = git(&["rev-parse", "--short=12", "HEAD"]);
        // Rebuild when HEAD moves: HEAD itself, the branch it names, packed refs.
        for path in [git(&["rev-parse", "--git-path", "HEAD"]), git(&["rev-parse", "--symbolic-full-name", "HEAD"]).and_then(|r| git(&["rev-parse", "--git-path", &r])), git(&["rev-parse", "--git-path", "packed-refs"])].into_iter().flatten() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let full = match commit {
        Some(c) if !release => format!("{version} DEV {c}"),
        _ => version,
    };
    println!("cargo:rustc-env=LITEK8S_VERSION={full}");
}

fn main() {
    version();
    let root = Path::new("proto");
    println!("cargo:rerun-if-changed=proto");
    let mut files = Vec::new();
    collect(root, root, &mut files);
    files.sort();
    let mut compiler = protox::Compiler::new([root]).expect("protox include path");
    compiler.include_imports(true);
    compiler
        .open_files(&files)
        .unwrap_or_else(|e| panic!("failed to compile Kubernetes protos: {e}"));
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("k8s-descriptors.bin");
    std::fs::write(&out, compiler.encode_file_descriptor_set()).expect("write descriptor set");
}
