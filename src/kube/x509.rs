//! Just enough X.509 / PKCS#10 to validate ClusterTrustBundles and
//! PodCertificateRequests like the API server does (`certificates.rs`):
//! PEM blocks, a certificate's validity, public key and CA bit, a CSR's
//! public key and self-signature. A small DER reader, no ASN.1 crate.

use base64::Engine;
use ring::signature;

/// One PEM block: its label, whether it had headers (`Key: value` lines),
/// and the decoded bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PemBlock {
    pub label: String,
    pub has_headers: bool,
    pub der: Vec<u8>,
}

/// Every PEM block in `text`, skipping data between blocks (like Go's
/// `pem.Decode`). Malformed blocks are errors.
pub fn pem_blocks(text: &str) -> Result<Vec<PemBlock>, String> {
    let mut out = Vec::new();
    let mut lines = text.lines().map(str::trim);
    while let Some(line) = lines.next() {
        let Some(label) = line.strip_prefix("-----BEGIN ").and_then(|l| l.strip_suffix("-----")) else { continue };
        let end = format!("-----END {label}-----");
        let mut body = String::new();
        let mut has_headers = false;
        let mut closed = false;
        for l in lines.by_ref() {
            if l == end {
                closed = true;
                break;
            }
            if l.contains(':') {
                has_headers = true;
                continue;
            }
            body.push_str(l);
        }
        if !closed {
            return Err(format!("PEM block {label:?} is not terminated"));
        }
        let der = base64::engine::general_purpose::STANDARD
            .decode(body.as_bytes())
            .map_err(|e| format!("PEM block {label:?} is not valid base64: {e}"))?;
        out.push(PemBlock { label: label.to_string(), has_headers, der });
    }
    Ok(out)
}

/// PEM-encode `der` with `label` (64-column lines).
pub fn pem_encode(label: &str, der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

// ---- DER

#[derive(Debug, Clone, Copy)]
struct Tlv<'a> {
    tag: u8,
    /// The contents.
    value: &'a [u8],
    /// Tag, length and contents.
    raw: &'a [u8],
}

fn read_tlv(input: &[u8]) -> Result<(Tlv<'_>, &[u8]), String> {
    let bad = || "malformed DER".to_string();
    let (&tag, rest) = input.split_first().ok_or_else(bad)?;
    if tag & 0x1f == 0x1f {
        return Err("unsupported DER tag".into());
    }
    let (&first, mut rest) = rest.split_first().ok_or_else(bad)?;
    let len = if first < 0x80 {
        first as usize
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return Err(bad());
        }
        let mut len = 0usize;
        for &b in &rest[..n] {
            len = (len << 8) | b as usize;
        }
        rest = &rest[n..];
        len
    };
    if rest.len() < len {
        return Err(bad());
    }
    let header = input.len() - rest.len();
    Ok((Tlv { tag, value: &rest[..len], raw: &input[..header + len] }, &rest[len..]))
}

/// Reads the elements of a constructed value one by one.
struct Seq<'a>(&'a [u8]);

impl<'a> Seq<'a> {
    fn next(&mut self) -> Result<Tlv<'a>, String> {
        let (t, rest) = read_tlv(self.0)?;
        self.0 = rest;
        Ok(t)
    }

    fn peek_tag(&self) -> Option<u8> {
        self.0.first().copied()
    }

    fn expect(&mut self, tag: u8) -> Result<Tlv<'a>, String> {
        let t = self.next()?;
        if t.tag != tag {
            return Err(format!("unexpected DER tag 0x{:02x} (want 0x{tag:02x})", t.tag));
        }
        Ok(t)
    }
}

const SEQUENCE: u8 = 0x30;
const OID: u8 = 0x06;
const BIT_STRING: u8 = 0x03;
const OCTET_STRING: u8 = 0x04;
const BOOLEAN: u8 = 0x01;
const INTEGER: u8 = 0x02;

/// The whole input must be one SEQUENCE.
fn sequence(der: &[u8]) -> Result<Seq<'_>, String> {
    let (t, rest) = read_tlv(der)?;
    if t.tag != SEQUENCE || !rest.is_empty() {
        return Err("not a DER SEQUENCE".into());
    }
    Ok(Seq(t.value))
}

