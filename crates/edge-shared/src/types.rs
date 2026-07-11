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
    pub subdomain: String, // <random16>.<region>.dun-studio.xyz
    /// SHA-256 hex of the tunnel JWT — kept for storage / audit.
    /// NOT used as the rathole shared secret because rathole compares
    /// raw bytes, not the hash; both sides MUST present the same
    /// raw value (see `tunnel_token`).
    pub tunnel_token_hash: String,
    /// Raw plaintext tunnel JWT. This is the shared secret rathole
    /// uses for the per-service handshake — both sides (edge server
    /// config + dun-app rathole client) must register the SAME raw
    /// value. The JWT itself is short-lived (≤ TTL) and the guard
    /// sidecar separately verifies its signature/jti at handshake
    /// time, so the wire-bytes match here is purely the auth proof
    /// rathole expects.
    pub tunnel_token: String,
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
    /// Media ingest endpoint for the owner's Neko GStreamer udpsink
    /// (Phase 2 SFU, direct-UDP architecture). The owner pushes VP8
    /// simulcast RTP (was VP9 — mediasoup rejects VP9 simulcast) to
    /// `<edge_public_host>:<media_rtp_port>` where mediasoup's
    /// comedia-mode PlainTransport auto-detects the remote peer. Host is
    /// resolved by dun-api from the region (edge-control sits behind NAT
    /// and only knows its private IP), so we only carry the port + RTP
    /// shape here. `None` for legacy/unprovisioned sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_rtp_port: Option<u16>,
    /// RTP payload type the SFU PlainTransport Producer expects (VP8 = 96; was VP9, same PT).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_payload_type: Option<u8>,
    /// Back-compat high-layer SSRC echoed to owners that only carry one SSRC.
    /// The SFU video Producer itself binds low/mid/high simulcast SSRCs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_ssrc: Option<u32>,
    /// Optional LAN-private host for same-network (hairpin) operation.
    /// Mirrors edge `SFU_ANNOUNCED_IP_LAN`. When set, an owner whose
    /// public IP equals the edge's public IP can target this private IP
    /// directly for the udpsink ingest leg instead of hairpinning off
    /// the public host. `None` in production (no LAN candidate).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_lan_host: Option<String>,
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
///
/// Wire format mirrors dun-api `edgeCallbackBody` typebox:
///   - Top-level discriminator field is `event` (literal: snake_case
///     name like `viewer_connected`, `bandwidth_delta`).
///   - All other fields use camelCase (per variant).
///
/// We achieve that with `tag = "event"` + `rename_all = "snake_case"`
/// at the enum level (handles the discriminator value) and a
/// per-variant `rename_all = "camelCase"` (handles the field names).
/// The callback is sent as a SINGLE flat event per HTTP request —
/// dun-api's typebox schema does not accept batched events. Bandwidth
/// delta has its own monotonic sequence so retries dedupe safely.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EdgeCallbackEvent {
    #[serde(rename_all = "camelCase")]
    TunnelConnected {
        session_id: SessionId,
        region: RegionId,
        sfu_router_id: String,
    },
    #[serde(rename_all = "camelCase")]
    TunnelDisconnected {
        session_id: SessionId,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    ViewerConnected {
        session_id: SessionId,
        #[serde(skip_serializing_if = "Option::is_none")]
        viewer_fingerprint: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ip: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        user_agent: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    ViewerDisconnected {
        session_id: SessionId,
        #[serde(skip_serializing_if = "Option::is_none")]
        viewer_fingerprint: Option<String>,
    },
    /// R3.5 — sequence-based idempotent delivery
    #[serde(rename_all = "camelCase")]
    BandwidthDelta {
        session_id: SessionId,
        delta_mb: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        interval_start: Option<DateTime<Utc>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        interval_end: Option<DateTime<Utc>>,
        sequence: u64,
    },
    /// 30s authoritative count from `SessionState.viewers.len()`.
    /// Self-healing: dun-api OVERWRITES `viewerCount` rather than
    /// $inc — any drift from dropped per-event callbacks collapses
    /// back in ≤ 30s.
    #[serde(rename_all = "camelCase")]
    SessionSnapshot {
        session_id: SessionId,
        active_connections: u32,
    },
    /// R18.2 — region health metrics every 30s
    #[serde(rename_all = "camelCase")]
    RegionMetrics {
        region: RegionId,
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

/// Rathole service transport. Defaults to TCP for backward compatibility
/// — every existing service in production runs HTTP/WebSocket and is TCP.
/// UDP is added for share-tunnel SFU integration (Phase 2 task 10.B.1):
/// each session that opts into mediasoup gets a second service block
/// `[<id>-rtp]` with `type = "udp"` so Neko's GStreamer udpsink RTP
/// stream tunnels from the owner container straight to the edge
/// mediasoup `PlainTransport`.
///
/// Wire format is the lowercase string Rathole expects in its TOML
/// (`"tcp"` / `"udp"`) — keeps the server config writer free of any
/// extra mapping layer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RatholeTransport {
    #[default]
    Tcp,
    Udp,
}

/// Rathole service entry for `edge-rathole-bridge`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RatholeService {
    pub name: String, // session_id
    pub token_hash: String,
    pub bind_addr: String, // "0.0.0.0:11042"
    /// Service transport. Defaults to TCP. UDP is used for the per-session
    /// `<id>-rtp` block tunneling Neko GStreamer RTP into mediasoup.
    /// Field is `Option<_>` so older wire payloads without this key
    /// (Phase 1 deployments) still deserialise as TCP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<RatholeTransport>,
}

/// Health check response cho `GET /healthz`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    pub region: RegionId,
    pub uptime_secs: u64,
    pub active_sessions: u32,
}

