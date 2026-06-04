//! Shared types between dun-api and edge-control.
//!
//! Wire format: JSON over HTTP/mTLS. Tham chiếu spec R23 + design 5 (API Contracts).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub type SessionId = String;
pub type RegionId = String;

/// Request từ dun-api → edge-control để provision 1 session mới.
/// Endpoint: `POST /v1/tunnels`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionReq {
    pub session_id: SessionId,
    pub subdomain: String, // <random16>.<region>.share.dun.app
    pub tunnel_token_hash: String, // sha256 hex
    pub viewer_token_hash: String,
    pub codecs: Vec<MediaCodec>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionResp {
    pub router_id: String,
    /// Producer transport params (encrypted AES-GCM per R16.4)
    pub producer_transport_encrypted: String,
    /// Consumer template encrypted
    pub consumer_template_encrypted: String,
    pub local_upstream_port: u16,
}

/// State snapshot endpoint cho reconciliation job (R22).
/// Endpoint: `GET /v1/state/snapshot`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateSnapshot {
    pub region: RegionId,
    pub captured_at: DateTime<Utc>,
    pub routes: Vec<RouteEntry>,
    pub routers: Vec<RouterEntry>,
    pub tunnels: Vec<TunnelEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteEntry {
    pub session_id: SessionId,
    pub subdomain: String,
    pub upstream_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouterEntry {
    pub session_id: SessionId,
    pub router_id: String,
    pub viewer_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TunnelEntry {
    pub session_id: SessionId,
    pub connected: bool,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaCodec {
    pub kind: String, // "audio" | "video"
    pub mime_type: String,
    pub clock_rate: u32,
    pub channels: Option<u32>,
}

/// Outbound callback events từ edge → dun-api.
/// Endpoint: `POST /tunnels/edge-callback` trên dun-api side.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EdgeCallbackEvent {
    TunnelConnected {
        session_id: SessionId,
        ts: DateTime<Utc>,
    },
    ViewerConnected {
        session_id: SessionId,
        ts: DateTime<Utc>,
        ip: String,
        user_agent: Option<String>,
        token_fingerprint: String,
    },
    ViewerDisconnected {
        session_id: SessionId,
        ts: DateTime<Utc>,
        token_fingerprint: String,
    },
    ViewerCapReached {
        session_id: SessionId,
        ts: DateTime<Utc>,
    },
    /// R3.5 — sequence-based idempotent delivery
    BandwidthDelta {
        session_id: SessionId,
        delta_mb: f64,
        interval_start: DateTime<Utc>,
        interval_end: DateTime<Utc>,
        sequence: u64,
    },
    /// R18.2 — region health metrics every 30s
    RegionMetrics {
        region: RegionId,
        ts: DateTime<Utc>,
        cpu_pct: f32,
        active_sessions: u32,
        bandwidth_utilization_pct: f32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeCallbackBatch {
    pub events: Vec<EdgeCallbackEvent>,
}

/// Caddy route config for `edge-caddy-bridge`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaddyRoute {
    pub host: String,
    pub upstream: String, // "127.0.0.1:11042"
    pub ws_paths: Vec<String>,
}

/// Rathole service entry for `edge-rathole-bridge`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RatholeService {
    pub name: String, // session_id
    pub token_hash: String,
    pub bind_addr: String, // "0.0.0.0:11042"
}

/// Health check response cho `GET /healthz`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    pub region: RegionId,
    pub uptime_secs: u64,
    pub active_sessions: u32,
}
