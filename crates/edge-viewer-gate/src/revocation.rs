//! In-memory revocation list mirror.
//!
//! Source of truth lives on dun-api (Redis-backed). The sidecar polls
//! `GET <revocation_url>` every few seconds and replaces its local
//! HashSet — verify becomes O(1) per request.
//!
//! Two operating modes (P0 hardening for Option E'):
//!
//!   * **Fail-OPEN** (default) — when polling fails, keep the last
//!     known set in cache. Signature + exp checks still gate
//!     cookies, so the worst case is a revoked-but-not-yet-expired
//!     token surviving its 10-minute TTL window. Acceptable for
//!     internal / closed-beta deployments.
//!
//!   * **Fail-CLOSED** (`REVOCATION_REQUIRED=true`) — when polling
//!     fails for longer than `max_staleness`, the verifier starts
//!     rejecting EVERY cookie until polling recovers. Used for
//!     high-trust deployments where instant revocation matters more
//!     than availability — `bandwidth force-revoke`, abuse take-down,
//!     etc. The trade-off: a dun-api outage now visibly affects
//!     viewers, but a leaked cookie cannot survive past detection.
//!
//! When `revocation_url` is not configured the list is permanently
//! empty AND `is_fresh()` returns `true` (we never expected to poll
//! so there's nothing to be stale about). Strict mode is therefore a
//! no-op when no URL is wired — operators must combine both knobs.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::RwLock;

#[derive(Debug, Deserialize)]
struct RevokedListResponse {
    /// Sorted list of currently-revoked jtis. We accept whatever
    /// order; HashSet membership is order-independent.
    jtis: Vec<String>,
}

/// Inner mutable state. Wrapping in a single RwLock keeps the
/// (set, last_success) update atomic so a verify call cannot read
/// a fresh timestamp paired with a stale set or vice versa.
struct Inner {
    set: HashSet<String>,
    /// `None` until the first successful poll. After that, the
    /// `Instant` of the most recent successful poll. Used by
    /// [`RevocationList::is_fresh`] to decide if strict-mode
    /// verification should fail-CLOSED.
    last_success: Option<Instant>,
    /// Last `ETag` received from dun-api. Sent back as `If-None-Match`
    /// on the next poll so the server can short-circuit with `304
    /// Not Modified` when the list has not changed. Avoids parsing
    /// the body and saves nearly all egress bandwidth on a steady
    /// state where revocations are rare.
    last_etag: Option<String>,
}

#[derive(Clone)]
pub struct RevocationList {
    inner: Arc<RwLock<Inner>>,
    /// Cached configuration so [`is_fresh`] can return the right
    /// answer even when no polling task is running (URL unset).
    polling_enabled: bool,
    max_staleness: Duration,
}

impl RevocationList {
    /// Spawn the polling task. Caller gets a clone-able handle that
    /// shares the inner set with the task.
    ///
    /// `max_staleness` is the strict-mode freshness threshold; only
    /// consulted by the verifier when `REVOCATION_REQUIRED=true`.
    /// Pass `Duration::MAX` to effectively disable strict mode at the
    /// data-structure level (the verifier still has its own toggle).
    pub fn start(
        url: Option<String>,
        api_key: Option<String>,
        poll_interval: Duration,
        max_staleness: Duration,
    ) -> Self {
        let inner = Arc::new(RwLock::new(Inner {
            set: HashSet::new(),
            last_success: None,
            last_etag: None,
        }));
        let polling_enabled = url.is_some();

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
                    // Conditional GET: if we already have a snapshot
                    // dun-api can short-circuit with 304 when the set
                    // has not changed since.
                    let current_etag = {
                        let guard = inner_clone.read().await;
                        guard.last_etag.clone()
                    };
                    if let Some(etag) = &current_etag {
                        req = req.header("If-None-Match", etag);
                    }
                    match req.send().await {
                        Ok(resp) => {
                            // 304 Not Modified — refresh `last_success`
                            // so strict-mode stays happy, keep the set
                            // and etag unchanged.
                            if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
                                let mut guard = inner_clone.write().await;
                                guard.last_success = Some(Instant::now());
                                continue;
                            }
                            // Non-304 success: read the new ETag
                            // BEFORE consuming the body (resp is
                            // moved by `.json()`).
                            let new_etag = resp
                                .headers()
                                .get(reqwest::header::ETAG)
                                .and_then(|v| v.to_str().ok())
                                .map(|s| s.to_string());
                            match resp.error_for_status() {
                                Ok(resp) => match resp.json::<RevokedListResponse>().await {
                                    Ok(body) => {
                                        let new: HashSet<String> =
                                            body.jtis.into_iter().collect();
                                        let mut guard = inner_clone.write().await;
                                        guard.set = new;
                                        guard.last_success = Some(Instant::now());
                                        guard.last_etag = new_etag;
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, "revocation list parse failed")
                                    }
                                },
                                Err(e) => {
                                    tracing::warn!(error = %e, "revocation list http error")
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "revocation list fetch failed")
                        }
                    }
                }
            });
        } else {
            tracing::warn!(
                "REVOCATION_URL not set — sidecar verifies signatures only, cannot block already-issued cookies",
            );
        }

        Self {
            inner,
            polling_enabled,
            max_staleness,
        }
    }

    /// Returns `true` if the given jti is in the local revoked set.
    pub async fn contains(&self, jti: &str) -> bool {
        self.inner.read().await.set.contains(jti)
    }

    /// Whether the local mirror is fresh enough to be trusted by
    /// strict-mode verification.
    ///
    /// Returns `true` when:
    ///   - polling is disabled (no URL configured) — there's nothing
    ///     to be stale about; the verifier should not block on this,
    ///   - OR a successful poll happened within `max_staleness`.
    ///
    /// Returns `false` when polling is enabled but every poll has
    /// failed since startup, or the last success was longer ago than
    /// `max_staleness`. Strict-mode verifier should reject in this
    /// case.
    pub async fn is_fresh(&self) -> bool {
        if !self.polling_enabled {
            return true;
        }
        match self.inner.read().await.last_success {
            None => false,
            Some(t) => t.elapsed() <= self.max_staleness,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_polling_url_means_always_fresh() {
        let r = RevocationList::start(None, None, Duration::from_secs(1), Duration::from_secs(10));
        assert!(r.is_fresh().await);
        assert!(!r.contains("anything").await);
    }

    #[tokio::test]
    async fn polling_url_starts_unfresh() {
        // URL is junk so the poll task will fail; staleness window is
        // a millisecond so the test doesn't have to wait. The point
        // is: polling enabled + no successful poll yet => not fresh.
        let r = RevocationList::start(
            Some("http://127.0.0.1:1/never-listens".to_string()),
            None,
            Duration::from_millis(50),
            Duration::from_millis(1),
        );
        // Give the task a moment to run at least once and fail.
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            !r.is_fresh().await,
            "polling enabled + no success should be unfresh"
        );
    }
}