fn bit_string(t: &Tlv<'_>) -> Result<Vec<u8>, String> {
    match t.value.split_first() {
        Some((0, bits)) => Ok(bits.to_vec()),
        _ => Err("unsupported BIT STRING".into()),
    }
}

fn oid_string(bytes: &[u8]) -> String {
    let mut parts = Vec::new();
    let mut acc: u64 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        acc = (acc << 7) | (b & 0x7f) as u64;
        if b & 0x80 == 0 {
            if parts.is_empty() && i < bytes.len() {
                let (a, b) = if acc < 40 { (0, acc) } else if acc < 80 { (1, acc - 40) } else { (2, acc - 80) };
                parts.push(a);
                parts.push(b);
            } else {
                parts.push(acc);
            }
            acc = 0;
        }
    }
    parts.iter().map(u64::to_string).collect::<Vec<_>>().join(".")
}

/// UTCTime / GeneralizedTime (`...Z`) as Unix seconds.
fn time(t: &Tlv<'_>) -> Result<i64, String> {
    let s = std::str::from_utf8(t.value).map_err(|_| "bad time")?;
    let s = s.strip_suffix('Z').ok_or("certificate time is not UTC")?;
    let full = match t.tag {
        0x17 if s.len() == 12 => {
            let yy: i32 = s[..2].parse().map_err(|_| "bad time")?;
            format!("{}{s}", if yy >= 50 { "19" } else { "20" })
        }
        0x18 if s.len() == 14 => s.to_string(),
        _ => return Err("unsupported certificate time".into()),
    };
    let dt = chrono::NaiveDateTime::parse_from_str(&full, "%Y%m%d%H%M%S").map_err(|e| format!("bad time: {e}"))?;
    Ok(dt.and_utc().timestamp())
}

// ---- keys

/// The key types PodCertificateRequests may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    Rsa(usize),
    EcP256,
    EcP384,
    EcP521,
    Ed25519,
}

impl KeyType {
    /// The name `keyType` uses (`RSA3072`, `ECDSAP256`, ...).
    pub fn name(&self) -> String {
        match self {
            KeyType::Rsa(bits) => format!("RSA{bits}"),
            KeyType::EcP256 => "ECDSAP256".into(),
            KeyType::EcP384 => "ECDSAP384".into(),
            KeyType::EcP521 => "ECDSAP521".into(),
            KeyType::Ed25519 => "ED25519".into(),
        }
    }
}

/// A parsed SubjectPublicKeyInfo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    pub key_type: KeyType,
    /// The BIT STRING contents (an EC point, an Ed25519 key, an RSAPublicKey).
    pub key: Vec<u8>,
    /// The whole SubjectPublicKeyInfo.
    pub spki: Vec<u8>,
}

pub fn parse_spki(der: &[u8]) -> Result<PublicKey, String> {
    let mut s = sequence(der)?;
    let alg = s.expect(SEQUENCE)?;
    let key = bit_string(&s.expect(BIT_STRING)?)?;
    let mut a = Seq(alg.value);
    let oid = oid_string(a.expect(OID)?.value);
    let key_type = match oid.as_str() {
        "1.2.840.10045.2.1" => match oid_string(a.expect(OID)?.value).as_str() {
            "1.2.840.10045.3.1.7" => KeyType::EcP256,
            "1.3.132.0.34" => KeyType::EcP384,
            "1.3.132.0.35" => KeyType::EcP521,
            other => return Err(format!("unsupported elliptic curve {other}")),
        },
        "1.3.101.112" => KeyType::Ed25519,
        "1.2.840.113549.1.1.1" => {
            let mut rsa = sequence(&key)?;
            let n = rsa.expect(INTEGER)?.value;
            let n = match n.split_first() {
                Some((0, rest)) => rest,
                _ => n,
            };
            KeyType::Rsa(n.len() * 8)
        }
        other => return Err(format!("unsupported public key algorithm {other}")),
    };
    Ok(PublicKey { key_type, key, spki: der.to_vec() })
}

