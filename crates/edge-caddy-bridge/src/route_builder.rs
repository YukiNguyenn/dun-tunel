//! Build Caddy JSON config snippets for a CaddyRoute.
//!
//! Caddy admin API expects route entries in the JSON config tree at
//! `/config/apps/http/servers/srv0/routes/`. Each entry has a deterministic
//! `@id` (= host) so we can PATCH/DELETE individual routes idempotently.
//!
//! WebSocket upgrade is configured via `handle_response` matching `Upgrade: websocket`.

use edge_shared::types::CaddyRoute;
use serde_json::{json, Value};

/// Path prefixes that the viewer page calls but which actually live
/// on dun-api, not on the dun-browser container behind the rathole
/// tunnel. We split-route them at the edge so:
///
///  * `https://<sub>/viewer/exchange|password|refresh-cookie` →
///    proxied to `dun-api` with a `/api` prefix prepended (Elysia
///    autoload mounts every route under `/api`). The viewer cookie is
///    therefore set with `Domain=<sub>` because dun-api sees the
///    request on the subdomain host — same-origin from the browser's
///    perspective.
///
///  * Everything else → forwarded into the rathole tunnel where the
///    container's API server (port 8080 internal) serves
///    `/viewer/` static files and the WebRTC handshake routes.
///
/// `tunnels/:id/verify-revoked` is the only non-`/viewer/...` path
/// dun-api exposes that the viewer page hits, but the page does not
/// call it directly — `verify-revoked` is server-to-server. We still
/// list it so future viewer-ui changes that probe revocation work
/// without an edge redeploy.
const DUN_API_PATH_PREFIXES: &[&str] = &[
    "/viewer/exchange",
    "/viewer/password",
    "/viewer/refresh-cookie",
];

/// Compute the deterministic `@id` for a route, derived from host.
///
/// Host already contains region + 16 char base36 random suffix so the @id is
/// already low-collision. We strip dots to make it config-tree path-safe.
pub fn route_id(host: &str) -> String {
    format!("dun-tunel-{}", host.replace('.', "_"))
}

