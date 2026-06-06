//! HTTP client for outbound edge-callback events.
//! mTLS + retry exponential backoff (3 attempts: 200ms, 600ms, 1.8s).
//!
//! Wire format: dun-api `POST /tunnels/edge-callback` accepts a flat
//! single-event JSON body discriminated by `event`. We serialise the
//! `EdgeCallbackEvent` enum directly (the enum's `#[serde(tag = "event")]`
//! produces exactly that shape). The legacy `EdgeCallbackBatch`
//! wrapper is kept around for state-snapshot consumers but not used
//! on the callback hot path.

use edge_shared::errors::EdgeResult;
use edge_shared::types::EdgeCallbackEvent;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Normalise the dun-api endpoint URL so the callback path is always
/// `<base>/tunnels/edge-callback` reachable.
///
/// dun-api mounts every route under `/api` via Elysia's autoload
/// `prefix: 'api'`. Operators historically configured both forms:
///
///   - `http://localhost:3010`        ← bare host:port
///   - `http://localhost:3010/api`    ← path included
///   - `https://api.dun-studio.xyz/api` ← prod fronted by reverse proxy
///
/// The early callback URL builder did `format!("{}/tunnels/edge-callback")`
/// directly. Operators using the bare form got 404 silently because
/// the request went to `/tunnels/edge-callback` instead of
/// `/api/tunnels/edge-callback`. We now detect the missing prefix
/// once at client construction and append it. If the endpoint
/// already ends with `/api` (or `/api/`), it is left alone.
fn normalise_endpoint(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('/');
    if trimmed.ends_with("/api") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/api")
    }
}

#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    endpoint: String,
    http: reqwest::Client,
    queue_dir: PathBuf,
    /// Shared API key sent in `X-Edge-Api-Key`. Loaded from
    /// `DUN_API_KEY` env at construction time. dun-api compares
    /// constant-time and rejects with 401 when missing or
    /// mismatching, so a misconfigured edge silently degrades
    /// instead of leaking events. We log on first failed send so
    /// operators see the rejection without parsing dun-api logs.
    api_key: Option<String>,
}

impl Client {
    pub fn new(endpoint: String, _mtls_cert: Option<PathBuf>, queue_dir: PathBuf) -> Self {
        // TODO Phase 4: configure rustls client cert from mtls_cert
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client");
        let api_key = std::env::var("DUN_API_KEY").ok().filter(|s| !s.is_empty());
        if api_key.is_none() {
            tracing::warn!(
                "DUN_API_KEY not set — edge → dun-api callbacks will 401 (dev mode only)"
            );
        }
        let normalised = normalise_endpoint(&endpoint);
        if normalised != endpoint {
            tracing::info!(
                raw = %endpoint,
                normalised = %normalised,
                "normalised DUN_API_ENDPOINT — appended `/api` so callbacks reach Elysia routes"
            );
        }
        Self {
            inner: Arc::new(ClientInner {
                endpoint: normalised,
                http,
                queue_dir,
                api_key,
            }),
        }
    }

    pub async fn send(&self, event: EdgeCallbackEvent) -> EdgeResult<()> {
        for attempt in 0..3 {
            match self.try_send(&event).await {
                Ok(_) => return Ok(()),
                Err(e) if attempt == 2 => {
                    tracing::warn!(error = ?e, "callback send failed after retries — queueing");
                    self.enqueue(&event).await?;
                    return Ok(());
                }
                Err(_) => {
                    let backoff = Duration::from_millis(200 * 3u64.pow(attempt as u32));
                    tokio::time::sleep(backoff).await;
                }
            }
        }
        Ok(())
    }

    async fn try_send(&self, event: &EdgeCallbackEvent) -> anyhow::Result<()> {
        let url = format!("{}/tunnels/edge-callback", self.inner.endpoint);
        let mut req = self.inner.http.post(&url).json(event);
        if let Some(key) = &self.inner.api_key {
            req = req.header("X-Edge-Api-Key", key);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("non-2xx status: {}", resp.status());
        }
        Ok(())
    }

    async fn enqueue(&self, _event: &EdgeCallbackEvent) -> EdgeResult<()> {
        // TODO Phase 2: write to file in queue_dir for retry by background task
        let _ = &self.inner.queue_dir;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::normalise_endpoint;

    #[test]
    fn normalise_endpoint_appends_api_when_missing() {
        assert_eq!(
            normalise_endpoint("http://localhost:3010"),
            "http://localhost:3010/api"
        );
        assert_eq!(
            normalise_endpoint("http://localhost:3010/"),
            "http://localhost:3010/api"
        );
    }

    #[test]
    fn normalise_endpoint_preserves_explicit_api_suffix() {
        assert_eq!(
            normalise_endpoint("http://localhost:3010/api"),
            "http://localhost:3010/api"
        );
        // Trailing slash trimmed, but the `/api` part stays.
        assert_eq!(
            normalise_endpoint("http://localhost:3010/api/"),
            "http://localhost:3010/api"
        );
    }

    #[test]
    fn normalise_endpoint_handles_https_with_subdomain() {
        assert_eq!(
            normalise_endpoint("https://api.dun-studio.xyz"),
            "https://api.dun-studio.xyz/api"
        );
        assert_eq!(
            normalise_endpoint("https://api.dun-studio.xyz/api"),
            "https://api.dun-studio.xyz/api"
        );
    }
}
