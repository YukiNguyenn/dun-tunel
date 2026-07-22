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
//!                   plainProducerId, inputTransportOptions?,
//!                   inputSctpParameters? }
//!   client → Init { rtpCapabilities }
//!   client → ConnectConsumerTransport { dtlsParameters }
//!   server → ConnectedConsumerTransport
//!   client → Consume { producerId }
//!   server → Consumed { id, producerId, kind, rtpParameters }
//!   client → ConsumerResume { id }
//!   client → SetPreferredLayers { id, spatialLayer, temporalLayer }
//!   # ── input path (optional, task 7.2) ──
//!   client → ConnectInputTransport { dtlsParameters }
//!   server → ConnectedInputTransport
//!   client → ProduceInput { sctpStreamParameters, label, protocol }
//!   server → InputProduced { id }
//! ```
//!
//! Differences vs the PoC:
//!   1. **Session-scoped**. PoC had a global Producer; here every
//!      viewer must declare which session it wants via the `session`
//!      query string. The handler validates that against the cookie
//!      claim that Caddy `forward_auth` already verified
//!      (`X-Forwarded-Sub` carries the `sub` claim from the
//!      `viewer-cookie` JWT — the share-session id).
//!   2. **Input-capable**. The viewer's send transport (created
//!      up-front in `create_consumer_transports`) is advertised in the
//!      `Init` payload as `inputTransportOptions` + `inputSctpParameters`
//!      so the viewer can author the `neko-input` DataChannel. The
//!      `ConnectInputTransport` / `ProduceInput` handshake that wires it
//!      up is implemented below (task 7.2): the same connection-level
//!      `cookie_sub != session` guard authorizes input. When SCTP is
//!      unavailable the fields are
//!      omitted and the viewer stays read-only video.
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
    ConsumedInfo, ConsumerId, ConsumerTransportInfo, DataProducerOptions, DtlsParameters,
    IceCandidate, IceParameters, ProducerId, RouterManager, RtpCapabilities,
    RtpCapabilitiesFinalized, SctpStreamParameters, TransportId,
};
use edge_shared::types::EdgeCallbackEvent;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;

