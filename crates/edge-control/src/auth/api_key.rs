//! API-key middleware for the edge-control plane.
//!
//! Guards the control routes dun-api calls (create/delete/token/snapshot) with
//! the shared `x-edge-api-key`. Fails CLOSED: if no key is configured the plane
//! rejects every request rather than running unauthenticated.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};

use crate::state::AppState;

/// Constant-time byte comparison — avoids a timing oracle on the key.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Require a valid `x-edge-api-key` header, compared in constant time to the
/// configured `DUN_API_KEY`. Returns 401 when the header is missing/wrong OR
/// when no key is configured (fail-closed — the control plane must never be
/// reachable without authentication).
pub async fn require_api_key(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let expected = match state.dun_api_key.as_deref() {
        Some(k) if !k.is_empty() => k,
        _ => return Err(StatusCode::UNAUTHORIZED),
    };
    let provided = req
        .headers()
        .get("x-edge-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !ct_eq(provided.as_bytes(), expected.as_bytes()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(req).await)
}
