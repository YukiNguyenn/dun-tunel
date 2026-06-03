//! POST /v1/tunnel/verify — verify a Tunnel_Token presented during rathole
//! handshake. Used by the rathole-bridge guard sidecar (5b.7).
//!
//! Request:
//! ```json
//! { "token": "eyJ...", "region": "sin" }
//! ```
//!
//! Response 200 (valid + not revoked):
//! ```json
//! { "ok": true, "session_id": "...", "jti": "..." }
//! ```
//!
//! Response 401 on any verification failure (invalid sig, wrong region,
//! revoked, expired). Body always JSON `{"ok": false, "reason": "..."}`.

use crate::state::AppState;
use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct VerifyRequest {
    pub token: String,
    pub region: String,
}

#[derive(Debug, Serialize)]
pub struct VerifyResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jti: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

pub async fn verify_tunnel_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<VerifyRequest>,
) -> (StatusCode, Json<VerifyResponse>) {
    match state.jwt.verify_tunnel(&req.token, &req.region).await {
        Ok(claims) => (
            StatusCode::OK,
            Json(VerifyResponse {
                ok: true,
                session_id: Some(claims.sub),
                jti: Some(claims.jti),
                reason: None,
            }),
        ),
        Err(e) => (
            StatusCode::UNAUTHORIZED,
            Json(VerifyResponse {
                ok: false,
                session_id: None,
                jti: None,
                reason: Some(e.to_string()),
            }),
        ),
    }
}
