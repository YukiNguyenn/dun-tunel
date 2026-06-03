//! GET /healthz — liveness probe.

use crate::state::AppState;
use axum::{extract::State, Json};
use edge_shared::types::HealthResponse;
use std::sync::Arc;

pub async fn check(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let active_sessions = state.sfu.list_active_sessions().await.len() as u32;
    Json(HealthResponse {
        region: state.region.clone(),
        uptime_secs: state.started_at.elapsed().as_secs(),
        active_sessions,
    })
}
