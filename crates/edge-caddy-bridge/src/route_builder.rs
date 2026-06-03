//! Build Caddy JSON config snippets for a CaddyRoute.
//!
//! Caddy admin API expects route entries in the JSON config tree at
//! `/config/apps/http/servers/srv0/routes/`. Each entry has a deterministic
//! `@id` (= host) so we can PATCH/DELETE individual routes idempotently.
//!
//! WebSocket upgrade is configured via `handle_response` matching `Upgrade: websocket`.

use edge_shared::types::CaddyRoute;
use serde_json::{json, Value};

/// Compute the deterministic `@id` for a route, derived from host.
///
/// Host already contains region + 16 char base36 random suffix so the @id is
/// already low-collision. We strip dots to make it config-tree path-safe.
pub fn route_id(host: &str) -> String {
    format!("dun-tunel-{}", host.replace('.', "_"))
}

/// Build a single Caddy route config object.
pub fn build_route(route: &CaddyRoute) -> Value {
    json!({
        "@id": route_id(&route.host),
        "match": [{
            "host": [route.host.clone()],
        }],
        "handle": [{
            "handler": "subroute",
            "routes": [{
                "handle": [{
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
                }],
            }],
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
            route_id("abc123.sin.share.dun.app"),
            "dun-tunel-abc123_sin_share_dun_app"
        );
    }

    #[test]
    fn build_route_has_deterministic_id() {
        let r = CaddyRoute {
            host: "abc.sin.share.dun.app".into(),
            upstream: "127.0.0.1:11042".into(),
            ws_paths: vec!["/api/ws".into()],
        };
        let v = build_route(&r);
        assert_eq!(v["@id"], "dun-tunel-abc_sin_share_dun_app");
        assert_eq!(v["match"][0]["host"][0], "abc.sin.share.dun.app");
    }
}
