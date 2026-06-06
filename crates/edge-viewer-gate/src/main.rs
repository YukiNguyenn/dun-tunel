//! edge-viewer-gate — on-edge sidecar that authorizes every viewer
//! HTTP request via Caddy `forward_auth` (browser-profile-public-tunnel
//! Option E').
//!
//! Architecture:
//!   1. Caddy intercepts HTTP requests for share subdomains.
//!   2. For every non-public-asset path, Caddy issues a sub-request
//!      to `127.0.0.1:9444/check` carrying the original Cookie +
//!      X-Forwarded-Host headers.
//!   3. This binary verifies the cookie JWT (EdDSA) statelessly using
//!      the JWKS public keys fetched from dun-api at boot. 200 = pass,
//!      401 = block.
//!
//! What we do NOT do:
//!   - Connect to MongoDB. Verifier is purely cryptographic.
//!   - Round-trip to dun-api per request. Only refresh JWKS every 24h
//!     and pull the revocation jti list every few seconds.
//!
//! Failure isolation:
//!   - dun-api unreachable → JWKS uses cached keys (still valid) +
//!     revocation list goes stale-fast (we treat unknown as
//!     revoked-no after a grace period). Viewers stay live.
//!   - JWKS fetch fails on first boot → return 503 from /check so
//!     Caddy treats the whole gate as down rather than silently
//!     allowing.

use axum::{
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod config;
mod jwks;
mod revocation;
mod verify;

use config::Config;
use jwks::JwksCache;
use revocation::RevocationList;

#[derive(Clone)]
pub struct AppState {
    pub jwks: JwksCache,
    pub revocation: RevocationList,
    /// When `true`, the verifier rejects every cookie whenever the
    /// local revocation mirror is stale. Surfaces operator intent
    /// for high-trust deployments (instant revocation > availability).
    /// Forwarded from `Config::revocation_required`.
    pub revocation_required: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,edge_viewer_gate=debug")),
        )
        .with(tracing_subscriber::fmt::layer().json())
        .init();

    let cfg = Config::from_env()?;
    tracing::info!(
        ?cfg.bind_addr,
        jwks_url = %cfg.jwks_url,
        revocation_required = cfg.revocation_required,
        "edge-viewer-gate starting",
    );

    // Initial JWKS fetch — fail loud if it can't, otherwise the gate
    // would block every request silently.
    let jwks = JwksCache::fetch(&cfg.jwks_url, cfg.refresh_interval).await?;
    let revocation = RevocationList::start(
        cfg.revocation_url.clone(),
        cfg.revocation_api_key.clone(),
        cfg.revocation_poll_interval,
        cfg.revocation_max_staleness,
    );

    if cfg.revocation_required && cfg.revocation_url.is_none() {
        // Operator wanted strict mode but didn't wire a URL. The
        // freshness check would otherwise short-circuit to "fresh"
        // (no polling expected) and silently downgrade to fail-OPEN.
        // Flag this loudly at boot so misconfiguration shows up in
        // logs / health rather than only when an attacker abuses the
        // gap.
        tracing::error!(
            "REVOCATION_REQUIRED=true but REVOCATION_URL not set — strict mode is INEFFECTIVE without polling"
        );
    }

    let state = AppState {
        jwks,
        revocation,
        revocation_required: cfg.revocation_required,
    };
    let app = Router::new()
        .route("/check", get(check_handler))
        .route("/healthz", get(|| async { StatusCode::OK }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(cfg.bind_addr).await?;
    tracing::info!(addr = ?listener.local_addr()?, "edge-viewer-gate listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// `/check` — Caddy `forward_auth` endpoint.
///
/// Caddy invokes this for every viewer request that is NOT in the
/// asset bypass list. We:
///   1. Parse cookie from `Cookie` header
///   2. Verify EdDSA signature using JWKS cache
///   3. Validate aud, exp, host claims
///   4. Check revocation list (optional, in-memory mirror)
///   5. Return 200 / 401
async fn check_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Tracing-friendly summary of the incoming sub-request. Caddy
    // forwards `Cookie` + `X-Forwarded-Host` + `X-Forwarded-Method` +
    // `X-Forwarded-Uri` so we can correlate gate decisions with the
    // original request the viewer made. We log header presence (not
    // values) so cookies don't leak into the log stream.
    let xfh = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("?");
    let xfu = headers
        .get("x-forwarded-uri")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("?");
    let has_cookie = headers.get("cookie").is_some();
    tracing::info!(
        forwarded_host = %xfh,
        forwarded_uri = %xfu,
        cookie_present = has_cookie,
        "/check received"
    );
    match verify::authorize(&headers, &state.jwks, &state.revocation, state.revocation_required)
        .await
    {
        Ok(claims) => {
            tracing::info!(
                forwarded_host = %xfh,
                jti = %claims.jti,
                sub = %claims.sub,
                "/check 200"
            );
            // Echo the verified `sub` (= share-session id) back to
            // Caddy so it can `copy_headers X-Forwarded-Sub` onto the
            // upstream sub-request. The SFU WS handler in
            // `edge-control` reads this header to authorize the
            // `?session=<id>` query parameter — without echoing it
            // here, every WS upgrade would 401 even though the
            // cookie was valid.
            let mut response_headers = HeaderMap::new();
            if let Ok(value) = HeaderValue::from_str(&claims.sub) {
                response_headers.insert("x-forwarded-sub", value);
            }
            (StatusCode::OK, response_headers).into_response()
        }
        Err(reason) => {
            // Distinguish "cookie bad" (401) from "we cannot decide
            // right now because revocation feed is stale" (503).
            // 503 prompts Caddy / the viewer-ui to surface a
            // "service unavailable" UX rather than the
            // session-ended-guard reload, which would just loop
            // until the feed recovers.
            let status = match &reason {
                verify::AuthError::RevocationStale => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::UNAUTHORIZED,
            };
            // Promote rejection to `info` so default `RUST_LOG=info`
            // operators see WHY a 401 happened. The reason variants
            // are bounded (no user input echoed) so log bloat is
            // capped — at most one line per failed sub-request.
            tracing::info!(
                forwarded_host = %xfh,
                forwarded_uri = %xfu,
                reason = %reason,
                status = status.as_u16(),
                "/check rejected"
            );
            status.into_response()
        }
    }
}
