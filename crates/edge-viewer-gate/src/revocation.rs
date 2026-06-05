//! In-memory revocation list mirror.
//!
//! Source of truth lives on dun-api (Redis-backed). The sidecar polls
//! `GET <revocation_url>` every few seconds and replaces its local
//! HashSet — verify becomes O(1) per request.
//!
//! When `revocation_url` is not configured the list is permanently
//! empty: signature + exp checks still gate cookies but already-
//! issued tokens stay valid until natural expiry.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::RwLock;

#[derive(Debug, Deserialize)]
struct RevokedListResponse {
    /// Sorted list of currently-revoked jtis. We accept whatever
    /// order; HashSet membership is order-independent.
    jtis: Vec<String>,
}

#[derive(Clone)]
pub struct RevocationList {
    inner: Arc<RwLock<HashSet<String>>>,
}

impl RevocationList {
    /// Spawn the polling task. Caller gets a clone-able handle that
    /// shares the inner set with the task.
    pub fn start(
        url: Option<String>,
        api_key: Option<String>,
        poll_interval: Duration,
    ) -> Self {
        let inner = Arc::new(RwLock::new(HashSet::new()));

        if let Some(url) = url {
            let inner_clone = inner.clone();
            tokio::spawn(async move {
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(5))
                    .build()
                    .expect("reqwest client build");
                let mut ticker = tokio::time::interval(poll_interval);
                loop {
                    ticker.tick().await;
                    let mut req = client.get(&url);
                    if let Some(key) = &api_key {
                        req = req.header("X-Edge-Api-Key", key);
                    }
                    match req.send().await {
                        Ok(resp) => match resp.error_for_status() {
                            Ok(resp) => match resp.json::<RevokedListResponse>().await {
                                Ok(body) => {
                                    let new: HashSet<String> = body.jtis.into_iter().collect();
                                    *inner_clone.write().await = new;
                                }
                                Err(e) => tracing::warn!(error = %e, "revocation list parse failed"),
                            },
                            Err(e) => tracing::warn!(error = %e, "revocation list http error"),
                        },
                        Err(e) => tracing::warn!(error = %e, "revocation list fetch failed"),
                    }
                }
            });
        } else {
            tracing::warn!(
                "REVOCATION_URL not set — sidecar verifies signatures only, cannot block already-issued cookies",
            );
        }

        Self { inner }
    }

    pub async fn contains(&self, jti: &str) -> bool {
        self.inner.read().await.contains(jti)
    }
}
