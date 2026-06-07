//! GET `/v1/sfu/viewer/ws?session=<id>` — WebSocket signaling for
//! viewer mediasoup clients (Phase 2 task 10.B.3).
//!
//! Protocol mirrors `dun-tunel/poc/neko-sfu/src/bin/sfu_main.rs` so
//! the existing PoC viewer client (and the upcoming
//! `viewer-ui-react/useSfuViewer` hook in 10.B.4) can talk to either
//! endpoint without code changes:
//!
//! ```text
//!   server → Init { consumerTransportOptions, routerRtpCapabilities,
//!                   plainProducerId }
//!   client → Init { rtpCapabilities }
//!   client → ConnectConsumerTransport { dtlsParameters }
//!   server → ConnectedConsumerTransport
//!   client → Consume { producerId }
//!   server → Consumed { id, producerId, kind, rtpParameters }
//!   client → ConsumerResume { id }
//! ```
//!
//! Differences vs the PoC:
//!   1. **Session-scoped**. PoC had a global Producer; here every
//!      viewer must declare which session it wants via the `session`
//!      query string. The handler validates that against the cookie
//!      claim that Caddy `forward_auth` already verified
//!      (`X-Forwarded-Sub` carries the `sub` claim from the
//!      `viewer-cookie` JWT — the share-session id).
//!   2. **Viewer-only**. We do NOT create a SendTransport or
//!      DataConsumer pipeline. Phase 2.2 will add the input
//!      DataChannel (mouse/keyboard) — until then the viewer is read-
//!      only video, matching the new "viewer = mediasoup, host =
//!      neko" architectural split.
//!   3. **Cap enforcement**. Reusing
//!      `RouterManager::create_consumer_transports` means the per-
//!      session 30-viewer cap (R8.8) covers WS connections too — the
//!      handler closes the socket with a 1011 error if the cap is
//!      hit.

