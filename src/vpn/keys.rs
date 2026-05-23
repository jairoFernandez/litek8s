//! WireGuard key generation and (de)serialization.
//!
//! WireGuard keys are X25519. Public keys are base64-encoded 32-byte values,
//! matching the `wg` tool format so configs are interoperable.

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey as X25519Public, StaticSecret};

const KEY_LEN: usize = 32;

/// Base64-encoded WireGuard public key. Safe to share, persist, replicate via raft.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PublicKey(String);

impl PublicKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    pub fn parse(value: &str) -> Result<Self> {
        let decoded = BASE64
            .decode(value.trim())
            .context("public key is not valid base64")?;
        if decoded.len() != KEY_LEN {
            bail!(
                "public key has {} bytes after base64 decoding, expected {KEY_LEN}",
                decoded.len()
            );
        }
        Ok(Self(value.trim().to_string()))
    }
}

/// Locally-held key pair. The private half MUST never leave the node.
#[derive(Clone)]
pub struct KeyPair {
    secret: StaticSecret,
}

impl KeyPair {
    /// Generate a fresh key pair using OS randomness.
    pub fn generate() -> Self {
        Self {
            secret: StaticSecret::random_from_rng(OsRng),
        }
    }

    /// Derive the public key from the held secret.
    pub fn public(&self) -> PublicKey {
        let public = X25519Public::from(&self.secret);
        PublicKey(BASE64.encode(public.as_bytes()))
    }

    /// Encode the private half as base64. Treat the result as a secret.
    pub fn secret_base64(&self) -> String {
        BASE64.encode(self.secret.to_bytes())
    }

    pub fn from_secret_base64(value: &str) -> Result<Self> {
        let decoded = BASE64
            .decode(value.trim())
            .context("private key is not valid base64")?;
        if decoded.len() != KEY_LEN {
            bail!(
                "private key has {} bytes after base64 decoding, expected {KEY_LEN}",
                decoded.len()
            );
        }
        let mut bytes = [0u8; KEY_LEN];
        bytes.copy_from_slice(&decoded);
        Ok(Self {
            secret: StaticSecret::from(bytes),
        })
    }
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyPair")
            .field("public", &self.public())
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_distinct_keys() {
        let a = KeyPair::generate();
        let b = KeyPair::generate();
        assert_ne!(a.public(), b.public());
    }

    #[test]
    fn roundtrips_secret_through_base64() {
        let pair = KeyPair::generate();
        let secret = pair.secret_base64();
        let restored = KeyPair::from_secret_base64(&secret).expect("restore");
        assert_eq!(pair.public(), restored.public());
    }

    #[test]
    fn rejects_short_public_key() {
        let bad = BASE64.encode([0u8; 16]);
        assert!(PublicKey::parse(&bad).is_err());
    }

    #[test]
    fn parses_valid_public_key() {
        let pair = KeyPair::generate();
        let encoded = pair.public().into_inner();
        let parsed = PublicKey::parse(&encoded).expect("parse");
        assert_eq!(parsed.as_str(), encoded);
    }

    #[test]
    fn debug_redacts_secret() {
        let pair = KeyPair::generate();
        let dbg = format!("{:?}", pair);
        assert!(dbg.contains("redacted"));
        assert!(!dbg.contains(&pair.secret_base64()));
    }
}
