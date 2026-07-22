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
    // Control-plane routes dun-api calls with `x-edge-api-key` (create/delete/
    // token-rotate/snapshot). Gated by `require_api_key`, which fails CLOSED if
    // DUN_API_KEY is unset — so this plane is never reachable unauthenticated.
    let keyed = Router::new()
        .route("/v1/tunnels", post(tunnels::create))
        .route("/v1/tunnels/:id", delete(tunnels::deprovision))
        // Rotate the rathole shared secret when dun-api re-mints the
        // tunnel JWT (resume/refresh) — see tunnels::update_token.
        .route("/v1/tunnels/:id/token", patch(tunnels::update_token))
        .route("/v1/state/snapshot", get(snapshot::get))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::api_key::require_api_key,
        ));

    // Routes NOT gated by the api key:
    // - `/v1/tunnels/:id/sfu/*` — per-tunnel SFU signaling; its caller is not
    //   the `x-edge-api-key` client, so keying it would break signaling. It
    //   still requires a valid, known tunnel id. (Follow-up: authenticate once
    //   the caller is confirmed.)
    // - `/v1/sfu/viewer/ws` — viewer WS; authenticated by Caddy's forward_auth
    //   (`X-Forwarded-Sub`) plus the optional `X-Edge-Gate-Secret`, NOT the api
    //   key (browsers cannot attach it).
    // - `/v1/tunnel/verify` — called by the co-located rathole-bridge guard over
    //   loopback during the handshake; MUST stay key-free or the tunnel breaks.
    // - `/healthz` — liveness.
    let open = Router::new()
        .route("/v1/tunnels/:id/sfu/router", post(sfu::create_router))
        .route(
            "/v1/tunnels/:id/sfu/viewer/:viewer_id",
            delete(sfu::remove_viewer),
        )
        .route("/v1/sfu/viewer/ws", get(sfu_ws::ws_handler))
        .route("/v1/tunnel/verify", post(verify::verify_tunnel_handler))
        .route("/healthz", get(healthz::check));

    keyed.merge(open).with_state(state)
}
