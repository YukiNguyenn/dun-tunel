//! Route definitions for edge-control HTTP server.

use crate::state::AppState;
use axum::{
    routing::{delete, get, patch, post},
    Router,
};
use std::sync::Arc;

pub mod healthz;
pub mod sfu;
pub mod sfu_ws;
pub mod snapshot;
pub mod tunnels;
pub mod verify;

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/tunnels", post(tunnels::create))
        .route("/v1/tunnels/:id", delete(tunnels::deprovision))
        // Rotate the rathole shared secret when dun-api re-mints the
        // tunnel JWT (resume/refresh) — see tunnels::update_token.
        .route("/v1/tunnels/:id/token", patch(tunnels::update_token))
        .route("/v1/tunnels/:id/sfu/router", post(sfu::create_router))
        .route("/v1/tunnels/:id/sfu/viewer/:viewer_id", delete(sfu::remove_viewer))
        // WebSocket signaling for the viewer mediasoup-client. Caddy
        // exposes this at `https://<sub>:8443/sfu/viewer/ws` via the
        // split-route block in `edge-caddy-bridge::route_builder`. The
        // edge-viewer-gate forward_auth check runs first, so reaching
        // this handler implies a verified `viewer-cookie` JWT — the
        // handler still re-checks the `X-Forwarded-Sub` header
        // matches the requested session as defense in depth.
        .route("/v1/sfu/viewer/ws", get(sfu_ws::ws_handler))
        // Used by rathole-bridge sidecar (or co-located guard) to verify a
        // tunnel JWT presented during the rathole handshake.
        .route("/v1/tunnel/verify", post(verify::verify_tunnel_handler))
        .route("/v1/state/snapshot", get(snapshot::get))
        .route("/healthz", get(healthz::check))
        .with_state(state)
}
