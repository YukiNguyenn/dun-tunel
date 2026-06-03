//! JWT verification cho Tunnel_Token và Viewer_Token.
//! Tham chiếu R6 (HS256, kid rotation, exp ≤ now + 15 min, region match).
//!
//! ## Revocation
//!
//! Edge MUST consult a revocation oracle (typically dun-api `/v1/tunnel/verify`)
//! before accepting a token, to honor jti revocation pushed by dun-api when a
//! session is revoked or refreshed (R6.4, 5b.7). The oracle is injected as an
//! `Arc<dyn RevocationOracle>` so callers can plug in HTTP, in-memory, or test
//! doubles.
//!
//! Fail-CLOSED policy: if the oracle returns Err, we reject the token. This
//! matches dun-api `tunnel-revocation.adapter.ts` behaviour.

use crate::errors::{EdgeError, EdgeResult};
use async_trait::async_trait;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelClaims {
    pub sub: String, // sessionId
    pub aud: String, // "tunnel-server"
    pub exp: i64,
    pub iat: i64,
    pub jti: String,
    pub region: String,
    pub kid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewerClaims {
    pub sub: String, // sessionId
    pub aud: String, // "viewer-exchange"
    pub exp: i64,
    pub iat: i64,
    pub jti: String,
    pub kid: String,
}

/// Pluggable revocation lookup. Implementations should be cheap (in-memory
/// cache hit fast path) so they can be called on every tunnel handshake.
#[async_trait]
pub trait RevocationOracle: Send + Sync {
    /// Returns Ok(true) if `jti` is revoked. Err on lookup failure → callers
    /// MUST treat as revoked (fail-CLOSED).
    async fn is_revoked(&self, jti: &str) -> EdgeResult<bool>;
}

/// Multi-key verifier supporting `kid` rotation (R16.3) + revocation oracle.
pub struct JwtVerifier {
    keys: HashMap<String, DecodingKey>,
    revocation: Option<Arc<dyn RevocationOracle>>,
}

impl JwtVerifier {
    pub fn new() -> Self {
        Self {
            keys: HashMap::new(),
            revocation: None,
        }
    }

    pub fn add_key(&mut self, kid: impl Into<String>, secret: &[u8]) {
        self.keys.insert(kid.into(), DecodingKey::from_secret(secret));
    }

    /// Inject revocation oracle. Recommended for production; absent oracle
    /// means revocation is NOT enforced (only for dev/test).
    pub fn with_revocation(mut self, oracle: Arc<dyn RevocationOracle>) -> Self {
        self.revocation = Some(oracle);
        self
    }

    pub async fn verify_tunnel(
        &self,
        token: &str,
        expected_region: &str,
    ) -> EdgeResult<TunnelClaims> {
        let claims: TunnelClaims = self.decode_with_kid(token, "tunnel-server")?;
        if claims.region != expected_region {
            return Err(EdgeError::RegionMismatch {
                token: claims.region,
                edge: expected_region.to_string(),
            });
        }
        self.check_revocation(&claims.jti).await?;
        Ok(claims)
    }

    pub async fn verify_viewer(&self, token: &str) -> EdgeResult<ViewerClaims> {
        let claims: ViewerClaims = self.decode_with_kid(token, "viewer-exchange")?;
        self.check_revocation(&claims.jti).await?;
        Ok(claims)
    }

    async fn check_revocation(&self, jti: &str) -> EdgeResult<()> {
        if let Some(oracle) = &self.revocation {
            // Fail-CLOSED: oracle error → treat as revoked.
            let revoked = oracle.is_revoked(jti).await.unwrap_or(true);
            if revoked {
                return Err(EdgeError::TokenRevoked(jti.to_string()));
            }
        }
        Ok(())
    }

    fn decode_with_kid<T: for<'de> Deserialize<'de>>(
        &self,
        token: &str,
        expected_aud: &str,
    ) -> EdgeResult<T> {
        let header = decode_header(token)
            .map_err(|e| EdgeError::InvalidToken(format!("header parse: {e}")))?;
        let kid = header
            .kid
            .ok_or_else(|| EdgeError::InvalidToken("missing kid".into()))?;
        let key = self
            .keys
            .get(&kid)
            .ok_or_else(|| EdgeError::InvalidToken(format!("unknown kid: {kid}")))?;
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_audience(&[expected_aud]);
        validation.validate_exp = true;
        let data = decode::<T>(token, key, &validation)
            .map_err(|e| EdgeError::InvalidToken(format!("{e}")))?;
        Ok(data.claims)
    }
}

impl Default for JwtVerifier {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};

    fn make_token(secret: &[u8], claims: &TunnelClaims) -> String {
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(claims.kid.clone());
        encode(&header, claims, &EncodingKey::from_secret(secret)).unwrap()
    }

    fn now_ts() -> i64 {
        chrono::Utc::now().timestamp()
    }

    #[tokio::test]
    async fn verify_tunnel_happy_path() {
        let secret = b"a".repeat(32);
        let claims = TunnelClaims {
            sub: "sess".into(),
            aud: "tunnel-server".into(),
            exp: now_ts() + 600,
            iat: now_ts(),
            jti: "jti1".into(),
            region: "sin".into(),
            kid: "v1".into(),
        };
        let token = make_token(&secret, &claims);

        let mut v = JwtVerifier::new();
        v.add_key("v1", &secret);
        let out = v.verify_tunnel(&token, "sin").await.unwrap();
        assert_eq!(out.jti, "jti1");
    }

    #[tokio::test]
    async fn verify_tunnel_region_mismatch() {
        let secret = b"a".repeat(32);
        let claims = TunnelClaims {
            sub: "sess".into(),
            aud: "tunnel-server".into(),
            exp: now_ts() + 600,
            iat: now_ts(),
            jti: "jti1".into(),
            region: "sin".into(),
            kid: "v1".into(),
        };
        let token = make_token(&secret, &claims);

        let mut v = JwtVerifier::new();
        v.add_key("v1", &secret);
        let err = v.verify_tunnel(&token, "iad").await.unwrap_err();
        assert!(matches!(err, EdgeError::RegionMismatch { .. }));
    }

    struct AlwaysRevoked;
    #[async_trait]
    impl RevocationOracle for AlwaysRevoked {
        async fn is_revoked(&self, _jti: &str) -> EdgeResult<bool> {
            Ok(true)
        }
    }

    struct OracleFails;
    #[async_trait]
    impl RevocationOracle for OracleFails {
        async fn is_revoked(&self, _jti: &str) -> EdgeResult<bool> {
            Err(EdgeError::Config("upstream down".into()))
        }
    }

    #[tokio::test]
    async fn revoked_jti_rejected() {
        let secret = b"a".repeat(32);
        let claims = TunnelClaims {
            sub: "sess".into(),
            aud: "tunnel-server".into(),
            exp: now_ts() + 600,
            iat: now_ts(),
            jti: "jti1".into(),
            region: "sin".into(),
            kid: "v1".into(),
        };
        let token = make_token(&secret, &claims);

        let mut v = JwtVerifier::new();
        v.add_key("v1", &secret);
        let v = v.with_revocation(Arc::new(AlwaysRevoked));
        let err = v.verify_tunnel(&token, "sin").await.unwrap_err();
        assert!(matches!(err, EdgeError::TokenRevoked(_)));
    }

    #[tokio::test]
    async fn oracle_failure_fails_closed() {
        let secret = b"a".repeat(32);
        let claims = TunnelClaims {
            sub: "sess".into(),
            aud: "tunnel-server".into(),
            exp: now_ts() + 600,
            iat: now_ts(),
            jti: "jti1".into(),
            region: "sin".into(),
            kid: "v1".into(),
        };
        let token = make_token(&secret, &claims);

        let mut v = JwtVerifier::new();
        v.add_key("v1", &secret);
        let v = v.with_revocation(Arc::new(OracleFails));
        let err = v.verify_tunnel(&token, "sin").await.unwrap_err();
        assert!(matches!(err, EdgeError::TokenRevoked(_)));
    }
}
