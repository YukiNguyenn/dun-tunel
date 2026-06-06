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

/// Path prefixes routed to the local `edge-control` loopback (the
/// SFU signalling crate that owns mediasoup `Router` + `Consumer`
/// per-session). Used by the viewer mediasoup-client to negotiate
/// the WebRTC handshake against the edge SFU instead of the legacy
/// neko WS inside the rathole tunnel.
///
/// Why is this a separate split block (and not just another auth-
/// gated tunnel rewrite)? edge-control lives on the host loopback
/// at `127.0.0.1:9443`, NOT inside the per-session container — so
/// the rathole tunnel upstream cannot serve it. The auth gate still
/// runs in front (forward_auth → 2xx) so only verified cookies can
/// open the SFU WS, with `X-Forwarded-Sub` carrying the share-
/// session id for the handler's `?session=<id>` query check.
const EDGE_CONTROL_PATH_PREFIXES: &[&str] = &["/v1/sfu"];

/// Path patterns that MUST be hard-blocked at the edge, regardless of
/// auth gate decisions, when accessed via a public viewer subdomain.
///
/// Currently: the host-mode SPA shell (`/host*`). The dun-browser
/// container API maps the same `index.html` bundle to both `/viewer`
/// and `/host`, distinguished only by URL path inside the JS. Without
/// this block, a viewer with a valid cookie could navigate to
/// `https://<sub>:8443/host` and the frontend would render the
/// host-mode UI (including mouse/keyboard handlers, settings panel,
/// etc.). The container's loopback middleware would then refuse the
/// follow-up control-plane API calls — so the user wouldn't actually
/// gain control — but the UI would still light up the input
/// affordances and confuse the threat model.
///
/// Belt-and-suspenders: 403 at the edge means the host UI never
/// loads on a public subdomain. Tied with the container loopback
/// gate, an attacker who reverse-engineers the bundle and re-hosts
/// it cannot connect their JS to the live container API either.
///
/// Add new patterns here for any future host-only paths that get
/// added to the container API.
const VIEWER_EDGE_BLOCK_PATHS: &[&str] = &[
    "/host",
    "/host/*",
];

/// Path patterns that bypass the cookie auth check entirely.
///
/// These are static viewer-ui-react bundle assets. The HTML at
/// `/viewer/` itself is allowed because it does not contain any
/// session data — it is the JS bundle that loads the share session
/// (and triggers a 401 if the cookie is missing/invalid via
/// `sessionEndedGuard`). Asset paths cover common Vite output
/// shapes:
///   - `/viewer` and `/viewer/` (the index.html shell)
///   - `/viewer/index.html`
///   - `/static/viewer/*` (default container build with
///     `base: '/static/viewer/'`)
///   - `/assets/*` (Vite default when `base: '/'`)
///   - `/static/*` (catch-all for any other subdir of `/static/`)
///   - `/env.js` (runtime config injection)
///   - `/favicon*`, `/robots.txt`
///
/// Without this bypass, the very first GET `/viewer/` from a fresh
/// link would 401 because the user has no cookie yet — they must
/// load the page to run the JS that POSTs `/viewer/exchange`.
///
/// We err on the side of generous bypass for static asset paths:
/// they contain no session data, so blocking them only breaks the
/// page without adding security. The cookie check still gates the
/// real session traffic — `/ws`, `/api/*`, neko REST, mediasoup
/// signalling — through the catch-all auth-gated tail route.
const VIEWER_PUBLIC_PATHS: &[&str] = &[
    "/viewer",
    "/viewer/",
    "/viewer/index.html",
    "/static/*",
    "/assets/*",
    "/env.js",
    "/favicon*",
    "/robots.txt",
];

