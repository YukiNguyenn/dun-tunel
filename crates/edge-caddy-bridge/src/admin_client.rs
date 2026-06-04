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
use serde_json::json;
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

    /// Idempotent upsert.
    ///
    /// Strategy (deterministic, easy to verify, no Caddy admin API
    /// quirks):
    ///
    ///   1. PUT `/id/<route_id>` — fast path when route already exists
    ///      (re-share same subdomain on TTL refresh).
    ///   2. On 404: GET full `srv0.routes` array, prepend our new
    ///      route, PATCH the whole array back. PATCH semantics replace
    ///      the value at the path.
    ///   3. Verify by `GET /id/<route_id>` — Caddy returns the route
    ///      JSON when the `@id` is registered. If verify fails we
    ///      surface a clear error rather than reporting "ok" while
    ///      the route silently went missing.
    ///
    /// Why not just `POST /routes/0`? In our deployment that path
    /// returned 200 but the route did not land (auto-restart race or
    /// silent rollback when the request body shape didn't match what
    /// the auto-saved config expected). PATCH-with-full-array gives
    /// us an explicit before/after snapshot we can verify.
    pub async fn add_route(&self, route: CaddyRoute) -> EdgeResult<()> {
        let id = route_id(&route.host);
        let body = build_route(&route);

        // Attempt 1: PUT /id/<route_id> for in-place update.
        let id_url = format!("{}/id/{}", self.inner.base_url, id);
        let resp = self
            .inner
            .http
            .put(&id_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| EdgeError::Config(format!("caddy put route: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            tracing::info!(host = %route.host, %id, "caddy add_route: PUT /id ok");
            self.inner.routes.insert(route.host.clone(), route);
            return self.verify_route_present(&id).await;
        }
        if status.as_u16() != 404 {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(EdgeError::Config(format!(
                "caddy put route failed: status={status} body={body_text}"
            )));
        }

        // Attempt 2: route is brand new. Fetch the routes array,
        // prepend our entry, PATCH it back. We avoid POST /routes/0
        // because in-the-wild it returned 200 without persisting.
        let server_name = self.first_server_name().await?;
        let routes_url = format!(
            "{}/config/apps/http/servers/{}/routes",
            self.inner.base_url, server_name
        );

        let existing: serde_json::Value = self
            .inner
            .http
            .get(&routes_url)
            .send()
            .await
            .map_err(|e| EdgeError::Config(format!("caddy fetch routes: {e}")))?
            .json()
            .await
            .map_err(|e| EdgeError::Config(format!("caddy fetch routes parse: {e}")))?;

        // Filter out `null` entries that legacy DELETE-by-index calls
        // can leave behind (Caddy doesn't always shrink the array; it
        // sets the slot to `null`). Including them in the PATCH would
        // re-persist the holes — harmless to routing but noisy in
        // dumps and they slowly accumulate over time.
        let mut arr: Vec<serde_json::Value> = existing
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|v| !v.is_null())
            .collect();
        arr.insert(0, body);

        let patch_resp = self
            .inner
            .http
            .patch(&routes_url)
            .json(&arr)
            .send()
            .await
            .map_err(|e| EdgeError::Config(format!("caddy patch routes: {e}")))?;
        let patch_status = patch_resp.status();
        if !patch_status.is_success() {
            let body_text = patch_resp.text().await.unwrap_or_default();
            return Err(EdgeError::Config(format!(
                "caddy patch routes failed (server={server_name}): \
                 status={patch_status} body={body_text}"
            )));
        }

        tracing::info!(
            %server_name,
            host = %route.host,
            %id,
            new_routes_len = arr.len(),
            "caddy add_route: PATCH /routes (prepended) ok"
        );
        self.inner.routes.insert(route.host.clone(), route);
        self.verify_route_present(&id).await
    }

    /// Confirm the route landed by querying Caddy's `@id` index.
    /// If the route is not visible after a successful add, surface
    /// an error so callers can roll back the share session instead of
    /// claiming success while the viewer URL goes to 404.
    async fn verify_route_present(&self, id: &str) -> EdgeResult<()> {
        let url = format!("{}/id/{}", self.inner.base_url, id);
        let resp = self
            .inner
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| EdgeError::Config(format!("caddy verify route: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        Err(EdgeError::Config(format!(
            "caddy verify route '{id}' missing after add: status={status}"
        )))
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

    /// Bootstrap a TLS automation policy that issues the wildcard
    /// `*.<region>.<domain>` cert via Cloudflare DNS challenge.
    ///
    /// Why this is needed: we removed the
    /// `*.<region>.<domain>:8443 { tls { dns cloudflare ... } }`
    /// site block from `Caddyfile.tpl` because the Caddyfile adapter
    /// emitted it as a `srv0.routes` entry with `terminal: true`,
    /// which shadowed every dynamic per-session subdomain (404 for
    /// every viewer URL). Without that site block, Caddy still needs
    /// some hint to obtain the wildcard cert via the DNS-01 challenge
    /// — that hint comes from the TLS automation policy installed
    /// here at edge-control startup.
    ///
    /// Idempotent: PUT `/id/dun-tunel-wildcard-tls` overwrites the
    /// existing entry on every call so a config drift never leaves
    /// stale policies behind.
    pub async fn ensure_wildcard_tls_policy(
        &self,
        region: &str,
        domain: &str,
        cloudflare_api_token: &str,
    ) -> EdgeResult<()> {
        let policy_id = "dun-tunel-wildcard-tls";
        let wildcard_subject = format!("*.{}.{}", region, domain);

        // Caddy 2.x policy shape:
        //   {
        //     "subjects": ["*.sin.dun-studio.xyz"],
        //     "issuers": [{
        //       "module": "acme",
        //       "challenges": {
        //         "dns": {
        //           "provider": {
        //             "name": "cloudflare",
        //             "api_token": "<token>"
        //           }
        //         }
        //       }
        //     }]
        //   }
        let policy = json!({
            "@id": policy_id,
            "subjects": [wildcard_subject],
            "issuers": [{
                "module": "acme",
                "challenges": {
                    "dns": {
                        "provider": {
                            "name": "cloudflare",
                            "api_token": cloudflare_api_token,
                        }
                    }
                }
            }]
        });

        // Try @id PUT first — cheapest path on subsequent restarts.
        let id_url = format!("{}/id/{}", self.inner.base_url, policy_id);
        let resp = self
            .inner
            .http
            .put(&id_url)
            .json(&policy)
            .send()
            .await
            .map_err(|e| EdgeError::Config(format!("caddy put tls policy: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            tracing::info!(%wildcard_subject, "caddy ensure_wildcard_tls_policy: PUT /id ok");
            return Ok(());
        }
        if status.as_u16() != 404 {
            let body = resp.text().await.unwrap_or_default();
            return Err(EdgeError::Config(format!(
                "caddy put tls policy failed: status={status} body={body}"
            )));
        }

        // 404 means the @id is unknown — we need to create the
        // policies array (or append into it). Caddy stores policies
        // at `/config/apps/tls/automation/policies`. Probe the array
        // first; if it is missing entirely we PATCH a new array,
        // otherwise prepend our entry.
        let policies_url = format!(
            "{}/config/apps/tls/automation/policies",
            self.inner.base_url
        );
        let probe = self
            .inner
            .http
            .get(&policies_url)
            .send()
            .await
            .map_err(|e| EdgeError::Config(format!("caddy get tls policies: {e}")))?;
        let probe_status = probe.status();

        if probe_status.is_success() {
            let existing: serde_json::Value = probe
                .json()
                .await
                .map_err(|e| EdgeError::Config(format!("caddy tls policies parse: {e}")))?;
            let mut arr = existing.as_array().cloned().unwrap_or_default();
            arr.insert(0, policy);
            let patch = self
                .inner
                .http
                .patch(&policies_url)
                .json(&arr)
                .send()
                .await
                .map_err(|e| EdgeError::Config(format!("caddy patch tls policies: {e}")))?;
            let patch_status = patch.status();
            if !patch_status.is_success() {
                let body = patch.text().await.unwrap_or_default();
                return Err(EdgeError::Config(format!(
                    "caddy patch tls policies failed: status={patch_status} body={body}"
                )));
            }
        } else {
            // Either /apps/tls or /apps/tls/automation does not exist
            // yet (Caddy started with no tls block at all). Create
            // the full path with a single policy. Caddy POST-with-
            // path-creation accepts nested missing paths via PATCH
            // on the parent.
            let automation_url =
                format!("{}/config/apps/tls/automation", self.inner.base_url);
            let body = json!({ "policies": [policy] });
            let patch = self
                .inner
                .http
                .patch(&automation_url)
                .json(&body)
                .send()
                .await
                .map_err(|e| EdgeError::Config(format!("caddy patch tls automation: {e}")))?;
            let patch_status = patch.status();
            if !patch_status.is_success() {
                let body = patch.text().await.unwrap_or_default();
                return Err(EdgeError::Config(format!(
                    "caddy patch tls automation failed: status={patch_status} body={body}"
                )));
            }
        }

        tracing::info!(
            %wildcard_subject,
            "caddy ensure_wildcard_tls_policy: bootstrapped policies array"
        );
        Ok(())
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
