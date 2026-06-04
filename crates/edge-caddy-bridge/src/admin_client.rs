//! Caddy admin API client.
//!
//! Default endpoint: `http://127.0.0.1:2019` (localhost-only, never expose).
//!
//! Routes are managed via Caddy's JSON config tree at
//! `/config/apps/http/servers/srv0/routes/...`. We use `@id` selectors so each
//! mutation targets `/id/<route_id>` and is idempotent (R23.5).

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

    /// Idempotent upsert. Try PUT to `/id/<route_id>` first; nếu route
    /// chưa tồn tại Caddy trả 404 → fallback POST vào array routes của
    /// server để insert mới. Lần sau gọi cho cùng host sẽ PUT thành công.
    /// Đây là pattern chuẩn của Caddy admin API cho `@id`-managed objects.
    pub async fn add_route(&self, route: CaddyRoute) -> EdgeResult<()> {
        let id = route_id(&route.host);
        let body = build_route(&route);

        // Attempt 1: PUT /id/<route_id> for in-place update
        let put_url = format!("{}/id/{}", self.inner.base_url, id);
        let resp = self
            .inner
            .http
            .put(&put_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| EdgeError::Config(format!("caddy put route: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            self.inner.routes.insert(route.host.clone(), route);
            return Ok(());
        }
        if status.as_u16() != 404 {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(EdgeError::Config(format!(
                "caddy put route failed: status={status} body={body_text}"
            )));
        }

        // Attempt 2: route chưa tồn tại → POST vào routes array của
        // server đầu tiên Caddy generate. Tên server thường là `srv0`
        // nhưng có thể khác nên fetch list và pick first.
        let server_name = self.first_server_name().await?;
        let post_url = format!(
            "{}/config/apps/http/servers/{}/routes/...",
            self.inner.base_url, server_name
        );
        let resp = self
            .inner
            .http
            .post(&post_url)
            .json(&serde_json::json!([body]))
            .send()
            .await
            .map_err(|e| EdgeError::Config(format!("caddy post route: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(EdgeError::Config(format!(
                "caddy post route failed (server={server_name}): status={status} body={body_text}"
            )));
        }
        self.inner.routes.insert(route.host.clone(), route);
        Ok(())
    }

    /// Pick first HTTP server name từ Caddy config. Dùng để `POST
    /// /config/apps/http/servers/<name>/routes/...` mà không phụ
    /// thuộc hardcoded `srv0`.
    async fn first_server_name(&self) -> EdgeResult<String> {
        let url = format!("{}/config/apps/http/servers/", self.inner.base_url);
        let resp = self
            .inner
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| EdgeError::Config(format!("caddy list servers: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(EdgeError::Config(format!(
                "caddy list servers failed: status={status} body={body}"
            )));
        }
        let map: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| EdgeError::Config(format!("caddy list servers parse: {e}")))?;
        let obj = map
            .as_object()
            .ok_or_else(|| EdgeError::Config("caddy servers list not an object".into()))?;
        obj.keys()
            .next()
            .cloned()
            .ok_or_else(|| EdgeError::Config("caddy servers list empty".into()))
    }

    /// Idempotent removal. 404 → Ok (already absent).
    pub async fn remove_route(&self, host: &str) -> EdgeResult<()> {
        let id = route_id(host);
        let url = format!("{}/id/{}", self.inner.base_url, id);

        let resp = self
            .inner
            .http
            .delete(&url)
            .send()
            .await
            .map_err(|e| EdgeError::Config(format!("caddy delete route: {e}")))?;

        let status = resp.status();
        if status.is_success() || status.as_u16() == 404 {
            self.inner.routes.remove(host);
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(EdgeError::Config(format!(
            "caddy delete route failed: status={status} body={body}"
        )))
    }

    /// Returns local cached view; used by snapshot reconciliation (R22).
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