/// Build a single Caddy route config object.
///
/// `dun_api_upstream` is the loopback `host:port` of dun-api as seen
/// from the Caddy container (typically `127.0.0.1:3010`). When `None`
/// we skip the split-route block entirely — useful for tests or
/// dev-mode setups that don't run dun-api on the same host.
pub fn build_route(route: &CaddyRoute, dun_api_upstream: Option<&str>) -> Value {
    let tunnel_handle = json!({
        "handler": "reverse_proxy",
        "transport": {
            "protocol": "http",
            "versions": ["1.1", "2"],
        },
        "upstreams": [{ "dial": route.upstream.clone() }],
        // WS frames are tunnelled through reverse_proxy automatically when
        // upstream sends 101 Switching Protocols. No extra config needed.
        "headers": {
            "request": {
                "set": {
                    "X-Forwarded-Host": ["{http.request.host}"],
                    "X-Forwarded-Proto": ["{http.request.scheme}"],
                    "X-Forwarded-For": ["{http.request.remote.host}"],
                }
            }
        },
    });

    // Inner subroute: split traffic between dun-api (cookie-bearing
    // viewer endpoints) and the rathole tunnel (everything else).
    // The dun-api handler runs first because subroute evaluates
    // entries top-to-bottom and stops at the first match.
    let inner_routes = match dun_api_upstream {
        Some(api_upstream) => {
            // Rewrite the path so `/viewer/exchange` becomes
            // `/api/viewer/exchange` before hitting dun-api. Elysia's
            // autoload mounts every route under `prefix: 'api'`
            // (see `dun-api/src/configs/route.ts`).
            let api_handle = json!({
                "group": "viewer-api",
                "handle": [
                    {
                        "handler": "rewrite",
                        "uri": "/api{http.request.uri.path}{http.request.uri.search}",
                    },
                    {
                        "handler": "reverse_proxy",
                        "transport": {
                            "protocol": "http",
                            "versions": ["1.1", "2"],
                        },
                        "upstreams": [{ "dial": api_upstream.to_string() }],
                        "headers": {
                            "request": {
                                "set": {
                                    "X-Forwarded-Host": ["{http.request.host}"],
                                    "X-Forwarded-Proto": ["{http.request.scheme}"],
                                    "X-Forwarded-For": ["{http.request.remote.host}"],
                                }
                            }
                        },
                    }
                ],
                "match": [{
                    "path": DUN_API_PATH_PREFIXES.iter()
                        .flat_map(|p| [p.to_string(), format!("{p}/*")])
                        .collect::<Vec<_>>(),
                }],
            });
            vec![api_handle, json!({ "handle": [tunnel_handle] })]
        }
        None => vec![json!({ "handle": [tunnel_handle] })],
    };

    json!({
        "@id": route_id(&route.host),
        "match": [{
            "host": [route.host.clone()],
        }],
        "handle": [{
            "handler": "subroute",
            "routes": inner_routes,
        }],
        "terminal": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_id_replaces_dots() {
        assert_eq!(
            route_id("abc123.sin.dun-studio.xyz"),
            "dun-tunel-abc123_sin_dun-studio_xyz"
        );
    }

    #[test]
    fn build_route_has_deterministic_id() {
        let r = CaddyRoute {
            host: "abc.sin.dun-studio.xyz".into(),
            upstream: "127.0.0.1:11042".into(),
            ws_paths: vec!["/api/ws".into()],
        };
        let v = build_route(&r, None);
        assert_eq!(v["@id"], "dun-tunel-abc_sin_dun-studio_xyz");
        assert_eq!(v["match"][0]["host"][0], "abc.sin.dun-studio.xyz");
    }

    #[test]
    fn build_route_split_routes_viewer_endpoints_to_api() {
        let r = CaddyRoute {
            host: "abc.sin.dun-studio.xyz".into(),
            upstream: "127.0.0.1:11042".into(),
            ws_paths: vec![],
        };
        let v = build_route(&r, Some("127.0.0.1:3010"));
        let inner = &v["handle"][0]["routes"];
        // First inner route handles dun-api viewer endpoints
        let api_paths = inner[0]["match"][0]["path"].as_array().unwrap();
        let paths: Vec<String> = api_paths
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect();
        assert!(paths.iter().any(|p| p == "/viewer/exchange"));
        assert!(paths.iter().any(|p| p == "/viewer/exchange/*"));
        assert!(paths.iter().any(|p| p == "/viewer/refresh-cookie"));
        // Rewrite + reverse_proxy chained
        let api_handles = inner[0]["handle"].as_array().unwrap();
        assert_eq!(api_handles[0]["handler"], "rewrite");
        assert_eq!(
            api_handles[0]["uri"],
            "/api{http.request.uri.path}{http.request.uri.search}"
        );
        assert_eq!(api_handles[1]["handler"], "reverse_proxy");
        assert_eq!(
            api_handles[1]["upstreams"][0]["dial"],
            "127.0.0.1:3010"
        );
        // Default tail route is the tunnel upstream
        assert_eq!(
            inner[1]["handle"][0]["upstreams"][0]["dial"],
            "127.0.0.1:11042"
        );
    }

    #[test]
    fn build_route_without_api_upstream_keeps_legacy_shape() {
        let r = CaddyRoute {
            host: "abc.sin.dun-studio.xyz".into(),
            upstream: "127.0.0.1:11042".into(),
            ws_paths: vec![],
        };
        let v = build_route(&r, None);
        let inner = v["handle"][0]["routes"].as_array().unwrap();
        // Single tunnel route — no dun-api split when upstream is absent
        assert_eq!(inner.len(), 1);
        assert_eq!(
            inner[0]["handle"][0]["upstreams"][0]["dial"],
            "127.0.0.1:11042"
        );
    }
}