/// Check `sig` over `msg` with `key`, for the signature algorithm `alg_oid`.
fn verify_with(key: &PublicKey, alg_oid: &str, msg: &[u8], sig: &[u8]) -> Result<(), String> {
    let alg: &dyn signature::VerificationAlgorithm = match (alg_oid, key.key_type) {
        ("1.2.840.10045.4.3.2", KeyType::EcP256) => &signature::ECDSA_P256_SHA256_ASN1,
        ("1.2.840.10045.4.3.3", KeyType::EcP256) => &signature::ECDSA_P256_SHA384_ASN1,
        ("1.2.840.10045.4.3.2", KeyType::EcP384) => &signature::ECDSA_P384_SHA256_ASN1,
        ("1.2.840.10045.4.3.3", KeyType::EcP384) => &signature::ECDSA_P384_SHA384_ASN1,
        ("1.3.101.112", KeyType::Ed25519) => &signature::ED25519,
        ("1.2.840.113549.1.1.11", KeyType::Rsa(_)) => &signature::RSA_PKCS1_2048_8192_SHA256,
        ("1.2.840.113549.1.1.12", KeyType::Rsa(_)) => &signature::RSA_PKCS1_2048_8192_SHA384,
        ("1.2.840.113549.1.1.13", KeyType::Rsa(_)) => &signature::RSA_PKCS1_2048_8192_SHA512,
        (alg, kt) => return Err(format!("unsupported signature algorithm {alg} for a {} key", kt.name())),
    };
    signature::UnparsedPublicKey::new(alg, &key.key)
        .verify(msg, sig)
        .map_err(|_| "signature verification failed".to_string())
}

/// Verify a `proofOfPossession` (v1beta1): the signature of the pod UID,
/// as kubelet makes it (Ed25519 over the bytes, ECDSA over their SHA-256).
pub fn verify_proof_of_possession(key: &PublicKey, pod_uid: &str, proof: &[u8]) -> Result<(), String> {
    let alg: &dyn signature::VerificationAlgorithm = match key.key_type {
        KeyType::Ed25519 => &signature::ED25519,
        KeyType::EcP256 => &signature::ECDSA_P256_SHA256_ASN1,
        KeyType::EcP384 => &signature::ECDSA_P384_SHA256_ASN1,
        kt => return Err(format!("proofOfPossession cannot be verified for a {} key; use stubPKCS10Request", kt.name())),
    };
    signature::UnparsedPublicKey::new(alg, &key.key)
        .verify(pod_uid.as_bytes(), proof)
        .map_err(|_| "proofOfPossession does not verify against pkixPublicKey".to_string())
}

// ---- certificates

#[derive(Debug, Clone)]
pub struct Certificate {
    pub not_before: i64,
    pub not_after: i64,
    pub public_key: PublicKey,
    pub is_ca: bool,
}

pub fn parse_certificate(der: &[u8]) -> Result<Certificate, String> {
    let mut cert = sequence(der)?;
    let tbs = cert.expect(SEQUENCE)?;
    cert.expect(SEQUENCE)?; // signatureAlgorithm
    cert.expect(BIT_STRING)?;
    if !cert.0.is_empty() {
        return Err("trailing data in certificate".into());
    }
    let mut t = Seq(tbs.value);
    if t.peek_tag() == Some(0xa0) {
        t.next()?; // [0] version
    }
    t.expect(INTEGER)?; // serialNumber
    t.expect(SEQUENCE)?; // signature
    t.expect(SEQUENCE)?; // issuer
    let mut validity = Seq(t.expect(SEQUENCE)?.value);
    let not_before = time(&validity.next()?)?;
    let not_after = time(&validity.next()?)?;
    t.expect(SEQUENCE)?; // subject
    let public_key = parse_spki(t.expect(SEQUENCE)?.raw)?;
    let mut is_ca = false;
    while !t.0.is_empty() {
        let field = t.next()?;
        if field.tag != 0xa3 {
            continue; // issuerUniqueID, subjectUniqueID
        }
        let mut exts = sequence(field.value)?;
        while !exts.0.is_empty() {
            let mut ext = Seq(exts.expect(SEQUENCE)?.value);
            let oid = oid_string(ext.expect(OID)?.value);
            if ext.peek_tag() == Some(BOOLEAN) {
                ext.next()?;
            }
            let value = ext.expect(OCTET_STRING)?.value;
            if oid == "2.5.29.19" {
                let mut bc = sequence(value)?;
                if bc.peek_tag() == Some(BOOLEAN) {
                    is_ca = bc.next()?.value.first().is_some_and(|b| *b != 0);
                }
            }
        }
    }
    Ok(Certificate { not_before, not_after, public_key, is_ca })
}