#[cfg(test)]
mod callback_event_tests {
    //! Wire-format guard for `EdgeCallbackEvent`.
    //!
    //! dun-api's `edgeCallbackBody` typebox schema accepts a flat
    //! single event with discriminator `event` and camelCase fields.
    //! These tests pin every variant's serialised shape so a
    //! refactor on either side fails loud here instead of silently
    //! breaking host-mode viewer count or bandwidth dedup.
    use super::*;
    use serde_json::json;

    #[test]
    fn viewer_connected_camel_case_and_event_tag() {
        let ev = EdgeCallbackEvent::ViewerConnected {
            session_id: "sess-1".into(),
            viewer_fingerprint: Some("v-abc".into()),
            ip: Some("198.51.100.42".into()),
            user_agent: Some("Mozilla/5.0".into()),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v,
            json!({
                "event": "viewer_connected",
                "sessionId": "sess-1",
                "viewerFingerprint": "v-abc",
                "ip": "198.51.100.42",
                "userAgent": "Mozilla/5.0",
            })
        );
    }

    #[test]
    fn viewer_disconnected_skips_none_fingerprint() {
        let ev = EdgeCallbackEvent::ViewerDisconnected {
            session_id: "sess-1".into(),
            viewer_fingerprint: None,
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v, json!({ "event": "viewer_disconnected", "sessionId": "sess-1" }));
    }

    #[test]
    fn bandwidth_delta_keeps_sequence_and_camel_case_intervals() {
        let ev = EdgeCallbackEvent::BandwidthDelta {
            session_id: "sess-1".into(),
            delta_mb: 1.25,
            interval_start: None,
            interval_end: None,
            sequence: 7,
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v,
            json!({
                "event": "bandwidth_delta",
                "sessionId": "sess-1",
                "deltaMb": 1.25,
                "sequence": 7,
            })
        );
    }

    #[test]
    fn session_snapshot_serialises_flat_event_with_camel_case_count() {
        let ev = EdgeCallbackEvent::SessionSnapshot {
            session_id: "sess-1".into(),
            active_connections: 5,
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v,
            json!({
                "event": "session_snapshot",
                "sessionId": "sess-1",
                "activeConnections": 5,
            })
        );
    }
}