use crate::state::AppState;
use axum::{
    extract::{
        ws::{Message, WebSocket},
        ConnectInfo, Query, State, WebSocketUpgrade,
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use edge_callback_client::Client as CallbackClient;
use edge_sfu::{
    ConsumedInfo, ConsumerId, ConsumerTransportInfo, DtlsParameters, IceCandidate,
    IceParameters, ProducerId, RouterManager, RtpCapabilities,
    RtpCapabilitiesFinalized, TransportId,
};
use edge_shared::types::EdgeCallbackEvent;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    /// Share-session id the viewer wants to subscribe to. MUST match
    /// the `sub` claim of the cookie (forwarded as `X-Forwarded-Sub`
    /// by `edge-viewer-gate`).
    pub session: String,
}

/// Axum handler — verifies the cookie matches the requested session
/// id, mints a fresh `viewer_id`, then upgrades to WS.
pub async fn ws_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<WsQuery>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, StatusCode> {
    // `X-Forwarded-Sub` is set by `edge-viewer-gate` after a
    // successful cookie verify. Caddy's `forward_auth` block at the
    // edge in front of `/sfu/viewer/ws` ensures the gate runs first;
    // direct hits to edge-control bypass Caddy and so will not carry
    // the header — we treat that as 401 to defend against an
    // operator misconfiguring the split-route block.
    let sub = headers
        .get("x-forwarded-sub")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let cookie_sub = match sub {
        Some(s) if !s.is_empty() => s,
        _ => {
            tracing::warn!(
                session = %q.session,
                "ws upgrade rejected: missing X-Forwarded-Sub (auth gate skipped?)"
            );
            return Err(StatusCode::UNAUTHORIZED);
        }
    };

    if cookie_sub != q.session {
        // Cross-session leak guard (D11.5). The cookie was issued for
        // a different share session than the WS query asks for — most
        // likely a stale tab navigating to a fresh viewer URL. Force
        // the viewer-ui to redo the cookie exchange instead of
        // letting the WS open and confuse the mediasoup-client with
        // wrong RTP capabilities.
        tracing::warn!(
            cookie_sub = %cookie_sub,
            requested = %q.session,
            "ws upgrade rejected: cookie/session mismatch"
        );
        return Err(StatusCode::FORBIDDEN);
    }

    // Mint an opaque viewer_id for this connection. We do NOT reuse
    // the cookie `jti` because the same cookie may be used by
    // multiple tabs of the same recipient — each tab gets its own
    // ViewerSlot and its own pair of WebRTC transports. Using the
    // jti would force serialised viewers per cookie, breaking the
    // multi-tab UX. Format: random 16-byte hex (matches the
    // existing pattern in `tunnel.service.ts::randomB64Url`).
    let viewer_id = mint_viewer_id();
    let session_id = q.session.clone();

    // Capture client IP and User-Agent before the upgrade so the
    // callback to dun-api carries them. IP comes from `X-Forwarded-For`
    // when Caddy is in front (production) and falls back to the raw
    // socket peer address for direct hits (dev). User-Agent is
    // optional metadata for the audit log; missing is fine.
    let viewer_ip = client_ip(&headers, addr);
    let user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // The gate's `sub` is the share-session id, NOT a per-viewer
    // identifier — so the dun-api `viewerFingerprint` field gets
    // the WS-scoped viewer_id we just minted instead. dun-api stores
    // this only for audit / dedup; SFU local state remains the
    // source of truth for the cap (D13).
    Ok(ws.on_upgrade(move |socket| async move {
        let sfu = state.sfu.clone_handle();
        let callback = state.callback.clone();

        // Track whether we actually allocated a viewer slot so the
        // disconnect callback only fires for sessions that had a
        // matching connect event. Without this, every failed upgrade
        // (bad cookie race, viewer cap, etc.) would emit a stale
        // viewer_disconnected and pollute the audit log even though
        // dun-api's $gt:0 decrement guard prevents a count drift.
        let slot_acquired = match run_session(
            socket,
            sfu.clone(),
            callback.clone(),
            session_id.clone(),
            viewer_id.clone(),
            viewer_ip.clone(),
            user_agent.clone(),
        )
        .await
        {
            Ok(slot) => slot,
            Err(err) => {
                tracing::warn!(
                    %session_id,
                    %viewer_id,
                    error = %err,
                    "ws session ended with error"
                );
                false
            }
        };

        // Always tear down the viewer slot on disconnect — covers
        // graceful close + transport errors uniformly. RouterManager
        // dropping the WebRtcTransport closes any consumers attached.
        sfu.remove_viewer(&session_id, &viewer_id).await;

        if slot_acquired {
            let event = EdgeCallbackEvent::ViewerDisconnected {
                session_id: session_id.clone(),
                viewer_fingerprint: Some(viewer_id.clone()),
            };
            if let Err(e) = callback.send(event).await {
                tracing::warn!(
                    %session_id, %viewer_id, error = ?e,
                    "viewer_disconnected callback failed"
                );
            }
        }
    }))
}

/// Wire protocol — discriminator field `action`, JSON body camelCase.
/// Mirrors `poc/neko-sfu` so the same client code path runs against
/// either endpoint during the migration.
#[derive(Debug, Deserialize)]
#[serde(tag = "action")]
enum ClientMessage {
    #[serde(rename_all = "camelCase")]
    Init { rtp_capabilities: RtpCapabilities },
    #[serde(rename_all = "camelCase")]
    ConnectConsumerTransport { dtls_parameters: DtlsParameters },
    #[serde(rename_all = "camelCase")]
    Consume { producer_id: ProducerId },
    #[serde(rename_all = "camelCase")]
    ConsumerResume { id: ConsumerId },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InitTransportOptions {
    id: TransportId,
    dtls_parameters: DtlsParameters,
    ice_candidates: Vec<IceCandidate>,
    ice_parameters: IceParameters,
}

impl From<ConsumerTransportInfo> for InitTransportOptions {
    fn from(info: ConsumerTransportInfo) -> Self {
        Self {
            id: info.transport_id,
            dtls_parameters: info.dtls_parameters,
            ice_candidates: info.ice_candidates,
            ice_parameters: info.ice_parameters,
        }
    }
}

/// Drive the WS connection until close. Each branch maps a single
/// `ClientMessage` to one or more `ServerMessage` writes. Returns
/// `Ok(true)` when a viewer slot was successfully allocated (the
/// matching `viewer_connected` callback was fired), so the caller
/// knows to emit a paired `viewer_disconnected`. Returns
/// `Ok(false)` when the upgrade was rejected before the slot landed
/// (e.g. viewer cap reached) so no disconnect echo is needed.
async fn run_session(
    mut socket: WebSocket,
    sfu: RouterManager,
    callback: CallbackClient,
    session_id: String,
    viewer_id: String,
    viewer_ip: Option<String>,
    user_agent: Option<String>,
) -> anyhow::Result<bool> {
    // Phase 1: provision a viewer slot + send Init payload before
    // we read anything from the client. This matches the PoC where
    // the server speaks first — mediasoup-client expects the
    // `routerRtpCapabilities` to arrive before `Device.load()` runs.
    let (recv_info, _send_info) = match sfu
        .create_consumer_transports(&session_id, &viewer_id)
        .await
    {
        Ok(pair) => pair,
        Err(err) => {
            let msg = err.to_string();
            // Distinguish "cap reached" from generic errors so the
            // viewer-ui can surface a tailored message. We use WS
            // close code 1013 ("try again later") for the cap and
            // 1011 ("server error") for everything else.
            let (code, reason) = if msg.contains("viewer cap") {
                (1013u16, "viewer cap reached")
            } else if msg.contains("session not found") {
                (1008u16, "session not found")
            } else {
                tracing::error!(
                    %session_id, %viewer_id, error = %err,
                    "create_consumer_transports failed in ws upgrade"
                );
                (1011u16, "internal error")
            };
            let _ = socket
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code,
                    reason: reason.into(),
                })))
                .await;
            return Ok(false);
        }
    };

    // Slot allocated — fire viewer_connected so the host's
    // ShareSession.viewerCount in dun-api increments. We deliberately
    // do this BEFORE the Init payload so the host UI lights up
    // before the viewer's mediasoup-client even loads. The disconnect
    // counterpart fires unconditionally in the outer handler — see
    // ws_handler closure.
    let connect_event = EdgeCallbackEvent::ViewerConnected {
        session_id: session_id.clone(),
        viewer_fingerprint: Some(viewer_id.clone()),
        ip: viewer_ip.clone(),
        user_agent: user_agent.clone(),
    };
    if let Err(e) = callback.send(connect_event).await {
        // Soft failure — the WS still proceeds. Worst case the host
        // UI underreports viewer count, which surfaces as a metrics
        // anomaly the operator can debug.
        tracing::warn!(
            %session_id, %viewer_id, error = ?e,
            "viewer_connected callback failed"
        );
    }

    let (producer_id, audio_producer_id, router_caps) =
        sfu.session_producer_info(&session_id).await?;

    let init = build_init_payload(recv_info, producer_id, audio_producer_id, router_caps);
    socket.send(Message::Text(init.to_string())).await?;

    // Holder for the client's RTP capabilities (sent via the Init
    // message). mediasoup `consume()` requires the consumer's caps,
    // not the router's, because it picks a codec subset both sides
    // support — using router caps would skip the negotiation step
    // mediasoup-client expects.
    let mut client_caps: Option<RtpCapabilities> = None;

    while let Some(frame) = socket.recv().await {
        let frame = match frame {
            Ok(f) => f,
            Err(err) => {
                tracing::debug!(error = %err, "ws recv error");
                break;
            }
        };
        let text = match frame {
            Message::Text(t) => t,
            Message::Binary(_) => continue,
            Message::Ping(p) => {
                socket.send(Message::Pong(p)).await?;
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => break,
        };

        let msg: ClientMessage = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(err) => {
                // Don't crash the connection on a bad frame — the
                // PoC swallows parse errors too, and a single
                // malformed message is well within the protocol's
                // tolerance (mediasoup-client retries on its own
                // when consume() rejects).
                tracing::warn!(error = %err, payload = %text, "ws parse client msg");
                continue;
            }
        };

        match msg {
            ClientMessage::Init { rtp_capabilities } => {
                tracing::debug!(
                    %session_id, %viewer_id,
                    "ws client Init"
                );
                client_caps = Some(rtp_capabilities);
            }
            ClientMessage::ConnectConsumerTransport { dtls_parameters } => {
                if let Err(err) = sfu
                    .connect_recv_transport(&session_id, &viewer_id, dtls_parameters)
                    .await
                {
                    tracing::warn!(
                        %session_id, %viewer_id, error = %err,
                        "connect_recv_transport failed"
                    );
                    send_error(&mut socket, "connect_consumer_transport_failed").await?;
                    continue;
                }
                let payload = json!({ "action": "ConnectedConsumerTransport" });
                socket.send(Message::Text(payload.to_string())).await?;
            }
            ClientMessage::Consume { producer_id } => {
                let caps = match client_caps.clone() {
                    Some(c) => c,
                    None => {
                        tracing::warn!(
                            %session_id, %viewer_id,
                            "Consume before Init — refusing"
                        );
                        send_error(&mut socket, "consume_before_init").await?;
                        continue;
                    }
                };
                let consumed = match sfu
                    .consume(&session_id, &viewer_id, producer_id, caps)
                    .await
                {
                    Ok(c) => c,
                    Err(err) => {
                        tracing::warn!(
                            %session_id, %viewer_id, error = %err,
                            "consume failed"
                        );
                        send_error(&mut socket, "consume_failed").await?;
                        continue;
                    }
                };
                socket
                    .send(Message::Text(consumed_to_payload(consumed).to_string()))
                    .await?;
            }
            ClientMessage::ConsumerResume { id } => {
                if let Err(err) = sfu.resume_consumer(&session_id, &viewer_id, id).await {
                    tracing::warn!(
                        %session_id, %viewer_id, consumer_id = %id, error = %err,
                        "resume_consumer failed"
                    );
                    send_error(&mut socket, "consumer_resume_failed").await?;
                }
            }
        }
    }
    Ok(true)
}

