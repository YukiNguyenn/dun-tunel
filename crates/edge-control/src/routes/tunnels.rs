//! POST /v1/tunnels, DELETE /v1/tunnels/:id

use crate::state::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use edge_shared::types::{
    CaddyRoute, CreateSessionReq, CreateSessionResp, RatholeService, SessionId,
};
use std::sync::Arc;

pub async fn create(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateSessionReq>,
) -> Result<Json<CreateSessionResp>, StatusCode> {
    tracing::info!(session_id = %req.session_id, subdomain = %req.subdomain, "create session");

    // 1. Allocate local upstream port
    let local_port = state
        .port_allocator
        .allocate()
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

    // 2. Register rathole service
    state
        .rathole
        .register(RatholeService {
            name: req.session_id.clone(),
            token_hash: req.tunnel_token_hash.clone(),
            bind_addr: format!("0.0.0.0:{local_port}"),
        })
        .await
        .map_err(|e| {
            tracing::error!(error = ?e, "rathole register failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // 3. Add Caddy route
    state
        .caddy
        .add_route(CaddyRoute {
            host: req.subdomain.clone(),
            upstream: format!("127.0.0.1:{local_port}"),
            ws_paths: vec!["/api/ws".into(), "/webrtc".into()],
        })
        .await
        .map_err(|e| {
            tracing::error!(error = ?e, "caddy add_route failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // 4. Create mediasoup Router + PlainTransport + Producer (R8.1, R8.2).
    //    On failure we tear down rathole + caddy (steps 2/3) so we don't
    //    leak edge resources for a session that never came online.
    let provisioned = match state.sfu.provision_session(&req.session_id).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = ?e, %req.session_id, "sfu provision_session failed");
            let _ = state.caddy.remove_route(&req.subdomain).await;
            let _ = state.rathole.deregister(&req.session_id).await;
            state.port_allocator.release(local_port).await;
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    Ok(Json(CreateSessionResp {
        router_id: provisioned.router_id.to_string(),
        // Phase 2: AES-GCM envelope (R16.4) wraps these blobs. Phase 1 →
        // Phase 2 transition leaves the raw JSON for now; viewer cookie
        // exchange already protects the wire path.
        producer_transport_encrypted: serde_json::to_string(&serde_json::json!({
            "producerId": provisioned.producer_id,
            "rtpCapabilities": provisioned.rtp_capabilities,
            "plainRtpPort": provisioned.plain_rtp_port,
            "plainRtcpPort": provisioned.plain_rtcp_port,
        }))
        .unwrap_or_default(),
        consumer_template_encrypted: String::new(),
        local_upstream_port: local_port,
    }))
}

pub async fn deprovision(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<SessionId>,
) -> StatusCode {
    tracing::info!(%session_id, "deprovision session");

    // Best-effort cleanup all components in parallel
    let (caddy_res, rathole_res, _) = tokio::join!(
        state.caddy.remove_route(&session_id),
        state.rathole.deregister(&session_id),
        state.sfu.close_session(&session_id),
    );
    if let Err(e) = caddy_res {
        tracing::warn!(error = ?e, %session_id, "caddy remove failed");
    }
    if let Err(e) = rathole_res {
        tracing::warn!(error = ?e, %session_id, "rathole deregister failed");
    }
    StatusCode::NO_CONTENT
}
