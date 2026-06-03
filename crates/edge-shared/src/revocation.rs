//! HTTP-backed `RevocationOracle` implementation calling dun-api
//! `/v1/tunnel/verify-revoked?jti=...`.
//!
//! Adds a small in-process TTL cache (default 5s) to absorb the burst of
//! per-handshake checks during multi-viewer fan-out. Cache TTL is bounded
//! tight enough that revocations propagate within the SLA (≤ 5s in spec
//! design 16.3).

use crate::errors::{EdgeError, EdgeResult};
use crate::jwt::RevocationOracle;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
struct CacheEntry {
    revoked: bool,
    inserted_at: Instant,
}

pub struct HttpRevocationOracle {
    endpoint: String,
    api_key: String,
    http: reqwest::Client,
    cache: Arc<RwLock<HashMap<String, CacheEntry>>>,
    ttl: Duration,
}

impl HttpRevocationOracle {
    pub fn new(endpoint: String, api_key: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("reqwest client build");
        Self {
            endpoint,
            api_key,
            http,
            cache: Arc::new(RwLock::new(HashMap::new())),
            ttl: DEFAULT_CACHE_TTL,
        }
    }

    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    fn cache_get(&self, jti: &str) -> Option<bool> {
        let cache = self.cache.try_read().ok()?;
        let entry = cache.get(jti)?;
        if entry.inserted_at.elapsed() < self.ttl {
            Some(entry.revoked)
        } else {
            None
        }
    }

    async fn cache_put(&self, jti: &str, revoked: bool) {
        let mut cache = self.cache.write().await;
        // Bound cache size — cheap LRU-ish prune when over 10k entries.
        if cache.len() > 10_000 {
            let cutoff = Instant::now() - self.ttl;
            cache.retain(|_, e| e.inserted_at > cutoff);
        }
        cache.insert(
            jti.to_string(),
            CacheEntry {
                revoked,
                inserted_at: Instant::now(),
            },
        );
    }

    async fn fetch(&self, jti: &str) -> EdgeResult<bool> {
        // dun-api routes are file-based without a `/v1` prefix; the
        // endpoint lives at `/tunnels/verify-revoked` per
        // `dun-api/src/routes/tunnels/verify-revoked.ts`.
        let url = format!("{}/tunnels/verify-revoked", self.endpoint);
        let resp = self
            .http
            .get(&url)
            .query(&[("jti", jti)])
            .header("x-edge-api-key", &self.api_key)
            .send()
            .await
            .map_err(|e| EdgeError::Config(format!("revocation fetch: {e}")))?;

        if !resp.status().is_success() {
            return Err(EdgeError::Config(format!(
                "revocation lookup non-2xx: {}",
                resp.status()
            )));
        }

        #[derive(serde::Deserialize)]
        struct Resp {
            revoked: bool,
        }
        let body: Resp = resp
            .json()
            .await
            .map_err(|e| EdgeError::Config(format!("revocation body: {e}")))?;
        Ok(body.revoked)
    }
}

#[async_trait]
impl RevocationOracle for HttpRevocationOracle {
    async fn is_revoked(&self, jti: &str) -> EdgeResult<bool> {
        if let Some(cached) = self.cache_get(jti) {
            return Ok(cached);
        }
        let revoked = self.fetch(jti).await?;
        self.cache_put(jti, revoked).await;
        Ok(revoked)
    }
}
