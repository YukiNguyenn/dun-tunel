//! JWKS cache — fetch dun-api's `/api/viewer/jwks` on startup, refresh
//! periodically. Each `kid` maps to a pre-built `DecodingKey` so the
//! per-request fast path is just a `HashMap` lookup + `jsonwebtoken`
//! verify call.
//!
//! Failure handling:
//!   * Initial fetch fail → return Err so `main` exits with a clear
//!     misconfiguration error (better than blocking every request).
//!   * Background refresh fail → keep the previous keys in cache,
//!     log a warning. Eventually the keys expire and Caddy will
//!     start rejecting cookies — the operator sees this in metrics
//!     before users notice.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use jsonwebtoken::DecodingKey;
use serde::Deserialize;
use tokio::sync::RwLock;
/// JWKS document shape we accept from dun-api. Only the fields we
/// care about are listed — extra fields are ignored, future-proofing
/// the verifier across dun-api schema changes.
#[derive(Debug, Deserialize)]
struct JwksDoc {
    keys: Vec<JwkEntry>,
}

#[derive(Debug, Deserialize)]
struct JwkEntry {
    /// Key type — must be `OKP` for Ed25519.
    kty: String,
    /// Curve — must be `Ed25519`.
    crv: Option<String>,
    /// Key id used in JWT header.
    kid: String,
    /// Algorithm hint. We require `EdDSA` because that's what the
    /// dun-api signer emits.
    alg: Option<String>,
    /// Raw 32-byte public key (base64url, no padding).
    x: String,
}

/// Cloneable handle around a shared HashMap of `kid -> DecodingKey`.
/// Reads are cheap (RwLock read guard returns the inner Arc).
#[derive(Clone)]
pub struct JwksCache {
    inner: Arc<RwLock<HashMap<String, Arc<DecodingKey>>>>,
}

impl JwksCache {
    pub async fn fetch(jwks_url: &str, refresh_interval: Duration) -> Result<Self> {
        let initial = fetch_jwks(jwks_url)
            .await
            .with_context(|| format!("initial JWKS fetch from {jwks_url}"))?;
        if initial.is_empty() {
            return Err(anyhow!(
                "JWKS document at {jwks_url} contains 0 keys — dun-api EdDSA keyring may not be initialised yet",
            ));
        }
        let cache = Self {
            inner: Arc::new(RwLock::new(initial)),
        };

        // Background refresh task. We deliberately do NOT propagate
        // errors here — keep the previous keys in cache and log a
        // warn instead so an upstream JWKS hiccup doesn't take the
        // sidecar down.
        let cache_for_task = cache.clone();
        let url = jwks_url.to_string();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(refresh_interval);
            // First tick fires immediately; skip it since we just
            // fetched.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                match fetch_jwks(&url).await {
                    Ok(map) if !map.is_empty() => {
                        *cache_for_task.inner.write().await = map;
                        tracing::info!("JWKS refreshed");
                    }
                    Ok(_) => tracing::warn!("JWKS refresh returned 0 keys, keeping cache"),
                    Err(e) => tracing::warn!(error = %e, "JWKS refresh failed, keeping cache"),
                }
            }
        });

        Ok(cache)
    }

    /// Look up the decoding key for the given kid. Returns None if
    /// not in cache (verifier should reject the token).
    pub async fn get(&self, kid: &str) -> Option<Arc<DecodingKey>> {
        self.inner.read().await.get(kid).cloned()
    }
}

async fn fetch_jwks(url: &str) -> Result<HashMap<String, Arc<DecodingKey>>> {
    let body: JwksDoc = reqwest::get(url)
        .await?
        .error_for_status()?
        .json()
        .await
        .context("JWKS body is not valid JSON")?;
    let mut out = HashMap::new();
    for entry in body.keys {
        if entry.kty != "OKP" {
            tracing::warn!(%entry.kid, kty = %entry.kty, "skipping non-OKP JWKS entry");
            continue;
        }
        if entry.crv.as_deref() != Some("Ed25519") {
            tracing::warn!(%entry.kid, crv = ?entry.crv, "skipping non-Ed25519 JWKS entry");
            continue;
        }
        if entry.alg.as_deref() != Some("EdDSA") {
            tracing::warn!(%entry.kid, alg = ?entry.alg, "skipping non-EdDSA JWKS entry");
            continue;
        }
        let raw = URL_SAFE_NO_PAD.decode(entry.x.as_bytes()).with_context(|| {
            format!("kid {} has malformed x (base64url decode failed)", entry.kid)
        })?;
        if raw.len() != 32 {
            tracing::warn!(
                %entry.kid,
                len = raw.len(),
                "skipping JWKS entry: Ed25519 public key must be exactly 32 bytes",
            );
            continue;
        }
        let key = DecodingKey::from_ed_der(&ed25519_raw_to_der(&raw));
        out.insert(entry.kid, Arc::new(key));
    }
    Ok(out)
}

/// `jsonwebtoken::DecodingKey::from_ed_der` accepts a SubjectPublicKeyInfo
/// DER. The 32-byte raw key needs to be wrapped in a fixed prefix:
///
///   30 2a 30 05 06 03 2b 65 70 03 21 00 <32 bytes>
///
/// where `06 03 2b 65 70` is the OID for Ed25519 (1.3.101.112).
/// This avoids pulling a full ASN.1 encoder for what is effectively a
/// constant prefix.
fn ed25519_raw_to_der(raw: &[u8]) -> Vec<u8> {
    debug_assert_eq!(raw.len(), 32);
    let mut out = Vec::with_capacity(44);
    out.extend_from_slice(&[
        0x30, 0x2a, // SEQUENCE (42 bytes)
        0x30, 0x05, // SEQUENCE (5 bytes) — alg id
        0x06, 0x03, 0x2b, 0x65, 0x70, // OID 1.3.101.112 (Ed25519)
        0x03, 0x21, 0x00, // BIT STRING (33 bytes incl. leading 0x00)
    ]);
    out.extend_from_slice(raw);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_to_der_has_expected_prefix() {
        let raw = [0xAB; 32];
        let der = ed25519_raw_to_der(&raw);
        assert_eq!(der.len(), 44);
        assert_eq!(&der[..12], &[
            0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
        ]);
        assert_eq!(&der[12..], &raw);
    }
}