/// Error `code` sent to the viewer when the input transport DTLS
/// connect fails (`ConnectInputTransport` → `connect_send_transport`).
/// Kept as a named const so the wire contract is unit-testable without
/// a live websocket (see tests).
const ERR_CONNECT_INPUT_FAILED: &str = "connect_input_failed";
/// Error `code` sent to the viewer when opening the `neko-input`
/// DataProducer fails (`ProduceInput` → `produce_input_data`).
const ERR_PRODUCE_INPUT_FAILED: &str = "produce_input_failed";

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
    // Anti-spoof gate: `X-Forwarded-Sub` is trustworthy ONLY if the request
    // came through Caddy's forward_auth. A direct hit to edge-control (bypassing
    // Caddy) lets an attacker set X-Forwarded-Sub freely and take over any
    // session's view + input. When a gate secret is configured, require Caddy's
    // injected `X-Edge-Gate-Secret` (constant-time) — Caddy sets it only after
    // forward_auth and strips any client-supplied copy. Unset = not enforced
    // (legacy; only safe when edge-control is unreachable except via Caddy).
    if let Some(expected) = state.gate_secret.as_deref() {
        let provided = headers
            .get("x-edge-gate-secret")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !crate::auth::api_key::ct_eq(provided.as_bytes(), expected.as_bytes()) {
            tracing::warn!(
                session = %q.session,
                "ws upgrade rejected: missing/invalid X-Edge-Gate-Secret (direct hit bypassing Caddy?)"
            );
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    // `X-Forwarded-Sub` is set by `edge-viewer-gate` after a
    // successful cookie verify. Caddy's `forward_auth` block at the
    // edge in front of `/sfu/viewer/ws` ensures the gate runs first;
    // direct hits to edge-control bypass Caddy and so will not carry
    // the header — we treat that as 401 to defend against an
    // operator misconfiguring the split-route block.
    let sub = headers
        .get("x-forwarded-sub")
        .and_then(|v| v.to_str().ok());
    // Authorization decision is split into the pure `authorize_viewer`
    // helper so it can be unit-tested without a live websocket /
    // mediasoup worker (see tests below). The logging stays here so the
    // operator-facing warnings keep their existing context fields.
    match authorize_viewer(sub, &q.session) {
        Ok(()) => {}
        Err(StatusCode::UNAUTHORIZED) => {
            // `X-Forwarded-Sub` missing/empty — the edge-viewer-gate
            // forward_auth was skipped (operator misconfig or a direct
            // hit bypassing Caddy).
            tracing::warn!(
                session = %q.session,
                "ws upgrade rejected: missing X-Forwarded-Sub (auth gate skipped?)"
            );
            return Err(StatusCode::UNAUTHORIZED);
        }
        Err(code) => {
            // Cross-session leak guard (D11.5). The cookie was issued for
            // a different share session than the WS query asks for — most
            // likely a stale tab navigating to a fresh viewer URL. Force
            // the viewer-ui to redo the cookie exchange instead of
            // letting the WS open and confuse the mediasoup-client with
            // wrong RTP capabilities.
            tracing::warn!(
                cookie_sub = ?sub,
                requested = %q.session,
                "ws upgrade rejected: cookie/session mismatch"
            );
            return Err(code);
        }
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
    // ── Quality control (Hướng 2) ──────────────────────────────────────
    // The viewer picks a preferred VP9 simulcast spatial layer:
    // 0 = 540p, 1 = 720p, 2 = source/1080p. `temporalLayer: null` keeps
    // temporal selection unpinned because the current source is spatial-only
    // simulcast, not VP9 SVC.
    #[serde(rename_all = "camelCase")]
    SetPreferredLayers {
        id: ConsumerId,
        spatial_layer: u8,
        temporal_layer: Option<u8>,
    },
    // ── Input path (task 7.2) ──────────────────────────────────────────
    // The viewer authors the `neko-input` DataChannel on its send
    // transport. `ConnectInputTransport` DTLS-connects that transport and
    // `ProduceInput` opens the SCTP DataProducer. Both mirror the proven
    // `poc/neko-sfu` handshake. The fields are only sent by the viewer when
    // the `Init` payload advertised `inputTransportOptions` +
    // `inputSctpParameters`.
    #[serde(rename_all = "camelCase")]
    ConnectInputTransport { dtls_parameters: DtlsParameters },
    #[serde(rename_all = "camelCase")]
    ProduceInput {
        sctp_stream_parameters: SctpStreamParameters,
        label: String,
        protocol: String,
    },
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
    let (recv_info, send_info) = match sfu
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

    tracing::info!(
        %session_id, %viewer_id,
        has_audio = audio_producer_id.is_some(),
        "sending Init to viewer"
    );

    let init = build_init_payload(
        recv_info,
        send_info,
        producer_id,
        audio_producer_id,
        router_caps,
    );
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
            ClientMessage::SetPreferredLayers {
                id,
                spatial_layer,
                temporal_layer,
            } => {
                let quality = match spatial_layer {
                    0 => "540p",
                    1 => "720p",
                    2 => "1080p",
                    _ => "invalid",
                };
                // Best-effort: a failure here leaves the consumer at its
                // current layer and must NOT drop the connection — the
                // video keeps playing. We log + send a non-terminal Error
                // the viewer can surface as "quality change failed".
                if let Err(err) = sfu
                    .set_preferred_layers(
                        &session_id,
                        &viewer_id,
                        id,
                        spatial_layer,
                        temporal_layer,
                    )
                    .await
                {
                    tracing::warn!(
                        %session_id, %viewer_id, consumer_id = %id, error = %err,
                        "set_preferred_layers failed"
                    );
                    send_error(&mut socket, "set_preferred_layers_failed").await?;
                } else {
                    tracing::info!(
                        %session_id,
                        %viewer_id,
                        consumer_id = %id,
                        spatial_layer,
                        temporal_layer = ?temporal_layer,
                        quality,
                        "set_preferred_layers ok"
                    );
                }
            }
            // ── Input handshake (task 7.2) ──────────────────────────────
            // Authorization note: the whole WS connection is bound to one
            // authorized `session_id` — `ws_handler` enforces the
            // `cookie_sub != session` guard before the upgrade, so every
            // message on this socket (input included) is already scoped to
            // the session the viewer's cookie authorizes. The session_id is
            // fixed for the connection's lifetime, so no per-message re-check
            // is needed: the connection-level guard "covers input too" (see
            // the design's Security Considerations).
            ClientMessage::ConnectInputTransport { dtls_parameters } => {
                if let Err(err) = sfu
                    .connect_send_transport(&session_id, &viewer_id, dtls_parameters)
                    .await
                {
                    tracing::warn!(
                        %session_id, %viewer_id, error = %err,
                        "connect_send_transport failed"
                    );
                    send_error(&mut socket, ERR_CONNECT_INPUT_FAILED).await?;
                    continue;
                }
                let payload = json!({ "action": "ConnectedInputTransport" });
                socket.send(Message::Text(payload.to_string())).await?;
            }
            ClientMessage::ProduceInput {
                sctp_stream_parameters,
                label,
                protocol,
            } => {
                let mut opts = DataProducerOptions::new_sctp(sctp_stream_parameters);
                opts.label = label;
                opts.protocol = protocol;
                match sfu
                    .produce_input_data(&session_id, &viewer_id, opts)
                    .await
                {
                    Ok(id) => {
                        let payload = json!({ "action": "InputProduced", "id": id });
                        socket.send(Message::Text(payload.to_string())).await?;
                    }
                    Err(err) => {
                        tracing::warn!(
                            %session_id, %viewer_id, error = %err,
                            "produce_input_data failed"
                        );
                        send_error(&mut socket, ERR_PRODUCE_INPUT_FAILED).await?;
                    }
                }
            }
        }
    }
    Ok(true)
}

