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
    http::{HeaderMap, StatusCode},
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
    tracing::info!(?cfg.bind_addr, jwks_url = %cfg.jwks_url, "edge-viewer-gate starting");

    // Initial JWKS fetch — fail loud if it can't, otherwise the gate
    // would block every request silently.
    let jwks = JwksCache::fetch(&cfg.jwks_url, cfg.refresh_interval).await?;
    let revocation = RevocationList::start(
        cfg.revocation_url.clone(),
        cfg.revocation_api_key.clone(),
        cfg.revocation_poll_interval,
    );

    let state = AppState { jwks, revocation };
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
    match verify::authorize(&headers, &state.jwks, &state.revocation).await {
        Ok(_claims) => StatusCode::OK,
        Err(reason) => {
            // Verbose debug log on failure helps operators distinguish
            // a misconfigured cookie domain from a real attack pattern.
            tracing::debug!(?reason, "viewer cookie rejected");
            StatusCode::UNAUTHORIZED
        }
    }
}
