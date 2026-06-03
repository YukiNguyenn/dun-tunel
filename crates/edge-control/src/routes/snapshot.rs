//! GET /v1/state/snapshot — for State Reconciliation Job (R22).

use crate::state::AppState;
use axum::{extract::State, Json};
use chrono::Utc;
use edge_shared::types::{RouteEntry, RouterEntry, StateSnapshot, TunnelEntry};
use std::sync::Arc;

pub async fn get(State(state): State<Arc<AppState>>) -> Json<StateSnapshot> {
    let routes = state
        .caddy
        .list_routes()
        .await
        .into_iter()
        .map(|r| RouteEntry {
            session_id: r.host.clone(),
            subdomain: r.host,
            upstream_port: r
                .upstream
                .split(':')
                .next_back()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        })
        .collect::<Vec<_>>();

    let routers = state
        .sfu
        .list_active_sessions()
        .await
        .into_iter()
        .map(|session_id| RouterEntry {
            session_id: session_id.clone(),
            router_id: format!("router-{session_id}"),
            viewer_count: 0, // TODO: real count from router_manager
        })
        .collect::<Vec<_>>();

    let tunnels = state
        .rathole
        .list()
        .await
        .into_iter()
        .map(|svc| TunnelEntry {
            session_id: svc.name,
            connected: true,
            last_seen_at: Utc::now(),
        })
        .collect::<Vec<_>>();

    Json(StateSnapshot {
        region: state.region.clone(),
        captured_at: Utc::now(),
        routes,
        routers,
        tunnels,
    })
}
