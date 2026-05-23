//! Join token format.
//!
//! A spoke uses a token to learn the hub's endpoint, public key, and CIDR.
//! The token itself does NOT authenticate the spoke — that comes via SSH
//! bootstrap (the hub registers the spoke's pubkey only over an authenticated
//! SSH session). The token is a convenience envelope for hub metadata.
//!
//! Phase 2 will add a signed nonce and a short expiry; for now the envelope
//! is unsigned.

use std::net::Ipv4Addr;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64;
use serde::{Deserialize, Serialize};

use super::PublicKey;

const TOKEN_PREFIX: &str = "litek8s-vpn-";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinToken {
    pub hub_pubkey: PublicKey,
    pub hub_endpoint: String,
    pub cidr_base: Ipv4Addr,
    pub cidr_prefix: u8,
}

impl JoinToken {
    pub fn encode(&self) -> Result<String> {
        let json = serde_json::to_vec(self).context("failed to serialize join token")?;
        Ok(format!("{TOKEN_PREFIX}{}", BASE64.encode(json)))
    }

    pub fn decode(value: &str) -> Result<Self> {
        let stripped = value
            .trim()
            .strip_prefix(TOKEN_PREFIX)
            .with_context(|| format!("join token must start with `{TOKEN_PREFIX}`"))?;
        let json = BASE64
            .decode(stripped)
            .context("join token payload is not valid base64")?;
        serde_json::from_slice(&json).context("join token payload is not valid JSON")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vpn::keys::KeyPair;

    #[test]
    fn roundtrips_token() {
        let token = JoinToken {
            hub_pubkey: KeyPair::generate().public(),
            hub_endpoint: "54.1.2.3:51820".to_string(),
            cidr_base: Ipv4Addr::new(10, 42, 0, 0),
            cidr_prefix: 24,
        };
        let encoded = token.encode().expect("encode");
        assert!(encoded.starts_with(TOKEN_PREFIX));

        let decoded = JoinToken::decode(&encoded).expect("decode");
        assert_eq!(decoded, token);
    }

    #[test]
    fn rejects_token_without_prefix() {
        assert!(JoinToken::decode("not-a-token").is_err());
    }
}
