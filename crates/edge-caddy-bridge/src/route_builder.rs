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
    "/viewer/jwks",
];

/// Path patterns that bypass the cookie auth check entirely.
///
/// These are static viewer-ui-react bundle assets. The HTML at
/// `/viewer/` itself is allowed because it does not contain any
/// session data — it is the JS bundle that loads the share session
/// (and triggers a 401 if the cookie is missing/invalid via
/// `sessionEndedGuard`). Asset paths cover Vite's default output:
///   - `/assets/*` (hashed JS / CSS / fonts)
///   - `/env.js` (runtime config injection)
///   - `/favicon*`
///   - `/viewer` and `/viewer/` (the index.html shell)
///
/// Without this bypass, the very first GET `/viewer/` from a fresh
/// link would 401 because the user has no cookie yet — they must
/// load the page to run the JS that POSTs `/viewer/exchange`.
const VIEWER_PUBLIC_PATHS: &[&str] = &[
    "/viewer",
    "/viewer/",
    "/viewer/index.html",
    "/assets/*",
    "/env.js",
    "/favicon*",
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
/// we skip the dun-api split-route block entirely — useful for
/// tests or dev-mode setups that don't run dun-api on the same host.
///
/// `auth_gate_upstream` is the loopback `host:port` of the
/// `edge-viewer-gate` sidecar (typically `127.0.0.1:9444`). When
/// `Some`, every non-public-asset and non-dun-api request is first
/// `forward_auth`'d to `<auth_gate_upstream>/check`; only 2xx
/// responses pass through to the rathole tunnel. When `None` the
/// auth check is skipped (legacy behaviour for tests / dev).
pub fn build_route(
    route: &CaddyRoute,
    dun_api_upstream: Option<&str>,
    auth_gate_upstream: Option<&str>,
) -> Value {
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
    // viewer endpoints), public asset bypass (viewer HTML + JS), an
    // auth-gated path (forward_auth + tunnel), and a final default
    // tunnel route (for tests when no auth_gate is set).
    //
    // Caddy evaluates entries top-to-bottom and stops at the first
    // match. Order matters:
    //   1. dun-api endpoints (most specific path matchers)
    //   2. public assets (viewer-ui-react bundle)
    //   3. forward_auth → tunnel (when auth_gate set)
    //   4. tunnel default (test / dev fallback)
    let mut inner_routes: Vec<Value> = Vec::new();

    if let Some(api_upstream) = dun_api_upstream {
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
        inner_routes.push(api_handle);
    }

    // Public assets bypass — only emit when auth_gate is active so a
    // dev-mode setup (no auth gate) keeps the legacy "everything to
    // tunnel" behaviour (no need to special-case assets when nothing
    // would block them anyway).
    if auth_gate_upstream.is_some() {
        let public_assets = json!({
            "group": "viewer-public",
            "handle": [tunnel_handle.clone()],
            "match": [{
                "path": VIEWER_PUBLIC_PATHS.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
            }],
        });
        inner_routes.push(public_assets);
    }

    if let Some(gate) = auth_gate_upstream {
        // forward_auth pattern: reverse_proxy to `<gate>/check`. On
        // 2xx the `handle_response` continues with the tunnel; on
        // 4xx/5xx Caddy returns the gate's response directly so the
        // viewer-ui receives a clean 401 (its `sessionEndedGuard`
        // then reloads to trigger the exchange flow).
        //
        // We strip `Content-Length` / body from the sub-request: the
        // gate only inspects headers (Cookie + X-Forwarded-Host) and
        // forwarding the body would prematurely consume the original
        // request. Caddy's reverse_proxy handles this when paired
        // with `rewrite.method=GET`.
        let auth_handle = json!({
            "handle": [{
                "handler": "reverse_proxy",
                "rewrite": {
                    "method": "GET",
                    "uri": "/check"
                },
                "upstreams": [{"dial": gate.to_string()}],
                "headers": {
                    "request": {
                        "set": {
                            "X-Forwarded-Host": ["{http.request.host}"],
                            "X-Forwarded-Method": ["{http.request.method}"],
                            "X-Forwarded-Uri": ["{http.request.uri}"]
                        }
                    }
                },
                "handle_response": [
                    {
                        "match": {"status_code": [2]},
                        "routes": [{
                            "handle": [tunnel_handle.clone()]
                        }]
                    }
                ]
            }],
        });
        inner_routes.push(auth_handle);
    } else {
        // Dev / test fallback — no auth gate, send everything to the
        // tunnel directly.
        inner_routes.push(json!({ "handle": [tunnel_handle] }));
    }

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

/// Build a tail-of-routes Caddy entry that responds `410 Gone` for
/// any host matching `host_pattern` (e.g. `*.sin.dun-studio.xyz`)
/// when no per-session route picks it up first. Used by
/// [`AdminClient::ensure_session_ended_fallback`] so a viewer who
/// hits an expired/revoked URL sees a deliberate "session ended"
/// page instead of a default Caddy fallback.
///
/// The handler emits a small HTML body so the response renders in a
/// browser (not just a bare status line). `Content-Type` and
/// `Cache-Control` are set so a CDN / browser cache does not pin
/// this 410 across a future re-share of the same subdomain.
pub fn build_session_ended_route(id: &str, host_pattern: &str) -> Value {
    json!({
        "@id": id,
        "match": [{
            "host": [host_pattern],
        }],
        "handle": [{
            "handler": "subroute",
            "routes": [{
                "handle": [{
                    "handler": "headers",
                    "response": {
                        "set": {
                            "Content-Type": ["text/html; charset=utf-8"],
                            "Cache-Control": ["no-store"],
                        }
                    }
                }, {
                    "handler": "static_response",
                    "status_code": 410,
                    "body": SESSION_ENDED_BODY,
                }],
            }],
        }],
        "terminal": true,
    })
}

const SESSION_ENDED_BODY: &str = "<!doctype html>\
<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>Session ended</title>\
<style>body{font-family:system-ui,-apple-system,Segoe UI,Roboto,sans-serif;\
background:#0b0d10;color:#e6e6e6;margin:0;display:grid;place-items:center;\
min-height:100vh}main{max-width:32rem;padding:2rem;text-align:center}\
h1{font-size:1.5rem;margin:0 0 .5rem;color:#f5f5f5}\
p{margin:0;color:#9aa0a6;line-height:1.5}\
code{background:#1a1d22;color:#cdd2d8;padding:.1rem .35rem;border-radius:.2rem;\
font-family:ui-monospace,monospace;font-size:.9em}</style></head>\
<body><main><h1>Session ended</h1>\
<p>Người chia sẻ đã đóng phiên này. Đường dẫn không còn hiệu lực.<br>\
Hãy yêu cầu họ tạo phiên mới nếu bạn cần truy cập tiếp.</p>\
</main></body></html>";

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
        let v = build_route(&r, None, None);
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
        let v = build_route(&r, Some("127.0.0.1:3010"), None);
        let inner = &v["handle"][0]["routes"];
        let api_paths = inner[0]["match"][0]["path"].as_array().unwrap();
        let paths: Vec<String> = api_paths
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect();
        assert!(paths.iter().any(|p| p == "/viewer/exchange"));
        assert!(paths.iter().any(|p| p == "/viewer/exchange/*"));
        assert!(paths.iter().any(|p| p == "/viewer/refresh-cookie"));
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
        // No auth gate → tail route is the tunnel upstream
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
        let v = build_route(&r, None, None);
        let inner = v["handle"][0]["routes"].as_array().unwrap();
        assert_eq!(inner.len(), 1);
        assert_eq!(
            inner[0]["handle"][0]["upstreams"][0]["dial"],
            "127.0.0.1:11042"
        );
    }

    #[test]
    fn build_route_with_auth_gate_inserts_forward_auth_block() {
        let r = CaddyRoute {
            host: "abc.sin.dun-studio.xyz".into(),
            upstream: "127.0.0.1:11042".into(),
            ws_paths: vec![],
        };
        let v = build_route(&r, Some("127.0.0.1:3010"), Some("127.0.0.1:9444"));
        let inner = v["handle"][0]["routes"].as_array().unwrap();
        // 0: dun-api split, 1: public assets, 2: auth-gated tunnel
        assert_eq!(inner.len(), 3);

        // public asset bypass
        let public_paths = inner[1]["match"][0]["path"].as_array().unwrap();
        let paths: Vec<String> = public_paths
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect();
        assert!(paths.iter().any(|p| p == "/viewer/"));
        assert!(paths.iter().any(|p| p == "/assets/*"));

        // forward_auth → tunnel
        let auth_handle = &inner[2]["handle"][0];
        assert_eq!(auth_handle["handler"], "reverse_proxy");
        assert_eq!(auth_handle["upstreams"][0]["dial"], "127.0.0.1:9444");
        assert_eq!(auth_handle["rewrite"]["uri"], "/check");
        // 2xx response → forward to tunnel
        let handle_response = auth_handle["handle_response"].as_array().unwrap();
        assert_eq!(handle_response[0]["match"]["status_code"][0], 2);
        assert_eq!(
            handle_response[0]["routes"][0]["handle"][0]["upstreams"][0]["dial"],
            "127.0.0.1:11042"
        );
    }

    #[test]
    fn build_session_ended_route_emits_410_with_html_body() {
        let v = build_session_ended_route(
            "dun-tunel-session-ended-sin",
            "*.sin.dun-studio.xyz",
        );
        assert_eq!(v["@id"], "dun-tunel-session-ended-sin");
        assert_eq!(v["match"][0]["host"][0], "*.sin.dun-studio.xyz");
        assert_eq!(v["terminal"], true);
        let inner = &v["handle"][0]["routes"][0]["handle"];
        // First handler sets headers, second emits 410 + HTML body.
        assert_eq!(inner[0]["handler"], "headers");
        assert_eq!(
            inner[0]["response"]["set"]["Content-Type"][0],
            "text/html; charset=utf-8"
        );
        assert_eq!(
            inner[0]["response"]["set"]["Cache-Control"][0],
            "no-store"
        );
        assert_eq!(inner[1]["handler"], "static_response");
        assert_eq!(inner[1]["status_code"], 410);
        let body = inner[1]["body"].as_str().unwrap();
        assert!(body.contains("Session ended"));
    }
}
