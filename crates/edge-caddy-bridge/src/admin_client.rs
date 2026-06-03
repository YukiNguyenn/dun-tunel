//! Caddy admin API client.
//!
//! Default endpoint: `http://127.0.0.1:2019` (localhost-only, never expose).
//! Routes managed via Caddy JSON config tree; idempotent via `@id` selectors (R23.5).

use crate::route_builder::{build_route, route_id};
use dashmap::DashMap;
use edge_shared::errors::{EdgeError, EdgeResult};
use edge_shared::types::CaddyRoute;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct AdminClient {
    inner: Arc<AdminClientInner>,
}

struct AdminClientInner {
    base_url: String,
    routes: DashMap<String, CaddyRoute>,
    http: reqwest::Client,
}

impl AdminClient {
    pub fn new(base_url: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client build");
        Self {
            inner: Arc::new(AdminClientInner {
                base_url,
                routes: DashMap::new(),
                http,
            }),
        }
    }

    pub async fn add_route(&self, route: CaddyRoute) -> EdgeResult<()> {
        let id = route_id(&route.host);
        let body = build_route(&route);
        let url = format!("{}/id/{}", self.inner.base_url, id);
        let resp = self.inner.http.put(&url).json(&body).send().await
            .map_err(|e| EdgeError::Config(format!("caddy put route: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            self.inner.routes.insert(route.host.clone(), route);
            return Ok(());
        }
        let body_text = resp.text().await.unwrap_or_default();
        Err(EdgeError::Config(format!("caddy put route failed: status={status} body={body_text}")))
    }

    pub async fn remove_route(&self, host: &str) -> EdgeResult<()> {
        let id = route_id(host);
        let url = format!("{}/id/{}", self.inner.base_url, id);
        let resp = self.inner.http.delete(&url).send().await
            .map_err(|e| EdgeError::Config(format!("caddy delete route: {e}")))?;
        let status = resp.status();
        if status.is_success() || status.as_u16() == 404 {
            self.inner.routes.remove(host);
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(EdgeError::Config(format!("caddy delete route failed: status={status} body={body}")))
    }

    pub async fn list_routes(&self) -> Vec<CaddyRoute> {
        self.inner.routes.iter().map(|e| e.value().clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn route_id_stable() {
        let host = "abc123.sin.share.dun.app";
        assert_eq!(route_id(host), route_id(host));
    }
}