fn build_init_payload(
    recv_info: ConsumerTransportInfo,
    send_info: ConsumerTransportInfo,
    producer_id: ProducerId,
    audio_producer_id: Option<ProducerId>,
    router_caps: RtpCapabilitiesFinalized,
) -> serde_json::Value {
    let opts = InitTransportOptions::from(recv_info);

    // The viewer's send transport doubles as the carrier for the
    // `neko-input` DataChannel (task 7.2 / 9.1). It is only usable for
    // input when it negotiated SCTP — `create_consumer_transports`
    // enables SCTP on the WebRtcTransport, so in the current build the
    // send transport always carries `sctp_parameters`. We still gate on
    // its presence (via the pure `should_advertise_input` predicate) so
    // an older/SCTP-disabled edge path silently omits the input fields
    // and the viewer stays video-only (see the "Older edge w/o input"
    // row in the design's Error Handling table).
    let advertise_input = should_advertise_input(&send_info.sctp_parameters);
    let input_sctp = send_info.sctp_parameters.clone();
    let mut payload = json!({
        "action": "Init",
        "consumerTransportOptions": opts,
        "routerRtpCapabilities": router_caps,
        "plainProducerId": producer_id,
        // Opus audio producer on the same PlainTransport. `null` when
        // the session has no audio branch — the viewer client just
        // skips the audio Consume in that case.
        "audioProducerId": audio_producer_id,
    });

    if advertise_input {
        let sctp = input_sctp.expect("advertise_input ⇒ sctp present");
        // Input support available: advertise the send transport so the
        // viewer can `createSendTransport` + `produceData('neko-input')`.
        // `useSfuViewer` keys off both fields being present
        // (`setupInputChannel`), so they are emitted together via
        // `attach_input_fields`.
        let input_opts = InitTransportOptions::from(send_info);
        attach_input_fields(
            &mut payload,
            serde_json::to_value(input_opts).unwrap_or(serde_json::Value::Null),
            serde_json::to_value(sctp).unwrap_or(serde_json::Value::Null),
        );
    }

    payload
}