// ---- PKCS#10

/// Parse a DER CertificationRequest and check its self-signature; returns
/// the subject's public key.
pub fn verify_csr(der: &[u8]) -> Result<PublicKey, String> {
    let mut csr = sequence(der)?;
    let info = csr.expect(SEQUENCE)?;
    let mut alg = Seq(csr.expect(SEQUENCE)?.value);
    let sig = bit_string(&csr.expect(BIT_STRING)?)?;
    if !csr.0.is_empty() {
        return Err("trailing data in certificate request".into());
    }
    let alg_oid = oid_string(alg.expect(OID)?.value);
    let mut i = Seq(info.value);
    i.expect(INTEGER)?; // version
    i.expect(SEQUENCE)?; // subject
    let key = parse_spki(i.expect(SEQUENCE)?.raw)?;
    verify_with(&key, &alg_oid, info.raw, &sig)?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ca(alg: &'static rcgen::SignatureAlgorithm) -> (rcgen::Certificate, rcgen::KeyPair) {
        let key = rcgen::KeyPair::generate_for(alg).unwrap();
        let mut p = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        (p.self_signed(&key).unwrap(), key)
    }

    #[test]
    fn reads_certificates() {
        for alg in [&rcgen::PKCS_ECDSA_P256_SHA256, &rcgen::PKCS_ECDSA_P384_SHA384, &rcgen::PKCS_ED25519] {
            let (cert, key) = ca(alg);
            let c = parse_certificate(cert.der()).unwrap();
            assert!(c.is_ca);
            assert!(c.not_after > c.not_before);
            assert_eq!(c.public_key.spki, key.public_key_der());
        }
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let leaf = rcgen::CertificateParams::new(vec!["a".to_string()]).unwrap().self_signed(&key).unwrap();
        let c = parse_certificate(leaf.der()).unwrap();
        assert!(!c.is_ca);
        assert_eq!(c.public_key.key_type, KeyType::EcP256);
    }

    #[test]
    fn verifies_csrs() {
        for (alg, kt) in [
            (&rcgen::PKCS_ECDSA_P256_SHA256, KeyType::EcP256),
            (&rcgen::PKCS_ECDSA_P384_SHA384, KeyType::EcP384),
            (&rcgen::PKCS_ED25519, KeyType::Ed25519),
        ] {
            let key = rcgen::KeyPair::generate_for(alg).unwrap();
            let mut p = rcgen::CertificateParams::default();
            p.distinguished_name = rcgen::DistinguishedName::new();
            let csr = p.serialize_request(&key).unwrap();
            let der = csr.der().to_vec();
            let pk = verify_csr(&der).unwrap();
            assert_eq!(pk.key_type, kt);
            assert_eq!(pk.spki, key.public_key_der());
            // A flipped bit in the signature fails.
            let mut bad = der.clone();
            let n = bad.len();
            bad[n - 2] ^= 1;
            assert!(verify_csr(&bad).is_err());
        }
    }

    #[test]
    fn pem_round_trip_and_headers() {
        let text = format!("junk\n{}between\n{}", pem_encode("CERTIFICATE", b"abc"), "-----BEGIN X-----\nProc-Type: 4,ENCRYPTED\nYWJj\n-----END X-----\n");
        let blocks = pem_blocks(&text).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], PemBlock { label: "CERTIFICATE".into(), has_headers: false, der: b"abc".to_vec() });
        assert!(blocks[1].has_headers);
        assert!(pem_blocks("-----BEGIN A-----\nYWJj\n").is_err());
    }

    #[test]
    fn oids() {
        assert_eq!(oid_string(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]), "1.2.840.10045.2.1");
        assert_eq!(oid_string(&[0x2b, 0x65, 0x70]), "1.3.101.112");
        assert_eq!(oid_string(&[0x55, 0x1d, 0x13]), "2.5.29.19");
    }
}