fn build_init_payload(
    recv_info: ConsumerTransportInfo,
    producer_id: ProducerId,
    audio_producer_id: Option<ProducerId>,
    router_caps: RtpCapabilitiesFinalized,
) -> serde_json::Value {
    let opts = InitTransportOptions::from(recv_info);
    json!({
        "action": "Init",
        "consumerTransportOptions": opts,
        // Phase 2.2 adds inputTransportOptions when the SendTransport
        // for the neko-input DataChannel comes online. For now we
        // omit the field; mediasoup-client treats it as undefined.
        "routerRtpCapabilities": router_caps,
        "plainProducerId": producer_id,
        // Opus audio producer on the same PlainTransport. `null` when
        // the session has no audio branch — the viewer client just
        // skips the audio Consume in that case.
        "audioProducerId": audio_producer_id,
    })
}

fn consumed_to_payload(consumed: ConsumedInfo) -> serde_json::Value {
    json!({
        "action": "Consumed",
        "id": consumed.id,
        "producerId": consumed.producer_id,
        "kind": consumed.kind,
        "rtpParameters": consumed.rtp_parameters,
    })
}

async fn send_error(socket: &mut WebSocket, code: &str) -> anyhow::Result<()> {
    let payload = json!({ "action": "Error", "code": code });
    socket.send(Message::Text(payload.to_string())).await?;
    Ok(())
}

fn mint_viewer_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Cheap unique id — 8 bytes of monotonic-ish time + 8 bytes of
    // hash mixing. Avoids pulling in `uuid` for a single call site.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mix = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("v-{:016x}{:016x}", nanos, mix)
}

/// Extract the client IP best we can. Caddy sets
/// `X-Forwarded-For: <viewer-ip>, <intermediates...>` after stripping
/// any client-supplied value (its `trusted_proxies` allow-list takes
/// care of that). We pick the FIRST entry, which is the original
/// client. When the header is missing (direct hits to edge-control
/// in dev), fall back to the raw socket peer address.
///
/// We don't try to be cleverer — a per-IP allowlist or
/// canonicalisation pass would just add complexity for very little
/// gain at this layer; the IP is used for audit + abuse detection
/// only and dun-api treats it as opaque.
fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> Option<String> {
    if let Some(value) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = value.split(',').next() {
            let trimmed = first.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    Some(peer.ip().to_string())
}
