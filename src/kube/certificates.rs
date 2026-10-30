//! `certificates.k8s.io` ClusterTrustBundles and PodCertificateRequests
//! (served in v1 and v1beta1) and the pod's `podCertificate` /
//! `clusterTrustBundle` projected volume sources: validation as upstream
//! 1.37 does it (`pkg/apis/certificates/validation`, `pkg/apis/core/
//! validation`), plus the binding check NodeRestriction makes. See
//! docs/specs/09-pod-certificates.md.

use std::collections::BTreeMap;

use base64::Engine;
use serde_json::{Value, json};

use super::cel::lib::{is_dns1123_label, is_dns1123_subdomain};
use super::registry::ResourceType;
use super::x509;

pub const GROUP: &str = "certificates.k8s.io";
pub const MIN_MAX_EXPIRATION: i64 = 3600;
pub const MAX_MAX_EXPIRATION: i64 = 91 * 24 * 3600;
pub const KUBERNETES_MAX_MAX_EXPIRATION: i64 = 24 * 3600;
pub const DEFAULT_MAX_EXPIRATION: i64 = 86400;
const MAX_TRUST_BUNDLE: usize = 1024 * 1024;
const MAX_KEY_FIELD: usize = 10 * 1024;
const MAX_CHAIN: usize = 100 * 1024;
const MAX_ANNOTATIONS: usize = 256 * 1024;
pub const KEY_TYPES: &[&str] = &["RSA3072", "RSA4096", "ECDSAP256", "ECDSAP384", "ECDSAP521", "ED25519"];

fn s<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or_default()
}

fn required(path: &str) -> String {
    format!("{path}: Required value")
}

fn invalid(path: &str, value: &str, msg: &str) -> String {
    format!("{path}: Invalid value: \"{value}\": {msg}")
}

fn b64(v: &Value, ptr: &str) -> Result<Vec<u8>, String> {
    match v.pointer(ptr).and_then(Value::as_str) {
        None | Some("") => Ok(Vec::new()),
        Some(text) => base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(|_| format!("{}: Invalid value: must be base64", ptr.trim_start_matches('/').replace('/', "."))),
    }
}

/// Whether `rt` is one of the types served in two versions here.
pub fn versioned(rt: &ResourceType) -> bool {
    rt.group == GROUP && matches!(rt.plural.as_str(), "clustertrustbundles" | "podcertificaterequests")
}

/// The stored object as the requested version: its apiVersion, and v1
/// PodCertificateRequests without the two fields only v1beta1 has.
pub fn serve_as(rt: &ResourceType, mut obj: Value) -> Value {
    if !versioned(rt) {
        return obj;
    }
    if let Some(m) = obj.as_object_mut() {
        m.insert("apiVersion".into(), json!(rt.api_version()));
    }
    if rt.version == "v1" && rt.plural == "podcertificaterequests" {
        if let Some(spec) = obj.get_mut("spec").and_then(Value::as_object_mut) {
            spec.remove("pkixPublicKey");
            spec.remove("proofOfPossession");
        }
    }
    obj
}

/// Whether `signer` is reserved by the Kubernetes project.
pub fn is_kubernetes_signer(signer: &str) -> bool {
    let host = signer.split('/').next().unwrap_or_default();
    host == "kubernetes.io" || host.ends_with(".kubernetes.io")
}

/// `ValidateSignerName`: `example.com/path`.
pub fn signer_name_errors(path: &str, signer: &str) -> Vec<String> {
    if signer.is_empty() {
        return vec![required(path)];
    }
    let segments: Vec<&str> = signer.split('/').collect();
    if segments.len() != 2 {
        return vec![invalid(path, signer, "must be a fully qualified domain and path of the form 'example.com/signer-name'")];
    }
    let mut errs = Vec::new();
    if segments[0].len() > 253 {
        errs.push(format!("{path}: Too long: may not be more than 253 bytes"));
    }
    let domain: Vec<&str> = segments[0].split('.').collect();
    if let Some((lbl, e)) = domain.iter().find_map(|l| is_dns1123_label(l).into_iter().next().map(|e| (l, e))) {
        errs.push(invalid(path, segments[0], &format!("validating label \"{lbl}\": {e}")));
    }
    if domain.len() < 2 {
        errs.push(invalid(path, segments[0], "should be a domain with at least two segments separated by dots"));
    }
    if let Some((lbl, e)) = segments[1].split('.').find_map(|l| is_dns1123_subdomain(l).into_iter().next().map(|e| (l, e))) {
        errs.push(invalid(path, segments[1], &format!("validating label \"{lbl}\": {e}")));
    }
    let max = 253 + 253 + 63 + 1 + 1;
    if signer.len() > max {
        errs.push(format!("{path}: Too long: may not be more than {max} bytes"));
    }
    errs
}

/// `ValidateClusterTrustBundleName`.
fn ctb_name_errors(name: &str, signer: &str) -> Vec<String> {
    if signer.is_empty() {
        if name.contains(':') {
            return vec!["ClusterTrustBundle without signer name must not have \":\" in its name".into()];
        }
        return is_dns1123_subdomain(name);
    }
    let prefix = format!("{}:", signer.replace('/', ":"));
    match name.strip_prefix(&prefix) {
        Some(rest) => is_dns1123_subdomain(rest),
        None => vec![format!("ClusterTrustBundle for signerName {signer} must be named with prefix {prefix}")],
    }
}

