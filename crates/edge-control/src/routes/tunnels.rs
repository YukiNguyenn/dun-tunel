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

    // 2. Register rathole service.
    //
    // Rathole shared secret is the RAW jwt bytes — not the sha256
    // hash. dun-app's rathole client writes the same raw JWT to its
    // own [client.services.<id>] block, and rathole HMAC-compares
    // wire bytes during handshake. Storing the hash here would
    // produce a silent mismatch (handshake fails, no client can
    // bind, Caddy 502s when proxying).
    //
    // The hash field is still recorded by dun-api for audit /
    // revocation lookups; the guard sidecar (verify_tunnel_handler)
    // re-validates the JWT signature + jti so authority is not
    // delegated to rathole's byte-equality check alone.
    state
        .rathole
        .register(RatholeService {
            name: req.session_id.clone(),
            token_hash: req.tunnel_token.clone(),
            bind_addr: format!("0.0.0.0:{local_port}"),
            transport: None, // default TCP — HTTP/WS upstream from rathole client
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

    // 4b. RTP transport — DIRECT UDP, not via rathole.
    //
    // The previous design (Phase 2 task 10.B.1 draft) tried to tunnel
    // VP8 RTP through a rathole UDP service block bound to the same
    // port mediasoup's PlainTransport had just opened. That double
    // bind is fundamentally impossible: two UDP sockets cannot share
    // a port, so rathole-server fell into an infinite
    // `Address already in use (os error 98). Retry...` loop while
    // mediasoup held the port.
    //
    // The right architecture is direct UDP from owner → edge public
    // IP on the `[plain_rtp_min, plain_rtp_max]` range (already open
    // in the firewall). mediasoup comedia mode auto-detects the
    // remote source from the first packet, so no out-of-band peer
    // exchange is needed. Implementation lives in dun-share-tunnel
    // when the GStreamer pipeline lands; until then this block is
    // intentionally empty — the TCP rathole service from step 2
    // covers control plane (HTTP/WS) which is the only thing
    // exercised in Phase 1 + Phase 2 alpha.
    //
    // Keep `rtp_service_name` definition for symmetry with
    // `deprovision`'s deregister call (idempotent — no-op when the
    // service was never registered).

    // 5. Persist session_id → subdomain mapping so the deprovision
    //    handler can recover the Caddy route host. Without this the
    //    DELETE handler would only know `session_id` but Caddy's @id
    //    is keyed by the host (subdomain), causing route leaks.
    //    Mirror the entry to disk for restart safety — best-effort:
    //    if disk write fails we still keep the in-memory entry and
    //    log so an operator can investigate.
    state
        .session_subdomains
        .insert(req.session_id.clone(), req.subdomain.clone());
    if let Err(e) = state
        .subdomain_store
        .save(&req.session_id, &req.subdomain)
        .await
    {
        tracing::warn!(
            error = ?e,
            %req.session_id,
            "subdomain_store save failed; mapping in-memory only"
        );
    }

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

    // Lookup + drop subdomain mapping registered at create time.
    // Idempotent: a second DELETE for the same session yields None and
    // we just skip the Caddy call (route is already gone).
    let subdomain = state
        .session_subdomains
        .remove(&session_id)
        .map(|(_, host)| host);
    // Drop the persistent record too so we don't replay a phantom
    // mapping on the next restart. Best-effort: a failed remove only
    // means the file lingers — the next deprovision (or boot rehydrate
    // followed by stale check) can still clean it up.
    if let Err(e) = state.subdomain_store.remove(&session_id).await {
        tracing::warn!(error = ?e, %session_id, "subdomain_store remove failed");
    }

    let caddy_fut = async {
        match subdomain.as_deref() {
            Some(host) => state.caddy.remove_route(host).await,
            None => {
                tracing::debug!(
                    %session_id,
                    "no subdomain mapping (already deprovisioned or never provisioned)"
                );
                Ok(())
            }
        }
    };

    // Best-effort cleanup all components in parallel
    let rtp_service_name = format!("{}-rtp", session_id);
    let (caddy_res, rathole_res, rathole_rtp_res, _) = tokio::join!(
        caddy_fut,
        state.rathole.deregister(&session_id),
        state.rathole.deregister(&rtp_service_name),
        state.sfu.close_session(&session_id),
    );
    if let Err(e) = caddy_res {
        tracing::warn!(error = ?e, %session_id, "caddy remove failed");
    }
    if let Err(e) = rathole_res {
        tracing::warn!(error = ?e, %session_id, "rathole deregister failed");
    }
    if let Err(e) = rathole_rtp_res {
        tracing::warn!(error = ?e, %session_id, "rathole RTP deregister failed");
    }
    StatusCode::NO_CONTENT
}
