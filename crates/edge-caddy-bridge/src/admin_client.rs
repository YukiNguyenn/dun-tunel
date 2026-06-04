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

    /// Idempotent removal. Caddy DELETE qua `/id/<name>` đôi khi
    /// không cleanup hoàn toàn entry trong server routes array (đặc
    /// biệt khi entry được POST qua `/...routes/...` thay vì PUT).
    /// Workaround: verify sau DELETE, nếu route vẫn tồn tại thì
    /// fallback DELETE bằng absolute path qua server.routes index.
    pub async fn remove_route(&self, host: &str) -> EdgeResult<()> {
        let id = route_id(host);
        let id_url = format!("{}/id/{}", self.inner.base_url, id);
        tracing::info!(%host, %id, "caddy remove_route: starting");

        // Step 1: DELETE qua @id alias. Mostly work, nhưng có race.
        let resp = self
            .inner
            .http
            .delete(&id_url)
            .send()
            .await
            .map_err(|e| EdgeError::Config(format!("caddy delete route: {e}")))?;
        let status = resp.status();
        tracing::info!(%id, ?status, "caddy remove_route: DELETE /id alias");
        if !status.is_success() && status.as_u16() != 404 {
            let body = resp.text().await.unwrap_or_default();
            return Err(EdgeError::Config(format!(
                "caddy delete route failed: status={status} body={body}"
            )));
        }

        // Step 2: verify entry đã thực sự gone. Caddy thỉnh thoảng
        // giữ stale entry trong server.routes array dù alias đã xóa.
        let probe = self
            .inner
            .http
            .get(&id_url)
            .send()
            .await
            .map_err(|e| EdgeError::Config(format!("caddy verify route deletion: {e}")))?;
        let probe_status = probe.status();
        tracing::info!(%id, ?probe_status, "caddy remove_route: verify GET /id");
        if probe_status.as_u16() == 404 {
            self.inner.routes.remove(host);
            return Ok(());
        }

        // Step 3: fallback — scan server routes array, tìm index có
        // matching @id, DELETE absolute path. Dùng được khi Caddy bị
        // alias-leak.
        let server_name = self.first_server_name().await?;
        let routes_url = format!(
            "{}/config/apps/http/servers/{}/routes",
            self.inner.base_url, server_name
        );
        tracing::info!(%server_name, "caddy remove_route: fallback scan routes array");
        let routes_resp = self
            .inner
            .http
            .get(&routes_url)
            .send()
            .await
            .map_err(|e| EdgeError::Config(format!("caddy fetch routes: {e}")))?;
        let routes_json: serde_json::Value = routes_resp
            .json()
            .await
            .map_err(|e| EdgeError::Config(format!("caddy routes parse: {e}")))?;
        let routes_arr = routes_json.as_array().ok_or_else(|| {
            EdgeError::Config(format!("caddy routes not an array: {routes_json}"))
        })?;
        tracing::info!(
            count = routes_arr.len(),
            "caddy remove_route: scanning routes array"
        );

        let mut found = false;
        // Iterate từ cuối về đầu để DELETE bằng index không invalidate
        // các index sau (sau khi xóa entry i, các entry > i shift lên).
        for (idx, entry) in routes_arr.iter().enumerate().rev() {
            let entry_id = entry.get("@id").and_then(|v| v.as_str()).unwrap_or("");
            if entry_id == id.as_str() {
                let abs_url = format!("{}/{}", routes_url, idx);
                tracing::info!(%abs_url, "caddy remove_route: DELETE absolute");
                let del = self
                    .inner
                    .http
                    .delete(&abs_url)
                    .send()
                    .await
                    .map_err(|e| EdgeError::Config(format!("caddy delete absolute: {e}")))?;
                let del_status = del.status();
                if !del_status.is_success() {
                    let body = del.text().await.unwrap_or_default();
                    return Err(EdgeError::Config(format!(
                        "caddy delete absolute failed: status={del_status} body={body}"
                    )));
                }
                found = true;
                break;
            }
        }
        if !found {
            tracing::warn!(%id, "caddy remove_route: probe said exists but @id not in array");
        }
        self.inner.routes.remove(host);
        Ok(())
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
        let host = "abc123.sin.dun-studio.xyz";
        assert_eq!(route_id(host), route_id(host));
    }
}