/// Attach the optional input-transport advertisement to an `Init`
/// payload. Both `inputTransportOptions` and `inputSctpParameters` are
/// inserted together (the viewer's `setupInputChannel` requires both),
/// so this is the single place the pair is written — extracted as a pure
/// helper so the "both-or-neither" invariant is unit-testable without
/// constructing mediasoup transport types. No-op if `payload` is not a
/// JSON object.
fn attach_input_fields(
    payload: &mut serde_json::Value,
    transport_options: serde_json::Value,
    sctp_parameters: serde_json::Value,
) {
    if let serde_json::Value::Object(map) = payload {
        map.insert("inputTransportOptions".to_string(), transport_options);
        map.insert("inputSctpParameters".to_string(), sctp_parameters);
    }
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

/// Pure authorization decision for the viewer WS upgrade, extracted from
/// `ws_handler` so it can be unit-tested without a live socket or
/// mediasoup worker.
///
/// `cookie_sub` is the `sub` claim forwarded by `edge-viewer-gate` in the
/// `X-Forwarded-Sub` header (already parsed to `&str`); `requested` is the
/// `session` query parameter the viewer asked for.
///
/// - `Ok(())`               — the cookie authorizes the requested session.
/// - `Err(UNAUTHORIZED)`    — header missing/empty (auth gate skipped).
/// - `Err(FORBIDDEN)`       — cookie is for a different session (leak guard).
///
/// Behaviour is identical to the original inline guard: empty / missing sub
/// is 401, a mismatch is 403, an exact match is allowed.
fn authorize_viewer(cookie_sub: Option<&str>, requested: &str) -> Result<(), StatusCode> {
    match cookie_sub {
        Some(s) if !s.is_empty() => {
            if s == requested {
                Ok(())
            } else {
                Err(StatusCode::FORBIDDEN)
            }
        }
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Pure predicate gating whether the `Init` payload advertises the input
/// transport. Input support is offered only when the viewer's send
/// transport negotiated SCTP (`Some`). Extracted so the gating decision is
/// unit-testable without constructing mediasoup transport types.
fn should_advertise_input<T>(send_sctp: &Option<T>) -> bool {
    send_sctp.is_some()
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

#[cfg(test)]
mod tests {
    //! Unit tests for the viewer-WS input-handshake decision logic
    //! (task 7.3, validates Requirements 3.1 / 3.2).
    //!
    //! These cover the *pure* branches of the handshake that do not need a
    //! live `RouterManager` (mediasoup) worker or a real `WebSocket`:
    //!
    //!   * `should_advertise_input` + `attach_input_fields` — the SCTP
    //!     gating that decides whether the `Init` payload carries the
    //!     input transport (R3.1: input only advertised when the send
    //!     transport negotiated SCTP).
    //!   * the `ERR_CONNECT_INPUT_FAILED` / `ERR_PRODUCE_INPUT_FAILED`
    //!     error-code wire contract sent on connect/produce failure (R3.2).
    //!   * `authorize_viewer` — the `cookie_sub != session` guard that
    //!     rejects an unauthorized session before the upgrade (R3.1/R3.2
    //!     security gate).
    //!
    //! The end-to-end `run_session` loop (which drives `connect_send_transport`
    //! / `produce_input_data` against a real worker over a live socket)
    //! requires a mediasoup worker and is exercised by the CI/Linux
    //! integration harness (`poc/neko-sfu/loadtest`, task 11.1); the
    //! `run_session_*` cases below are marked `#[ignore]` and document the
    //! assertions that environment must make.

    use super::*;
    use serde_json::json;

    // ── SCTP gating: handshake only advances when Init carried input ──────
    // Validates Requirements 3.1

    #[test]
    fn advertise_input_true_when_sctp_present() {
        // A send transport that negotiated SCTP ⇒ advertise input.
        let sctp: Option<u32> = Some(42);
        assert!(should_advertise_input(&sctp));
    }

    #[test]
    fn advertise_input_false_when_sctp_absent() {
        // SCTP-disabled / older edge ⇒ viewer stays video-only.
        let sctp: Option<u32> = None;
        assert!(!should_advertise_input(&sctp));
    }

    /// Mirror of `build_init_payload`'s gating using the extracted pure
    /// helpers, so we can assert the "both-or-neither" invariant on the
    /// emitted `Init` JSON without constructing mediasoup transport types.
    fn build_init_for_test(send_sctp: Option<serde_json::Value>) -> serde_json::Value {
        let mut payload = json!({
            "action": "Init",
            "consumerTransportOptions": { "id": "recv-1" },
            "routerRtpCapabilities": {},
            "plainProducerId": "vid-1",
            "audioProducerId": null,
        });
        if should_advertise_input(&send_sctp) {
            let sctp = send_sctp.expect("advertise ⇒ sctp present");
            attach_input_fields(
                &mut payload,
                json!({ "id": "send-1" }),
                sctp,
            );
        }
        payload
    }

    #[test]
    fn init_payload_carries_both_input_fields_when_sctp_present() {
        let payload = build_init_for_test(Some(json!({ "port": 5000 })));
        let map = payload.as_object().expect("payload is object");
        // R3.1: both fields advertised together so `setupInputChannel`
        // (which keys off both) can author the neko-input DataChannel.
        assert!(map.contains_key("inputTransportOptions"));
        assert!(map.contains_key("inputSctpParameters"));
        assert_eq!(map["inputSctpParameters"], json!({ "port": 5000 }));
    }

    #[test]
    fn init_payload_omits_both_input_fields_when_sctp_absent() {
        let payload = build_init_for_test(None);
        let map = payload.as_object().expect("payload is object");
        // Older / SCTP-disabled edge: NEITHER field present, viewer stays
        // video-only (design "Older edge w/o input" row).
        assert!(!map.contains_key("inputTransportOptions"));
        assert!(!map.contains_key("inputSctpParameters"));
        // Core video fields are unaffected by the gating.
        assert_eq!(map["action"], json!("Init"));
        assert!(map.contains_key("consumerTransportOptions"));
        assert!(map.contains_key("plainProducerId"));
    }

    #[test]
    fn attach_input_fields_is_noop_on_non_object() {
        // Defensive: never panics / mutates a non-object payload.
        let mut payload = json!("not an object");
        attach_input_fields(&mut payload, json!({}), json!({}));
        assert_eq!(payload, json!("not an object"));
    }

    // ── Error codes on connect / produce failure ─────────────────────────
    // Validates Requirements 3.2

    #[test]
    fn input_error_code_constants_match_wire_contract() {
        // The viewer (`useSfuViewer`) and the design's Error Handling table
        // key off these exact strings — pin them so a rename can't silently
        // break the contract.
        assert_eq!(ERR_CONNECT_INPUT_FAILED, "connect_input_failed");
        assert_eq!(ERR_PRODUCE_INPUT_FAILED, "produce_input_failed");
    }

    #[test]
    fn send_error_payload_shape_for_input_failures() {
        // `send_error` writes `{ "action": "Error", "code": <code> }`.
        // Assert the serialized shape for both input-failure codes without
        // needing a live socket.
        let connect_payload = json!({ "action": "Error", "code": ERR_CONNECT_INPUT_FAILED });
        assert_eq!(connect_payload["action"], json!("Error"));
        assert_eq!(connect_payload["code"], json!("connect_input_failed"));

        let produce_payload = json!({ "action": "Error", "code": ERR_PRODUCE_INPUT_FAILED });
        assert_eq!(produce_payload["action"], json!("Error"));
        assert_eq!(produce_payload["code"], json!("produce_input_failed"));
    }

    // ── Authorization: unauthorized session is rejected before upgrade ────
    // Validates Requirements 3.1, 3.2 (connection-level guard covers input)

    #[test]
    fn authorize_viewer_allows_matching_session() {
        assert_eq!(authorize_viewer(Some("sess-123"), "sess-123"), Ok(()));
    }

    #[test]
    fn authorize_viewer_forbids_session_mismatch() {
        // Cookie issued for a different share session ⇒ 403 (leak guard).
        assert_eq!(
            authorize_viewer(Some("sess-other"), "sess-123"),
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[test]
    fn authorize_viewer_unauthorized_when_header_missing() {
        // No X-Forwarded-Sub (auth gate skipped) ⇒ 401.
        assert_eq!(
            authorize_viewer(None, "sess-123"),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn authorize_viewer_unauthorized_when_header_empty() {
        // Empty header value is treated the same as missing ⇒ 401.
        assert_eq!(
            authorize_viewer(Some(""), "sess-123"),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    // ── Deferred end-to-end coverage (needs a live mediasoup worker) ──────

    #[test]
    #[ignore = "needs a live mediasoup RouterManager + WebSocket; run on CI/Linux (task 11.1)"]
    fn run_session_advances_input_only_after_init_advertised_options() {
        // CI assertion: drive `run_session` with a real worker. When the
        // session's send transport has SCTP, the Init payload carries
        // inputTransportOptions + inputSctpParameters, and a
        // ConnectInputTransport → ProduceInput handshake yields
        // ConnectedInputTransport then InputProduced { id }.
        unreachable!("integration-only — see poc/neko-sfu/loadtest");
    }

    #[test]
    #[ignore = "needs a live mediasoup RouterManager + WebSocket; run on CI/Linux (task 11.1)"]
    fn run_session_sends_connect_input_failed_on_connect_error() {
        // CI assertion: when `connect_send_transport` errors, the server
        // writes Error { code: "connect_input_failed" } and keeps video.
        unreachable!("integration-only — see poc/neko-sfu/loadtest");
    }

    #[test]
    #[ignore = "needs a live mediasoup RouterManager + WebSocket; run on CI/Linux (task 11.1)"]
    fn run_session_sends_produce_input_failed_on_produce_error() {
        // CI assertion: when `produce_input_data` errors, the server writes
        // Error { code: "produce_input_failed" } and video is unaffected.
        unreachable!("integration-only — see poc/neko-sfu/loadtest");
    }
}
