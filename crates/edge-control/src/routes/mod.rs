//! Route definitions for edge-control HTTP server.

use crate::state::AppState;
use axum::{
    routing::{delete, get, post},
    Router,
};
use std::sync::Arc;

pub mod healthz;
pub mod sfu;
pub mod snapshot;
pub mod tunnels;
pub mod verify;

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/tunnels", post(tunnels::create))
        .route("/v1/tunnels/:id", delete(tunnels::deprovision))
        .route("/v1/tunnels/:id/sfu/router", post(sfu::create_router))
        .route("/v1/tunnels/:id/sfu/viewer/:viewer_id", delete(sfu::remove_viewer))
        // Used by rathole-bridge sidecar (or co-located guard) to verify a
        // tunnel JWT presented during the rathole handshake.
        .route("/v1/tunnel/verify", post(verify::verify_tunnel_handler))
        .route("/v1/state/snapshot", get(snapshot::get))
        .route("/healthz", get(healthz::check))
        .with_state(state)
}