/// Build the response-header `set` block that Caddy applies to every
/// viewer-subdomain response. These are defense-in-depth headers
/// hardening the viewer-ui-react bundle and any neko UI it loads
/// against XSS, clickjacking, MIME confusion, and referrer leaks
/// (security review W7).
///
/// Why set them HERE (Caddy edge) rather than in dun-api or the
/// container: the viewer subdomain is the only origin the viewer
/// page ever runs at. Centralising the policy at the proxy means a
/// single point to audit, no per-route forgetting, and it survives
/// container upgrades. The dun-api `helmet` plugin already sets
/// near-identical headers for `api.<domain>`, so this just brings
/// the viewer namespace to parity.
///
/// Header rationale:
///
///  * **Content-Security-Policy** — the viewer-ui-react bundle is
///    a static SPA that talks ONLY to its own origin (`'self'`) for
///    HTTP, WS, and the WebRTC signaling endpoint. We allow inline
///    styles because Vite hash-injects critical CSS at build, and
///    inline scripts because the bundle's runtime config splat
///    (`window.__ENV__ = {...}`) lives in `index.html`. We do
///    NOT allow `unsafe-eval` so a compromised dependency cannot
///    construct functions from strings. `connect-src` covers fetch
///    + XHR + WS + EventSource + Beacon; `media-src blob:` lets
///    `<video>` play decoded WebRTC streams.
///  * **X-Content-Type-Options: nosniff** — block MIME confusion
///    where browsers second-guess `Content-Type` and execute a
///    text file as JS.
///  * **X-Frame-Options: DENY** — viewer page must NEVER be iframed
///    by another site. Combined with `frame-ancestors 'none'` for
///    browsers that prefer CSP, this is our clickjacking defense.
///  * **Referrer-Policy: strict-origin-when-cross-origin** — when
///    the viewer makes outbound calls (none today, but future neko
///    plugins might), don't leak the URL path which could include
///    profile-name hints.
///  * **Permissions-Policy** — disable APIs the viewer never uses:
///    geolocation, microphone except where neko explicitly grants
///    it, payment, USB. Keeps a XSS payload from siphoning device
///    sensors.
///  * **Strict-Transport-Security** — Caddy emits this on the
///    automatic-HTTPS layer when wildcard cert is in place, but we
///    repeat it here with `max-age=31536000; includeSubDomains` so
///    a future operator can't accidentally disable HSTS at the
///    automation level without losing it on viewer subdomains too.
fn viewer_response_security_headers() -> Value {
    json!({
        "set": {
            // CSP — see the doc comment above for each directive's
            // rationale. We use one consolidated header so Caddy
            // doesn't emit two CSPs (browsers intersect them which
            // produces hard-to-reason-about effective policies).
            "Content-Security-Policy": [concat!(
                "default-src 'self'; ",
                "script-src 'self' 'unsafe-inline'; ",
                "style-src 'self' 'unsafe-inline'; ",
                "img-src 'self' data: blob:; ",
                "media-src 'self' blob:; ",
                "connect-src 'self' wss: https:; ",
                "font-src 'self' data:; ",
                "frame-ancestors 'none'; ",
                "form-action 'self'; ",
                "base-uri 'self'; ",
                "object-src 'none'; ",
                "worker-src 'self' blob:",
            )],
            "X-Content-Type-Options": ["nosniff"],
            "X-Frame-Options": ["DENY"],
            "Referrer-Policy": ["strict-origin-when-cross-origin"],
            "Permissions-Policy": [concat!(
                "geolocation=(), ",
                "microphone=(self), ",
                "camera=(self), ",
                "payment=(), ",
                "usb=(), ",
                "magnetometer=(), ",
                "gyroscope=(), ",
                "accelerometer=()",
            )],
            "Strict-Transport-Security": ["max-age=31536000; includeSubDomains"],
            "Cross-Origin-Opener-Policy": ["same-origin"],
            "Cross-Origin-Resource-Policy": ["same-origin"],
        },
        // `deferred: true` lets the headers apply even when the
        // upstream returns its own `Content-Security-Policy` etc.
        // We'd rather override (defense-in-depth) than let a
        // misconfigured neko upstream punch a hole in our policy.
        "deferred": true,
    })
}

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
///
/// `edge_control_upstream` is the loopback `host:port` of edge-
/// control itself (typically `127.0.0.1:9443`). When `Some`, the
/// `/v1/sfu/*` paths split-route to edge-control (still gated by
/// `auth_gate_upstream` when set) so the viewer mediasoup-client can
/// open `/v1/sfu/viewer/ws` against the same origin as the share
/// page. When `None`, those paths fall through to the tunnel — which
/// will 404 because the container has no SFU handler. Provided as
/// `None` only by tests that exercise the legacy behaviour.
pub fn build_route(
    route: &CaddyRoute,
    dun_api_upstream: Option<&str>,
    auth_gate_upstream: Option<&str>,
    edge_control_upstream: Option<&str>,
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
                    // Pin Host explicitly to the share subdomain. Caddy's
                    // reverse_proxy default preserves the original client
                    // Host, which is fine in the normal case (Host already
                    // matches `route.host`). The explicit set defends
                    // against an attacker who hand-crafts an HTTP request
                    // to `https://<sub>:8443` with `Host: localhost` —
                    // even if Caddy's host matcher accepted such a
                    // request (it shouldn't; matchers also key on Host),
                    // the upstream container would receive
                    // `Host: <sub>.<region>.<domain>` and the
                    // loopback-only middleware would treat it as
                    // remote. Belt-and-suspenders for the view-only
                    // boundary on the container side.
                    "Host": [route.host.clone()],
                    "X-Forwarded-Host": ["{http.request.host}"],
                    "X-Forwarded-Proto": ["{http.request.scheme}"],
                    "X-Forwarded-For": ["{http.request.remote.host}"],
                }
            },
            "response": viewer_response_security_headers(),
        },
    });

    // Inner subroute: split traffic between dun-api (cookie-bearing
    // viewer endpoints), public asset bypass (viewer HTML + JS), an
    // auth-gated path (forward_auth + tunnel), and a final default
    // tunnel route (for tests when no auth_gate is set).
    //
    // Caddy evaluates entries top-to-bottom and stops at the first
    // match. Order matters:
    //   1. Hard-block paths (host-mode SPA, etc.) — 403 with body
    //      explaining why, BEFORE any other handler runs.
    //   2. dun-api endpoints (most specific path matchers)
    //   3. public assets (viewer-ui-react bundle)
    //   4. forward_auth → tunnel (when auth_gate set)
    //   5. tunnel default (test / dev fallback)
    let mut inner_routes: Vec<Value> = Vec::new();

    // Hard-block /host* on every public viewer subdomain. Defense-
    // in-depth: even if Caddy auth gate or a future config drift
    // would let the request through, this static_response 403
    // prevents the host SPA shell from ever loading on a public
    // subdomain. The static body explains what happened so a
    // confused user can act on it (request the owner to share an
    // updated link or reload from a fresh fragment).
    let hard_block = json!({
        "match": [{
            "path": VIEWER_EDGE_BLOCK_PATHS.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
        }],
        "handle": [{
            "handler": "static_response",
            "status_code": 403,
            "headers": {
                "Content-Type": ["text/plain; charset=utf-8"],
                "Cache-Control": ["no-store"],
                "X-Content-Type-Options": ["nosniff"],
                "X-Frame-Options": ["DENY"],
            },
            "body": EDGE_BLOCK_HOST_BODY,
        }],
    });
    inner_routes.push(hard_block);

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
                        },
                        // Apply the same security-header set as the
                        // tunnel handle. dun-api responds with JSON
                        // for `/viewer/exchange` and friends; if a
                        // future route ever returns HTML we still
                        // want CSP / nosniff in front of it.
                        "response": viewer_response_security_headers(),
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
        // 2xx the `handle_response` continues with the upstream
        // (edge-control for `/v1/sfu/*`, tunnel for everything
        // else); on 4xx/5xx Caddy returns the gate's response
        // directly so the viewer-ui receives a clean 401 (its
        // `sessionEndedGuard` then reloads to trigger the exchange
        // flow).
        //
        // We strip `Content-Length` / body from the sub-request: the
        // gate only inspects headers (Cookie + X-Forwarded-Host) and
        // forwarding the body would prematurely consume the original
        // request. Caddy's reverse_proxy handles this when paired
        // with `rewrite.method=GET`.
        //
        // Security headers piggy-back on the gate handler so 401 /
        // 503 responses also carry CSP / nosniff / frame-ancestors.
        // Without this a gate-generated error page would still be
        // sniffable / iframable, defeating the whole hardening pass.
        //
        // The `copy_headers` shim (a `headers` handler prepended
        // inside the 2xx routes list) propagates the gate's
        // `X-Forwarded-Sub` response header onto the upstream
        // request. The SFU WS handler reads this to authorize the
        // `?session=<id>` query parameter against the verified
        // share-session id from the cookie. Without this shim,
        // every WS upgrade would 401.

        // SFU-only forward_auth → edge-control. Emitted before the
        // generic tunnel block so the more specific path matcher
        // wins. Same gate sub-request as the tunnel block — Caddy
        // dedup is at the cache layer, not per-route, so the call
        // hits the gate twice for any viewer that opens both the
        // page and the SFU WS. The gate is in-process EdDSA verify
        // (~200µs) so the duplicate is acceptable.
        if let Some(edge_upstream) = edge_control_upstream {
            let edge_handle = json!({
                "handler": "reverse_proxy",
                "transport": {
                    "protocol": "http",
                    "versions": ["1.1", "2"],
                },
                "upstreams": [{"dial": edge_upstream.to_string()}],
                // X-Forwarded-Host is set by the auth-gate sub-
                // request, but the upstream request needs it set
                // explicitly here so the SFU handler can validate
                // the host claim against the request host. Same
                // shape as the tunnel handle.
                "headers": {
                    "request": {
                        "set": {
                            "Host": [route.host.clone()],
                            "X-Forwarded-Host": ["{http.request.host}"],
                            "X-Forwarded-Proto": ["{http.request.scheme}"],
                            "X-Forwarded-For": ["{http.request.remote.host}"],
                        }
                    },
                    "response": viewer_response_security_headers(),
                },
            });
            let sfu_auth_handle = json!({
                "match": [{
                    "path": EDGE_CONTROL_PATH_PREFIXES.iter()
                        .flat_map(|p| [p.to_string(), format!("{p}/*")])
                        .collect::<Vec<_>>(),
                }],
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
                        },
                        "response": viewer_response_security_headers(),
                    },
                    "handle_response": [
                        {
                            "match": {"status_code": [2]},
                            "routes": [
                                // copy_headers shim — propagate the
                                // verified `sub` claim onto the
                                // upstream request before forwarding.
                                {
                                    "handle": [{
                                        "handler": "headers",
                                        "request": {
                                            "set": {
                                                "X-Forwarded-Sub": [
                                                    "{http.reverse_proxy.header.X-Forwarded-Sub}"
                                                ]
                                            }
                                        }
                                    }]
                                },
                                {
                                    "handle": [edge_handle]
                                }
                            ]
                        }
                    ]
                }],
            });
            inner_routes.push(sfu_auth_handle);
        }

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
                    },
                    "response": viewer_response_security_headers(),
                },
                "handle_response": [
                    {
                        "match": {"status_code": [2]},
                        "routes": [
                            {
                                "handle": [{
                                    "handler": "headers",
                                    "request": {
                                        "set": {
                                            "X-Forwarded-Sub": [
                                                "{http.reverse_proxy.header.X-Forwarded-Sub}"
                                            ]
                                        }
                                    }
                                }]
                            },
                            {
                                "handle": [tunnel_handle.clone()]
                            }
                        ]
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
                            // Apply the same hardening to the 410
                            // page itself — without it the static
                            // body could be iframed by a malicious
                            // site to fingerprint share-tunnel
                            // domains, or sniffed if a CDN
                            // mid-stream rewrites the Content-Type.
                            "Content-Security-Policy": [
                                "default-src 'self'; script-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
                            ],
                            "X-Content-Type-Options": ["nosniff"],
                            "X-Frame-Options": ["DENY"],
                            "Referrer-Policy": ["no-referrer"],
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

/// Plain-text body for `/host*` 403 on public viewer subdomains.
/// Kept short and explanation-focused — anyone landing here either
/// (a) clicked a stale link expecting host control or (b) tried to
/// probe whether the public surface exposes the host SPA. Either
/// way, telling them clearly what happened is more useful than a
/// generic 403 page.
const EDGE_BLOCK_HOST_BODY: &str =
    "403 Forbidden\n\nThe `/host` route is local-only and cannot be \
accessed via a public share link. Only the device that owns this \
profile can open the host UI. If you received a share link, please \
use the `/viewer/` URL the owner sent you.\n";

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
        let v = build_route(&r, None, None, None);
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
        let v = build_route(&r, Some("127.0.0.1:3010"), None, None);
        let inner = &v["handle"][0]["routes"];
        // Index 0 is now the hard-block /host entry; api split is at
        // index 1.
        let api_paths = inner[1]["match"][0]["path"].as_array().unwrap();
        let paths: Vec<String> = api_paths
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect();
        assert!(paths.iter().any(|p| p == "/viewer/exchange"));
        assert!(paths.iter().any(|p| p == "/viewer/exchange/*"));
        assert!(paths.iter().any(|p| p == "/viewer/refresh-cookie"));
        let api_handles = inner[1]["handle"].as_array().unwrap();
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
        // No auth gate → tail route is the tunnel upstream (idx 2).
        assert_eq!(
            inner[2]["handle"][0]["upstreams"][0]["dial"],
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
        let v = build_route(&r, None, None, None);
        let inner = v["handle"][0]["routes"].as_array().unwrap();
        // Hard-block /host (idx 0) + tunnel default (idx 1) = 2 entries.
        assert_eq!(inner.len(), 2);
        assert_eq!(
            inner[1]["handle"][0]["upstreams"][0]["dial"],
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
        let v = build_route(&r, Some("127.0.0.1:3010"), Some("127.0.0.1:9444"), None);
        let inner = v["handle"][0]["routes"].as_array().unwrap();
        // 0: hard-block /host, 1: dun-api split, 2: public assets,
        // 3: auth-gated tunnel. (No SFU block when edge_control is None.)
        assert_eq!(inner.len(), 4);

        // public asset bypass
        let public_paths = inner[2]["match"][0]["path"].as_array().unwrap();
        let paths: Vec<String> = public_paths
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect();
        assert!(paths.iter().any(|p| p == "/viewer/"));
        assert!(paths.iter().any(|p| p == "/static/*"));
        assert!(paths.iter().any(|p| p == "/assets/*"));

        // forward_auth → tunnel (idx 3)
        let auth_handle = &inner[3]["handle"][0];
        assert_eq!(auth_handle["handler"], "reverse_proxy");
        assert_eq!(auth_handle["upstreams"][0]["dial"], "127.0.0.1:9444");
        assert_eq!(auth_handle["rewrite"]["uri"], "/check");
        // 2xx response → copy_headers shim then forward to tunnel
        let handle_response = auth_handle["handle_response"].as_array().unwrap();
        assert_eq!(handle_response[0]["match"]["status_code"][0], 2);
        let routes = handle_response[0]["routes"].as_array().unwrap();
        // [0] copy_headers shim, [1] tunnel reverse_proxy.
        assert_eq!(routes[0]["handle"][0]["handler"], "headers");
        assert!(
            routes[0]["handle"][0]["request"]["set"]["X-Forwarded-Sub"]
                .is_array(),
            "copy_headers shim must propagate X-Forwarded-Sub"
        );
        assert_eq!(
            routes[1]["handle"][0]["upstreams"][0]["dial"],
            "127.0.0.1:11042"
        );
    }

    #[test]
    fn build_route_with_edge_control_inserts_sfu_split() {
        let r = CaddyRoute {
            host: "abc.sin.dun-studio.xyz".into(),
            upstream: "127.0.0.1:11042".into(),
            ws_paths: vec![],
        };
        let v = build_route(
            &r,
            Some("127.0.0.1:3010"),
            Some("127.0.0.1:9444"),
            Some("127.0.0.1:9443"),
        );
        let inner = v["handle"][0]["routes"].as_array().unwrap();
        // 0: hard-block, 1: dun-api split, 2: public assets,
        // 3: SFU forward_auth → edge-control, 4: generic auth tunnel.
        assert_eq!(inner.len(), 5);

        let sfu_handle = &inner[3];
        let sfu_paths = sfu_handle["match"][0]["path"].as_array().unwrap();
        let sfu_paths_str: Vec<String> = sfu_paths
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect();
        assert!(sfu_paths_str.iter().any(|p| p == "/v1/sfu"));
        assert!(sfu_paths_str.iter().any(|p| p == "/v1/sfu/*"));

        // Forward_auth → gate, 2xx → edge-control upstream (after
        // the X-Forwarded-Sub copy_headers shim).
        let sfu_proxy = &sfu_handle["handle"][0];
        assert_eq!(sfu_proxy["handler"], "reverse_proxy");
        assert_eq!(sfu_proxy["upstreams"][0]["dial"], "127.0.0.1:9444");
        let routes_2xx = sfu_proxy["handle_response"][0]["routes"]
            .as_array()
            .unwrap();
        assert_eq!(routes_2xx[0]["handle"][0]["handler"], "headers");
        assert_eq!(
            routes_2xx[1]["handle"][0]["upstreams"][0]["dial"],
            "127.0.0.1:9443"
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
        // Security headers attached to the 410 page.
        assert_eq!(inner[0]["response"]["set"]["X-Frame-Options"][0], "DENY");
        assert_eq!(
            inner[0]["response"]["set"]["X-Content-Type-Options"][0],
            "nosniff"
        );
        assert!(
            inner[0]["response"]["set"]["Content-Security-Policy"][0]
                .as_str()
                .unwrap()
                .contains("frame-ancestors 'none'"),
        );
        assert_eq!(inner[1]["handler"], "static_response");
        assert_eq!(inner[1]["status_code"], 410);
        let body = inner[1]["body"].as_str().unwrap();
        assert!(body.contains("Session ended"));
    }

    #[test]
    fn build_route_attaches_security_headers_to_tunnel_response() {
        let r = CaddyRoute {
            host: "abc.sin.dun-studio.xyz".into(),
            upstream: "127.0.0.1:11042".into(),
            ws_paths: vec![],
        };
        let v = build_route(&r, None, None, None);
        let inner = &v["handle"][0]["routes"];
        // [0] hard-block /host, [1] tunnel default. Security headers
        // are on the tunnel handler.
        let response_headers = &inner[1]["handle"][0]["headers"]["response"]["set"];

        // CSP must include `frame-ancestors 'none'` (clickjacking
        // defense — equivalent to X-Frame-Options: DENY for modern
        // browsers).
        let csp = response_headers["Content-Security-Policy"][0]
            .as_str()
            .unwrap();
        assert!(csp.contains("frame-ancestors 'none'"), "csp = {csp}");
        assert!(csp.contains("default-src 'self'"), "csp = {csp}");
        assert!(csp.contains("object-src 'none'"), "csp = {csp}");

        assert_eq!(response_headers["X-Content-Type-Options"][0], "nosniff");
        assert_eq!(response_headers["X-Frame-Options"][0], "DENY");
        assert_eq!(
            response_headers["Referrer-Policy"][0],
            "strict-origin-when-cross-origin"
        );
        assert!(response_headers["Strict-Transport-Security"][0]
            .as_str()
            .unwrap()
            .contains("max-age=31536000"));
    }

    #[test]
    fn build_route_with_auth_gate_applies_security_headers_on_gate_response_too() {
        let r = CaddyRoute {
            host: "abc.sin.dun-studio.xyz".into(),
            upstream: "127.0.0.1:11042".into(),
            ws_paths: vec![],
        };
        let v = build_route(&r, Some("127.0.0.1:3010"), Some("127.0.0.1:9444"), None);
        let inner = v["handle"][0]["routes"].as_array().unwrap();
        // Inner route order with full wiring (api + assets + auth):
        //   [0] hard-block /host (defense in depth)
        //   [1] dun-api split (api routes)
        //   [2] public assets bypass
        //   [3] auth-gated tunnel
        let auth_handle = &inner[3]["handle"][0];
        let response_headers = &auth_handle["headers"]["response"]["set"];
        assert!(
            response_headers["Content-Security-Policy"][0]
                .as_str()
                .unwrap()
                .contains("frame-ancestors 'none'"),
            "auth gate response missing frame-ancestors directive"
        );
        assert_eq!(response_headers["X-Frame-Options"][0], "DENY");
    }

    #[test]
    fn build_route_hard_blocks_host_path_first() {
        let r = CaddyRoute {
            host: "abc.sin.dun-studio.xyz".into(),
            upstream: "127.0.0.1:11042".into(),
            ws_paths: vec![],
        };
        let v = build_route(&r, Some("127.0.0.1:3010"), Some("127.0.0.1:9444"), None);
        let inner = v["handle"][0]["routes"].as_array().unwrap();
        // Hard-block must be first so Caddy returns 403 before any
        // auth gate / dun-api / tunnel handler runs.
        let block = &inner[0];
        let block_paths = block["match"][0]["path"].as_array().unwrap();
        let paths: Vec<String> = block_paths
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect();
        assert!(paths.iter().any(|p| p == "/host"));
        assert!(paths.iter().any(|p| p == "/host/*"));

        let block_handle = &block["handle"][0];
        assert_eq!(block_handle["handler"], "static_response");
        assert_eq!(block_handle["status_code"], 403);
        let body = block_handle["body"].as_str().unwrap();
        assert!(body.contains("local-only"));
        assert_eq!(
            block_handle["headers"]["X-Content-Type-Options"][0],
            "nosniff"
        );
        assert_eq!(block_handle["headers"]["X-Frame-Options"][0], "DENY");
    }
}
