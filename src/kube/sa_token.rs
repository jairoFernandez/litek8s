//! Service account tokens: ES256-signed JWTs with the claims the real API
//! server issues, verifiable through the OIDC discovery documents
//! (`/.well-known/openid-configuration`, `/openid/v1/jwks`).

use std::path::Path;

use anyhow::{Context, Result};
use base64::Engine;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED, ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair, UnparsedPublicKey};
use serde_json::{Value, json};

pub const ISSUER: &str = "https://kubernetes.default.svc.cluster.local";
/// Audiences a token gets when the request names none.
pub const DEFAULT_AUDIENCES: [&str; 2] = [ISSUER, "k3s"];

pub struct TokenSigner {
    key: EcdsaKeyPair,
    kid: String,
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

impl TokenSigner {
    /// The signing key from `<data_dir>/sa.key` (PKCS#8), created if missing;
    /// in memory without a data directory.
    pub fn load_or_create(data_dir: Option<&Path>) -> Result<Self> {
        let rng = SystemRandom::new();
        let path = data_dir.map(|d| d.join("sa.key"));
        let pkcs8 = match path.as_ref().and_then(|p| std::fs::read(p).ok()) {
            Some(bytes) => bytes,
            None => {
                let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
                    .map_err(|_| anyhow::anyhow!("failed to generate the service account key"))?;
                if let Some(p) = &path {
                    std::fs::write(p, doc.as_ref()).with_context(|| format!("failed to write {}", p.display()))?;
                }
                doc.as_ref().to_vec()
            }
        };
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8, &rng)
            .map_err(|_| anyhow::anyhow!("invalid service account key"))?;
        use sha2::Digest;
        let kid = b64(&sha2::Sha256::digest(key.public_key().as_ref())[..16]);
        Ok(Self { key, kid })
    }

    pub fn sign(&self, claims: &Value) -> String {
        let header = json!({"alg": "ES256", "kid": self.kid});
        let input = format!("{}.{}", b64(header.to_string().as_bytes()), b64(claims.to_string().as_bytes()));
        let sig = self.key.sign(&SystemRandom::new(), input.as_bytes()).map(|s| s.as_ref().to_vec()).unwrap_or_default();
        format!("{input}.{}", b64(&sig))
    }

    /// The claims of a token this server signed, if the signature holds and
    /// it has not expired.
    pub fn verify(&self, token: &str) -> Option<Value> {
        let mut parts = token.split('.');
        let (h, c, sig) = (parts.next()?, parts.next()?, parts.next()?);
        let decode = |p: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(p.trim_end_matches('=')).ok();
        let sig = decode(sig)?;
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, self.key.public_key().as_ref())
            .verify(format!("{h}.{c}").as_bytes(), &sig)
            .ok()?;
        let claims: Value = serde_json::from_slice(&decode(c)?).ok()?;
        let now = chrono::Utc::now().timestamp();
        if claims.get("exp").and_then(Value::as_i64).is_some_and(|exp| exp < now) {
            return None;
        }
        Some(claims)
    }

    pub fn jwks(&self) -> Value {
        let point = self.key.public_key().as_ref();
        json!({"keys": [{
            "use": "sig", "kty": "EC", "kid": self.kid, "crv": "P-256", "alg": "ES256",
            "x": b64(&point[1..33]), "y": b64(&point[33..65]),
        }]})
    }
}

pub fn openid_configuration() -> Value {
    json!({
        "issuer": ISSUER,
        "jwks_uri": format!("{ISSUER}/openid/v1/jwks"),
        "response_types_supported": ["id_token"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["ES256"],
    })
}

/// What a token is bound to besides its service account.
#[derive(Default)]
pub struct Binding {
    pub pod: Option<(String, String)>,
    pub secret: Option<(String, String)>,
    pub node: Option<(String, String)>,
}

pub fn claims(namespace: &str, sa_name: &str, sa_uid: &str, audiences: &[String], expiration: i64, binding: &Binding) -> Value {
    let now = chrono::Utc::now().timestamp();
    let mut k8s = json!({"namespace": namespace, "serviceaccount": {"name": sa_name, "uid": sa_uid}});
    if let Some((name, uid)) = &binding.pod {
        k8s["pod"] = json!({"name": name, "uid": uid});
    }
    if let Some((name, uid)) = &binding.secret {
        k8s["secret"] = json!({"name": name, "uid": uid});
    }
    if let Some((name, uid)) = &binding.node {
        k8s["node"] = json!({"name": name, "uid": uid});
    }
    let mut jti = [0u8; 16];
    use rand_core::RngCore;
    rand_core::OsRng.fill_bytes(&mut jti);
    let jti = hex::encode(jti);
    json!({
        "aud": audiences,
        "exp": now + expiration,
        "iat": now,
        "nbf": now,
        "iss": ISSUER,
        "jti": format!("{}-{}-{}-{}-{}", &jti[0..8], &jti[8..12], &jti[12..16], &jti[16..20], &jti[20..32]),
        "kubernetes.io": k8s,
        "sub": format!("system:serviceaccount:{namespace}:{sa_name}"),
    })
}

/// The user a verified token's claims name (TokenReview status.user).
pub fn user_of(claims: &Value) -> Value {
    let s = |p: &str| claims.pointer(p).and_then(Value::as_str).unwrap_or_default().to_string();
    let ns = s("/kubernetes.io/namespace");
    let mut extra = serde_json::Map::new();
    extra.insert("authentication.kubernetes.io/credential-id".into(), json!([format!("JTI={}", s("/jti"))]));
    for (kind, key) in [("pod", "pod"), ("node", "node")] {
        if claims.pointer(&format!("/kubernetes.io/{kind}")).is_some() {
            extra.insert(format!("authentication.kubernetes.io/{key}-name"), json!([s(&format!("/kubernetes.io/{kind}/name"))]));
            extra.insert(format!("authentication.kubernetes.io/{key}-uid"), json!([s(&format!("/kubernetes.io/{kind}/uid"))]));
        }
    }
    json!({
        "username": s("/sub"),
        "uid": s("/kubernetes.io/serviceaccount/uid"),
        "groups": ["system:serviceaccounts", format!("system:serviceaccounts:{ns}"), "system:authenticated"],
        "extra": extra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let signer = TokenSigner::load_or_create(None).unwrap();
        let c = claims("ns", "sa", "u1", &["aud".into()], 60, &Binding { pod: Some(("p".into(), "pu".into())), ..Default::default() });
        let token = signer.sign(&c);
        let back = signer.verify(&token).unwrap();
        assert_eq!(back["sub"], "system:serviceaccount:ns:sa");
        let user = user_of(&back);
        assert_eq!(user["extra"]["authentication.kubernetes.io/pod-name"], json!(["p"]));
        assert!(user["extra"]["authentication.kubernetes.io/credential-id"][0].as_str().unwrap().starts_with("JTI="));
        assert!(signer.verify(&format!("{token}x")).is_none());
        assert_eq!(signer.jwks()["keys"][0]["crv"], "P-256");
    }
}