/// `IsDomainPrefixedKey` over the keys, and the annotations size limit.
fn user_annotation_errors(path: &str, annotations: Option<&Value>) -> Vec<String> {
    let Some(map) = annotations.and_then(Value::as_object) else { return Vec::new() };
    let mut errs = Vec::new();
    let mut size = 0;
    for (k, v) in map {
        size += k.len() + v.as_str().map(str::len).unwrap_or_default();
        let key = k.to_lowercase();
        let ok = match key.split_once('/') {
            Some((prefix, name)) => {
                is_dns1123_subdomain(prefix).is_empty()
                    && !name.is_empty()
                    && name.len() <= 63
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                    && name.starts_with(|c: char| c.is_ascii_alphanumeric())
                    && name.ends_with(|c: char| c.is_ascii_alphanumeric())
            }
            None => false,
        };
        if !ok {
            errs.push(invalid(&format!("{path}[{k}]"), k, "must be a domain-prefixed key (such as \"acme.io/foo\")"));
        }
    }
    if size > MAX_ANNOTATIONS {
        errs.push(format!("{path}: Too long: may not be more than {MAX_ANNOTATIONS} bytes"));
    }
    errs
}

/// `ValidateLocalNonReservedPath`.
fn local_path_errors(path: &str, target: &str) -> Vec<String> {
    let mut errs = Vec::new();
    if target.starts_with('/') {
        errs.push(invalid(path, target, "must be a relative path"));
    }
    if target.split('/').any(|seg| seg == "..") {
        errs.push(invalid(path, target, "must not contain '..'"));
    } else if target.starts_with("..") {
        errs.push(invalid(path, target, "must not start with '..'"));
    }
    errs
}

