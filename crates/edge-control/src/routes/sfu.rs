//! POST /v1/tunnels/:id/sfu/router — create viewer WebRtcTransports for
//! a session (Phase 2 task 10.3).
//!
//! Called by Caddy / viewer-ui-react after the cookie exchange has
//! authenticated the viewer. Returns the recv + send transport options
//! so mediasoup-client can complete the WebRTC handshake.
//!
//! Cap enforcement (R8.7-R8.8): RouterManager rejects with
//! "viewer cap reached" once 30 viewers are present in the session.

use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use edge_shared::types::SessionId;
use edge_sfu::ConsumerTransportInfo;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct CreateRouterQuery {
    /// Opaque viewer id minted on accept (e.g. UUID stored in cookie).
    pub viewer_id: String,
}

#[derive(Debug, Serialize)]
pub struct CreateRouterResponse {
    pub recv: ConsumerTransportInfo,
    pub send: ConsumerTransportInfo,
}

pub async fn create_router(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<SessionId>,
    Query(q): Query<CreateRouterQuery>,
) -> Result<Json<CreateRouterResponse>, StatusCode> {
    match state
        .sfu
        .create_consumer_transports(&session_id, &q.viewer_id)
        .await
    {
        Ok((recv, send)) => Ok(Json(CreateRouterResponse { recv, send })),
        Err(err) => {
            // The single failure mode we treat specifically is the viewer
            // cap; everything else is a 500. The cap message is matched
            // textually because RouterManager surfaces it as an
            // `anyhow::Error`. Migrating to a typed error in Phase 3 is on
            // the backlog.
            let msg = err.to_string();
            if msg.contains("viewer cap") {
                tracing::info!(%session_id, viewer_id = %q.viewer_id, "viewer cap reached");
                return Err(StatusCode::TOO_MANY_REQUESTS);
            }
            if msg.contains("session not found") {
                return Err(StatusCode::NOT_FOUND);
            }
            tracing::error!(error = ?err, %session_id, "create_consumer_transports failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}


/// DELETE /v1/tunnels/:id/sfu/viewer/:viewer_id — drop a viewer's
/// transports. Idempotent.
pub async fn remove_viewer(
    State(state): State<Arc<AppState>>,
    Path((session_id, viewer_id)): Path<(SessionId, String)>,
) -> StatusCode {
    state.sfu.remove_viewer(&session_id, &viewer_id).await;
    StatusCode::NO_CONTENT
}