/// Field errors for a ClusterTrustBundle or PodCertificateRequest create
/// (`old` is `None`) or update (not `/status`).
pub fn validate(kind: &str, obj: &Value, old: Option<&Value>) -> Vec<String> {
    match kind {
        "ClusterTrustBundle" => validate_ctb(obj, old),
        "PodCertificateRequest" => match old {
            None => validate_pcr_create(obj),
            Some(old) if old.get("spec") != obj.get("spec") => {
                vec!["spec: Invalid value: \"object\": field is immutable".into()]
            }
            Some(_) => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn validate_ctb(obj: &Value, old: Option<&Value>) -> Vec<String> {
    let mut errs = Vec::new();
    let name = s(obj, "/metadata/name");
    let signer = s(obj, "/spec/signerName");
    for msg in ctb_name_errors(name, signer) {
        errs.push(invalid("metadata.name", name, &msg));
    }
    if !signer.is_empty() {
        errs.extend(signer_name_errors("spec.signerName", signer));
    }
    let bundle = s(obj, "/spec/trustBundle");
    let unchanged = old.is_some_and(|o| s(o, "/spec/trustBundle") == bundle);
    if !unchanged {
        errs.extend(trust_bundle_errors("spec.trustBundle", bundle));
    }
    if let Some(old) = old {
        if s(old, "/spec/signerName") != signer {
            errs.push(invalid("spec.signerName", signer, "field is immutable"));
        }
    }
    errs
}

fn trust_bundle_errors(path: &str, bundle: &str) -> Vec<String> {
    if bundle.len() > MAX_TRUST_BUNDLE {
        return vec![format!("{path}: Too long: may not be more than {MAX_TRUST_BUNDLE} bytes")];
    }
    let bad = |msg: String| format!("{path}: Invalid value: \"<value omitted>\": {msg}");
    let blocks = match x509::pem_blocks(bundle) {
        Ok(b) => b,
        Err(e) => return vec![bad(e)],
    };
    let mut errs = Vec::new();
    let mut seen: BTreeMap<Vec<u8>, Vec<usize>> = BTreeMap::new();
    for (i, b) in blocks.iter().enumerate() {
        if b.label != "CERTIFICATE" {
            errs.push(bad(format!("entry {i} has bad block type: {}", b.label)));
        } else if b.has_headers {
            errs.push(bad(format!("entry {i} has PEM block headers")));
        } else {
            match x509::parse_certificate(&b.der) {
                Err(_) => errs.push(bad(format!("entry {i} does not parse as X.509"))),
                Ok(c) if !c.is_ca => errs.push(bad(format!("entry {i} does not have the CA bit set"))),
                Ok(_) => seen.entry(b.der.clone()).or_default().push(i),
            }
        }
    }
    if !errs.is_empty() {
        return errs;
    }
    if seen.is_empty() {
        errs.push(bad("at least one trust anchor must be provided".into()));
    }
    for indices in seen.values().filter(|i| i.len() > 1) {
        errs.push(bad(format!("duplicate trust anchor (indices {indices:?})")));
    }
    errs
}

/// Default `spec.maxExpirationSeconds` (86400) on create.
pub fn default_pcr(obj: &mut Value) {
    if obj.pointer("/spec/maxExpirationSeconds").is_none_or(Value::is_null) {
        if let Some(spec) = obj.get_mut("spec").and_then(Value::as_object_mut) {
            spec.insert("maxExpirationSeconds".into(), json!(DEFAULT_MAX_EXPIRATION));
        }
    }
}

/// The public key a PodCertificateRequest asks a certificate for.
pub fn pcr_public_key(obj: &Value) -> Result<x509::PublicKey, String> {
    let stub = b64(obj, "/spec/stubPKCS10Request")?;
    if !stub.is_empty() {
        return x509::verify_csr(&stub);
    }
    x509::parse_spki(&b64(obj, "/spec/pkixPublicKey")?)
}

fn validate_pcr_create(obj: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    let signer = s(obj, "/spec/signerName");
    errs.extend(signer_name_errors("spec.signerName", signer));
    errs.extend(user_annotation_errors("spec.unverifiedUserAnnotations", obj.pointer("/spec/unverifiedUserAnnotations")));
    for (field, subdomain) in [("podName", true), ("serviceAccountName", true), ("nodeName", true)] {
        let v = s(obj, &format!("/spec/{field}"));
        let msgs = if subdomain { is_dns1123_subdomain(v) } else { Vec::new() };
        for m in msgs {
            errs.push(invalid(&format!("spec.{field}"), v, &m));
        }
    }
    for field in ["podUID", "serviceAccountUID", "nodeUID"] {
        let v = s(obj, &format!("/spec/{field}"));
        if v.is_empty() {
            errs.push(invalid(&format!("spec.{field}"), v, "must not be empty"));
        } else if v.len() > 128 {
            errs.push(format!("spec.{field}: Too long: may not be more than 128 bytes"));
        }
    }
    let Some(max) = obj.pointer("/spec/maxExpirationSeconds").and_then(Value::as_i64) else {
        errs.push("spec.maxExpirationSeconds: Required value: must be set".into());
        return errs;
    };
    let ceiling = if is_kubernetes_signer(signer) { KUBERNETES_MAX_MAX_EXPIRATION } else { MAX_MAX_EXPIRATION };
    if !(MIN_MAX_EXPIRATION..=ceiling).contains(&max) {
        errs.push(format!("spec.maxExpirationSeconds: Invalid value: {max}: must be in the range [{MIN_MAX_EXPIRATION}, {ceiling}]"));
    }
    let (stub, pkix, pop) = match (b64(obj, "/spec/stubPKCS10Request"), b64(obj, "/spec/pkixPublicKey"), b64(obj, "/spec/proofOfPossession")) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        (a, b, c) => {
            errs.extend([a.err(), b.err(), c.err()].into_iter().flatten());
            return errs;
        }
    };
    let key_ok = |k: &x509::PublicKey, path: &str| -> Option<String> {
        match k.key_type {
            x509::KeyType::Rsa(bits) if bits != 3072 && bits != 4096 => {
                Some(invalid(path, &format!("{bits}-bit modulus"), "RSA keys must have modulus size 3072 or 4096"))
            }
            _ => None,
        }
    };
    match (stub.is_empty(), pkix.is_empty(), pop.is_empty()) {
        (false, true, true) => {
            if stub.len() > MAX_KEY_FIELD {
                errs.push(format!("spec.stubPKCS10Request: Too long: may not be more than {MAX_KEY_FIELD} bytes"));
                return errs;
            }
            match x509::verify_csr(&stub) {
                Ok(k) => errs.extend(key_ok(&k, "spec.stubPKCS10Request")),
                Err(e) => errs.push(format!("spec.stubPKCS10Request: Invalid value: must be a valid PKCS#10 CSR with a valid signature: {e}")),
            }
        }
        (true, false, false) => {
            if pkix.len() > MAX_KEY_FIELD || pop.len() > MAX_KEY_FIELD {
                errs.push(format!("spec.pkixPublicKey: Too long: may not be more than {MAX_KEY_FIELD} bytes"));
                return errs;
            }
            match x509::parse_spki(&pkix) {
                Ok(k) => {
                    errs.extend(key_ok(&k, "spec.pkixPublicKey"));
                    if let Err(e) = x509::verify_proof_of_possession(&k, s(obj, "/spec/podUID"), &pop) {
                        errs.push(format!("spec.proofOfPossession: Invalid value: {e}"));
                    }
                }
                Err(e) => errs.push(format!("spec.pkixPublicKey: Invalid value: must be a valid PKIX-serialized public key: {e}")),
            }
        }
        _ => errs.push("spec: Invalid value: exactly one of (stubPKCS10Request) or (pkixPublicKey, proofOfPossession) must be set".into()),
    }
    errs
}

/// The PodCertificateRequest must name the pod it is for as it is: same
/// UID, service account and node (what NodeRestriction and the strategy
/// check upstream), so no one gets a certificate for another pod.
pub fn binding_errors(pcr: &Value, pod: Option<&Value>, sa: Option<&Value>, node: Option<&Value>) -> Vec<String> {
    let mut errs = Vec::new();
    let Some(pod) = pod else {
        return vec![invalid("spec.podName", s(pcr, "/spec/podName"), "pod not found")];
    };
    let pod_sa = match s(pod, "/spec/serviceAccountName") {
        "" => "default",
        sa => sa,
    };
    let checks = [
        ("spec.podUID", s(pcr, "/spec/podUID"), s(pod, "/metadata/uid")),
        ("spec.serviceAccountName", s(pcr, "/spec/serviceAccountName"), pod_sa),
        ("spec.nodeName", s(pcr, "/spec/nodeName"), s(pod, "/spec/nodeName")),
        ("spec.serviceAccountUID", s(pcr, "/spec/serviceAccountUID"), sa.map(|v| s(v, "/metadata/uid")).unwrap_or("<not found>")),
        ("spec.nodeUID", s(pcr, "/spec/nodeUID"), node.map(|v| s(v, "/metadata/uid")).unwrap_or("<not found>")),
    ];
    for (path, got, want) in checks {
        if got != want {
            errs.push(invalid(path, got, &format!("does not match the pod (want \"{want}\")")));
        }
    }
    errs
}

fn time(v: &Value, ptr: &str) -> Option<i64> {
    v.pointer(ptr).and_then(Value::as_str).and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.timestamp())
}

fn terminal(obj: &Value) -> Option<String> {
    obj.pointer("/status/conditions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|c| matches!(s(c, "/type"), "Issued" | "Denied" | "Failed") && s(c, "/status") == "True")
        .map(|c| s(c, "/type").to_string())
}

/// The issued condition of `pcr`, if any: `Ok(Some(chain))`; Denied or
/// Failed: `Err(reason: message)`.
pub fn pcr_outcome(pcr: &Value) -> Result<Option<String>, String> {
    match terminal(pcr).as_deref() {
        Some("Issued") => Ok(Some(s(pcr, "/status/certificateChain").to_string())),
        Some(t) => {
            let c = pcr
                .pointer("/status/conditions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|c| s(c, "/type") == t)
                .cloned()
                .unwrap_or_default();
            Err(format!("{t}: {}: {}", s(&c, "/reason"), s(&c, "/message")))
        }
        None => Ok(None),
    }
}

/// `ValidatePodCertificateRequestStatusUpdate`; `now` in Unix seconds.
pub fn validate_pcr_status(new: &Value, old: &Value, now: i64) -> Vec<String> {
    let mut errs = Vec::new();
    let conds: Vec<&Value> = new.pointer("/status/conditions").and_then(Value::as_array).into_iter().flatten().collect();
    let mut known = 0;
    for (i, c) in conds.iter().enumerate() {
        let t = s(c, "/type");
        if matches!(t, "Issued" | "Denied" | "Failed") {
            known += 1;
            if known > 1 {
                errs.push(invalid(&format!("status.conditions[{i}].type"), t, "There may be at most one condition with type \"Issued\", \"Denied\", or \"Failed\""));
            }
            if s(c, "/status") != "True" {
                errs.push(format!("status.conditions[{i}].status: Unsupported value: \"{}\": supported values: \"True\"", s(c, "/status")));
            }
        } else {
            errs.push(format!("status.conditions[{i}].type: Unsupported value: \"{t}\": supported values: \"Issued\", \"Denied\", \"Failed\""));
        }
        let reason = s(c, "/reason");
        let reason_ok = reason.starts_with(|ch: char| ch.is_ascii_alphabetic())
            && reason.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | ',' | ':'))
            && !reason.ends_with([',', ':']);
        if !reason_ok {
            errs.push(invalid(&format!("status.conditions[{i}].reason"), reason, "a condition reason must start with alphabetic character, optionally followed by a string of alphanumeric characters or '_,:', and must end with an alphanumeric character or '_'"));
        }
        if time(c, "/lastTransitionTime").is_none() {
            errs.push(required(&format!("status.conditions[{i}].lastTransitionTime")));
        }
    }
    if !errs.is_empty() {
        return errs;
    }
    let status = |o: &Value| o.get("status").cloned().filter(|v| !v.is_null()).unwrap_or_else(|| json!({}));
    if terminal(old).is_some() {
        if status(new) != status(old) {
            errs.push("status: Invalid value: immutable after PodCertificateRequest is issued, denied, or failed".into());
        }
        return errs;
    }
    match terminal(new).as_deref() {
        Some("Denied" | "Failed") => {
            if status(new) != json!({"conditions": new.pointer("/status/conditions").cloned().unwrap_or_default()}) {
                errs.push("status: Invalid value: non-condition status fields must be empty when denying or failing the PodCertificateRequest".into());
            }
            errs
        }
        Some(_) => issued_errors(new, old, now),
        None => {
            if status(new) != status(old) {
                errs.push("status: Invalid value: status is immutable unless transitioning to \"Issued\", \"Denied\", or \"Failed\"".into());
            }
            errs
        }
    }
}

fn issued_errors(new: &Value, old: &Value, now: i64) -> Vec<String> {
    let chain_path = "status.certificateChain";
    let chain = s(new, "/status/certificateChain");
    let bad = |msg: &str| vec![format!("{chain_path}: Invalid value: \"<value omitted>\": {msg}")];
    if chain.len() > MAX_CHAIN {
        return vec![format!("{chain_path}: Too long: may not be more than {MAX_CHAIN} bytes")];
    }
    let Ok(blocks) = x509::pem_blocks(chain) else { return bad("issued certificate chain must consist entirely of CERTIFICATE PEM blocks") };
    let Some(leaf_block) = blocks.first() else { return bad("issued certificate chain must contain at least one certificate") };
    if blocks.iter().any(|b| b.label != "CERTIFICATE") {
        return bad("issued certificate chain must consist entirely of CERTIFICATE PEM blocks");
    }
    let Ok(leaf) = x509::parse_certificate(&leaf_block.der) else { return bad("leaf certificate does not parse as valid X.509") };
    match pcr_public_key(old) {
        Ok(k) if k.spki == leaf.public_key.spki => {}
        Ok(_) => return bad("leaf certificate was not issued to the requested public key"),
        Err(e) => return vec![format!("spec: Invalid value: {e}")],
    }
    let mut errs = Vec::new();
    let (nb, na, br) = (time(new, "/status/notBefore"), time(new, "/status/notAfter"), time(new, "/status/beginRefreshAt"));
    if nb.is_none() {
        errs.push("status.notBefore: Required value: must be present and consistent with the issued certificate".into());
    }
    if na.is_none() {
        errs.push("status.notAfter: Required value: must be present and consistent with the issued certificate".into());
    }
    if br.is_none() {
        errs.push("status.beginRefreshAt: Required value: must be present and in the range [notbefore+10min, notafter-10min]".into());
    }
    let (Some(nb), Some(na), Some(br)) = (nb, na, br) else { return errs };
    if nb != leaf.not_before {
        return vec!["status.notBefore: Invalid value: must be set to the NotBefore time encoded in the leaf certificate".into()];
    }
    if (nb - now).abs() > 300 {
        return vec!["status.notBefore: Invalid value: must be set to within 5 minutes of kube-apiserver's current time".into()];
    }
    if na != leaf.not_after {
        return vec!["status.notAfter: Invalid value: must be set to the NotAfter time encoded in the leaf certificate".into()];
    }
    let lifetime = leaf.not_after - leaf.not_before;
    let max = new.pointer("/spec/maxExpirationSeconds").and_then(Value::as_i64).unwrap_or(DEFAULT_MAX_EXPIRATION);
    if lifetime < 3600 {
        return bad("leaf certificate lifetime must be >= 1 hour");
    }
    if lifetime > max {
        return bad(&format!("leaf certificate lifetime must be <= spec.maxExpirationSeconds ({max})"));
    }
    if br < nb + 600 {
        return vec!["status.beginRefreshAt: Invalid value: must be at least 10 minutes after status.notBefore".into()];
    }
    if br > na - 600 {
        return vec!["status.beginRefreshAt: Invalid value: must be at least 10 minutes before status.notAfter".into()];
    }
    if blocks[1..].iter().any(|b| x509::parse_certificate(&b.der).is_err()) {
        return bad("intermediate certificate does not parse as valid X.509");
    }
    errs
}

/// Errors in the pod's `podCertificate` and `clusterTrustBundle` projected
/// volume sources (`validateProjectionSources`), and duplicate paths among
/// all of a projected volume's sources.
pub fn projection_errors(pod: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    for (vi, vol) in pod.pointer("/spec/volumes").and_then(Value::as_array).into_iter().flatten().enumerate() {
        let Some(sources) = vol.pointer("/projected/sources").and_then(Value::as_array) else { continue };
        let vpath = format!("spec.volumes[{vi}].projected");
        let mut paths: Vec<String> = Vec::new();
        let mut add_path = |errs: &mut Vec<String>, p: &str| {
            if paths.iter().any(|x| x == p) {
                errs.push(invalid(&vpath, p, "conflicting duplicate paths"));
            } else {
                paths.push(p.to_string());
            }
        };
        for (i, src) in sources.iter().enumerate() {
            let spath = format!("{vpath}.sources[{i}]");
            let kinds = ["secret", "downwardAPI", "configMap", "serviceAccountToken", "clusterTrustBundle", "podCertificate"];
            if kinds.iter().filter(|k| src.get(**k).is_some_and(|v| !v.is_null())).count() > 1 {
                errs.push(format!("{spath}: Forbidden: may not specify more than 1 volume type per source"));
            }
            if let Some(ctb) = src.get("clusterTrustBundle").filter(|v| !v.is_null()) {
                let p = format!("{spath}.clusterTrustBundle");
                let name = ctb.get("name").and_then(Value::as_str);
                let signer = ctb.get("signerName").and_then(Value::as_str);
                match (name, signer) {
                    (Some(_), Some(_)) => errs.push(invalid(&p, "", "only one of name and signerName may be used")),
                    (Some(name), None) => {
                        if name.is_empty() {
                            errs.push(format!("{p}.name: Required value: must be a valid object name"));
                        }
                        let signer = name.rfind(':').map(|at| name[..at].replace(':', "/")).unwrap_or_default();
                        for m in ctb_name_errors(name, &signer) {
                            errs.push(invalid(&format!("{p}.name"), name, &format!("not a valid clustertrustbundlename: {m}")));
                        }
                        if ctb.get("labelSelector").is_some_and(|v| !v.is_null()) {
                            errs.push(invalid(&format!("{p}.labelSelector"), "", "labelSelector must be unset if name is specified"));
                        }
                    }
                    (None, Some(signer)) => errs.extend(signer_name_errors(&format!("{p}.signerName"), signer)),
                    (None, None) => errs.push(format!("{p}: Required value: either name or signerName must be specified")),
                }
                let path = s(ctb, "/path");
                if path.is_empty() {
                    errs.push(required(&format!("{p}.path")));
                }
                errs.extend(local_path_errors(&format!("{p}.path"), path));
                add_path(&mut errs, path);
            }
            if let Some(pc) = src.get("podCertificate").filter(|v| !v.is_null()) {
                let p = format!("{spath}.podCertificate");
                let signer = s(pc, "/signerName");
                errs.extend(signer_name_errors(&format!("{p}.signerName"), signer));
                errs.extend(user_annotation_errors(&format!("{p}.userAnnotations"), pc.get("userAnnotations")));
                let kt = s(pc, "/keyType");
                if !KEY_TYPES.contains(&kt) {
                    let supported = KEY_TYPES.iter().map(|k| format!("\"{k}\"")).collect::<Vec<_>>().join(", ");
                    errs.push(format!("{p}.keyType: Unsupported value: \"{kt}\": supported values: {supported}"));
                }
                if let Some(max) = pc.get("maxExpirationSeconds").and_then(Value::as_i64) {
                    if max < MIN_MAX_EXPIRATION {
                        errs.push(format!("{p}.maxExpirationSeconds: Invalid value: {max}: if provided, maxExpirationSeconds must be >= 3600"));
                    }
                    let ceiling = if is_kubernetes_signer(signer) { KUBERNETES_MAX_MAX_EXPIRATION } else { MAX_MAX_EXPIRATION };
                    if max > ceiling {
                        errs.push(format!("{p}.maxExpirationSeconds: Invalid value: {max}: if provided, maxExpirationSeconds must be <= {ceiling}"));
                    }
                }
                let mut n = 0;
                for field in ["credentialBundlePath", "keyPath", "certificateChainPath"] {
                    let path = s(pc, &format!("/{field}"));
                    if path.is_empty() {
                        continue;
                    }
                    n += 1;
                    errs.extend(local_path_errors(&format!("{p}.{field}"), path));
                    add_path(&mut errs, path);
                }
                if n == 0 {
                    errs.push(format!("{p}: Required value: specify at least one of credentialBundlePath, keyPath, and certificateChainPath"));
                }
            }
            for (kind, field) in [("secret", "items"), ("configMap", "items"), ("downwardAPI", "items")] {
                for item in src.pointer(&format!("/{kind}/{field}")).and_then(Value::as_array).into_iter().flatten() {
                    add_path(&mut errs, s(item, "/path"));
                }
            }
            if let Some(tok) = src.get("serviceAccountToken").filter(|v| !v.is_null()) {
                add_path(&mut errs, s(tok, "/path"));
            }
        }
    }
    errs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ca_pem(alg: &'static rcgen::SignatureAlgorithm) -> (String, rcgen::Certificate, rcgen::KeyPair) {
        let key = rcgen::KeyPair::generate_for(alg).unwrap();
        let mut p = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let c = p.self_signed(&key).unwrap();
        (c.pem(), c, key)
    }

    #[test]
    fn signer_names() {
        assert!(signer_name_errors("p", "example.com/foo").is_empty());
        assert!(signer_name_errors("p", "podidentity.podcert.ate.dev/identity").is_empty());
        assert!(!signer_name_errors("p", "").is_empty());
        assert!(!signer_name_errors("p", "example/foo").is_empty());
        assert!(!signer_name_errors("p", "example.com/foo/bar").is_empty());
        assert!(!signer_name_errors("p", "Example.com/foo").is_empty());
        assert!(is_kubernetes_signer("kubernetes.io/kube-apiserver-client-pod"));
        assert!(is_kubernetes_signer("x.kubernetes.io/a"));
        assert!(!is_kubernetes_signer("example.com/a"));
    }

    #[test]
    fn trust_bundles() {
        let (a, ..) = ca_pem(&rcgen::PKCS_ECDSA_P256_SHA256);
        let (b, ..) = ca_pem(&rcgen::PKCS_ED25519);
        let ctb = |name: &str, signer: &str, bundle: &str| json!({"metadata": {"name": name}, "spec": {"signerName": signer, "trustBundle": bundle}});
        assert_eq!(validate("ClusterTrustBundle", &ctb("example.com:foo:v1", "example.com/foo", &format!("{a}{b}")), None), Vec::<String>::new());
        assert_eq!(validate("ClusterTrustBundle", &ctb("plain", "", &a), None), Vec::<String>::new());
        let errs = validate("ClusterTrustBundle", &ctb("plain", "example.com/foo", &a), None);
        assert!(errs[0].contains("must be named with prefix example.com:foo:"), "{errs:?}");
        assert!(validate("ClusterTrustBundle", &ctb("a:b", "", &a), None)[0].contains("must not have \":\""));
        assert!(validate("ClusterTrustBundle", &ctb("x", "", &format!("{a}{a}")), None)[0].contains("duplicate trust anchor"));
        assert!(validate("ClusterTrustBundle", &ctb("x", "", ""), None)[0].contains("at least one trust anchor"));
        let leaf_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let leaf = rcgen::CertificateParams::new(vec!["a".into()]).unwrap().self_signed(&leaf_key).unwrap().pem();
        assert!(validate("ClusterTrustBundle", &ctb("x", "", &leaf), None)[0].contains("CA bit"));
        let key = x509::pem_encode("PRIVATE KEY", b"x");
        assert!(validate("ClusterTrustBundle", &ctb("x", "", &key), None)[0].contains("bad block type"));
        // signerName is immutable.
        let old = ctb("example.com:foo:v1", "example.com/foo", &a);
        let mut new = old.clone();
        new["spec"]["signerName"] = json!("example.com/bar");
        assert!(validate("ClusterTrustBundle", &new, Some(&old)).iter().any(|e| e.contains("immutable")));
    }

    fn pcr(stub: &[u8]) -> Value {
        json!({"metadata": {"name": "p", "namespace": "ns"}, "spec": {
            "signerName": "example.com/foo", "podName": "web", "podUID": "pod-uid",
            "serviceAccountName": "default", "serviceAccountUID": "sa-uid",
            "nodeName": "n1", "nodeUID": "node-uid", "maxExpirationSeconds": 86400,
            "stubPKCS10Request": base64::engine::general_purpose::STANDARD.encode(stub),
        }})
    }

    fn csr(key: &rcgen::KeyPair) -> Vec<u8> {
        let mut p = rcgen::CertificateParams::default();
        p.distinguished_name = rcgen::DistinguishedName::new();
        p.serialize_request(key).unwrap().der().to_vec()
    }

    #[test]
    fn pod_certificate_requests() {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let ok = pcr(&csr(&key));
        assert_eq!(validate("PodCertificateRequest", &ok, None), Vec::<String>::new());
        let mut bad = ok.clone();
        bad["spec"]["maxExpirationSeconds"] = json!(60);
        assert!(validate("PodCertificateRequest", &bad, None)[0].contains("must be in the range [3600, 7862400]"));
        let mut bad = ok.clone();
        bad["spec"]["signerName"] = json!("kubernetes.io/foo");
        bad["spec"]["maxExpirationSeconds"] = json!(7 * 86400);
        assert!(validate("PodCertificateRequest", &bad, None)[0].contains("[3600, 86400]"));
        let mut bad = ok.clone();
        bad["spec"].as_object_mut().unwrap().remove("stubPKCS10Request");
        assert!(validate("PodCertificateRequest", &bad, None)[0].contains("exactly one of"));
        let mut bad = ok.clone();
        bad["spec"]["unverifiedUserAnnotations"] = json!({"nodomain": "x"});
        assert!(validate("PodCertificateRequest", &bad, None)[0].contains("domain-prefixed"));
        // v1beta1's pkixPublicKey + proofOfPossession (the pod UID, signed).
        let ed = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        let pkcs8 = ed.serialize_der();
        let signer = ring::signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(&pkcs8).unwrap();
        let mut beta = ok.clone();
        let spec = beta["spec"].as_object_mut().unwrap();
        spec.remove("stubPKCS10Request");
        spec.insert("pkixPublicKey".into(), json!(base64::engine::general_purpose::STANDARD.encode(ed.public_key_der())));
        spec.insert("proofOfPossession".into(), json!(base64::engine::general_purpose::STANDARD.encode(signer.sign(b"pod-uid").as_ref())));
        assert_eq!(validate("PodCertificateRequest", &beta, None), Vec::<String>::new());
        beta["spec"]["podUID"] = json!("other");
        assert!(validate("PodCertificateRequest", &beta, None)[0].contains("proofOfPossession"));
        // The spec is immutable.
        let mut changed = ok.clone();
        changed["spec"]["podName"] = json!("x");
        assert!(validate("PodCertificateRequest", &changed, Some(&ok))[0].contains("immutable"));
        let mut labeled = ok.clone();
        labeled["metadata"]["labels"] = json!({"a": "b"});
        assert!(validate("PodCertificateRequest", &labeled, Some(&ok)).is_empty());
        // v1 drops the v1beta1-only fields.
        let mut rt = crate::kube::registry::Registry::with_builtins().find(GROUP, "v1", "podcertificaterequests").unwrap();
        let mut both = ok.clone();
        both["spec"]["pkixPublicKey"] = json!("AA==");
        let v1 = serve_as(&rt, both.clone());
        assert_eq!(v1["apiVersion"], "certificates.k8s.io/v1");
        assert!(v1.pointer("/spec/pkixPublicKey").is_none());
        rt.version = "v1beta1".into();
        assert!(serve_as(&rt, both).pointer("/spec/pkixPublicKey").is_some());
    }

    fn issue(pcr: &Value, key: &rcgen::KeyPair, lifetime: i64, refresh_after: i64) -> (Value, i64) {
        let (_, ca, ca_key) = ca_pem(&rcgen::PKCS_ED25519);
        let now = chrono::Utc::now().timestamp();
        let mut p = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        p.not_before = time::OffsetDateTime::from_unix_timestamp(now).unwrap();
        p.not_after = time::OffsetDateTime::from_unix_timestamp(now + lifetime).unwrap();
        let leaf = p.signed_by(key, &ca, &ca_key).unwrap();
        let ts = |t: i64| chrono::DateTime::from_timestamp(t, 0).unwrap().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let mut out = pcr.clone();
        out["status"] = json!({
            "conditions": [{"type": "Issued", "status": "True", "reason": "Reason", "message": "", "lastTransitionTime": ts(now)}],
            "certificateChain": format!("{}{}", leaf.pem(), ca.pem()),
            "notBefore": ts(now), "notAfter": ts(now + lifetime), "beginRefreshAt": ts(now + refresh_after),
        });
        (out, now)
    }

    #[test]
    fn status_updates() {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let old = pcr(&csr(&key));
        let (issued, now) = issue(&old, &key, 7200, 3600);
        assert_eq!(validate_pcr_status(&issued, &old, now), Vec::<String>::new());
        // Once issued, the status is frozen.
        let mut again = issued.clone();
        again["status"]["certificateChain"] = json!("");
        assert!(validate_pcr_status(&again, &issued, now)[0].contains("immutable"));
        // Another key's certificate.
        let other = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let (wrong, now) = issue(&old, &other, 7200, 3600);
        assert!(validate_pcr_status(&wrong, &old, now)[0].contains("not issued to the requested public key"));
        let (short, now) = issue(&old, &key, 1800, 900);
        assert!(validate_pcr_status(&short, &old, now)[0].contains(">= 1 hour"));
        let (late, now) = issue(&old, &key, 7200, 7000);
        assert!(validate_pcr_status(&late, &old, now)[0].contains("10 minutes before"));
        // Denied: conditions only.
        let mut denied = old.clone();
        denied["status"] = json!({"conditions": [{"type": "Denied", "status": "True", "reason": "UnsupportedKeyType", "message": "use ED25519", "lastTransitionTime": "2026-01-01T00:00:00Z"}]});
        assert!(validate_pcr_status(&denied, &old, now).is_empty());
        assert_eq!(pcr_outcome(&denied), Err("Denied: UnsupportedKeyType: use ED25519".into()));
        let mut two = denied.clone();
        two["status"]["conditions"].as_array_mut().unwrap().push(json!({"type": "Failed", "status": "True", "reason": "X", "lastTransitionTime": "2026-01-01T00:00:00Z"}));
        assert!(validate_pcr_status(&two, &old, now)[0].contains("at most one"));
        let mut odd = old.clone();
        odd["status"] = json!({"conditions": [{"type": "Pending", "status": "True", "reason": "X", "lastTransitionTime": "2026-01-01T00:00:00Z"}]});
        assert!(validate_pcr_status(&odd, &old, now)[0].contains("Unsupported value"));
    }

    #[test]
    fn pod_projections() {
        let pod = |sources: Value| json!({"spec": {"volumes": [{"name": "v", "projected": {"sources": sources}}]}});
        let ok = pod(json!([
            {"podCertificate": {"signerName": "example.com/foo", "keyType": "ECDSAP256", "credentialBundlePath": "creds.pem", "userAnnotations": {"example.com/a": "b"}}},
            {"podCertificate": {"signerName": "example.com/foo", "keyType": "ED25519", "keyPath": "k.pem", "certificateChainPath": "c.pem", "maxExpirationSeconds": 3600}},
            {"clusterTrustBundle": {"signerName": "example.com/foo", "labelSelector": {}, "path": "ca.pem"}},
            {"clusterTrustBundle": {"name": "example.com:foo:v1", "path": "ca2.pem", "optional": true}},
            {"serviceAccountToken": {"path": "token"}},
        ]));
        assert_eq!(projection_errors(&ok), Vec::<String>::new());
        let errs = projection_errors(&pod(json!([
            {"podCertificate": {"signerName": "foo", "keyType": "DSA", "maxExpirationSeconds": 60}},
            {"clusterTrustBundle": {"path": "/abs"}},
            {"clusterTrustBundle": {"name": "a", "signerName": "example.com/x", "path": "x"}},
            {"podCertificate": {"signerName": "example.com/x", "keyType": "ED25519", "keyPath": "x"}},
        ])));
        let joined = errs.join("\n");
        for want in [
            "sources[0].podCertificate.signerName: Invalid value",
            "sources[0].podCertificate.keyType: Unsupported value: \"DSA\"",
            "sources[0].podCertificate.maxExpirationSeconds: Invalid value: 60",
            "sources[0].podCertificate: Required value: specify at least one of",
            "sources[1].clusterTrustBundle: Required value: either name or signerName",
            "sources[1].clusterTrustBundle.path: Invalid value: \"/abs\": must be a relative path",
            "sources[2].clusterTrustBundle: Invalid value",
            "projected: Invalid value: \"x\": conflicting duplicate paths",
        ] {
            assert!(joined.contains(want), "missing {want:?} in\n{joined}");
        }
    }

    #[test]
    fn bindings() {
        let p = pcr(b"");
        let pod = json!({"metadata": {"uid": "pod-uid"}, "spec": {"nodeName": "n1"}});
        let sa = json!({"metadata": {"uid": "sa-uid"}});
        let node = json!({"metadata": {"uid": "node-uid"}});
        assert!(binding_errors(&p, Some(&pod), Some(&sa), Some(&node)).is_empty());
        assert!(binding_errors(&p, None, None, None)[0].contains("pod not found"));
        let moved = json!({"metadata": {"uid": "pod-uid"}, "spec": {"nodeName": "n2"}});
        assert!(binding_errors(&p, Some(&moved), Some(&sa), Some(&node))[0].contains("spec.nodeName"));
        let mut other_sa = p.clone();
        other_sa["spec"]["serviceAccountName"] = json!("admin");
        assert!(binding_errors(&other_sa, Some(&pod), Some(&sa), Some(&node))[0].contains("spec.serviceAccountName"));
    }
}
