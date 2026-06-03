//! Phase 0 PoC SFU — Neko Producer (RTP via PlainTransport) + browser Consumers.
//!
//! Architecture:
//!
//!   Neko (gstreamer)  ── RTP UDP ──▶  mediasoup PlainTransport  ──▶  Router Producer
//!                                                                          │
//!                                                                          ├─▶ Consumer 1 (viewer A)
//!                                                                          ├─▶ Consumer 2 (viewer B)
//!                                                                          └─▶ Consumer N
//!
//! Endpoints:
//!   POST /v1/plain-producer  → idempotently create PlainTransport listening on
//!                              `SFU_PLAIN_RTP_PORT` UDP and a Producer feeding RTP
//!                              into the shared Router. Returns producer id + the
//!                              UDP address Neko should send RTP to.
//!   GET  /ws                 → viewer signalling (mediasoup-client). Same JSON
//!                              wire protocol as the upstream `echo.rs` example.
//!
//! Single shared Worker + Router for the whole process. Router lifetime tied to
//! AppState; Producers/Consumers held in the connection actor or AppState as
//! appropriate to keep them alive.

use std::collections::HashMap;
use std::env;
use std::net::IpAddr;
use std::num::{NonZeroU32, NonZeroU8};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use actix::prelude::*;
use actix_cors::Cors;
use actix_web::web::{Data, Payload};
use actix_web::{App, Error, HttpRequest, HttpResponse, HttpServer, web};
use actix_web_actors::ws;
use mediasoup::prelude::*;
use mediasoup::worker::{WorkerLogLevel, WorkerLogTag};
use mediasoup_types::data_structures::WebRtcMessage;
use mediasoup_types::sctp_parameters::SctpParameters;
use serde::{Deserialize, Serialize};

// ────────────────────────────────────────────────────────────────────────────
// Shared application state
// ────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct SfuListen {
    listen_ip: IpAddr,
    announced_ip: IpAddr,
    rtc_min_port: u16,
    rtc_max_port: u16,
    plain_rtp_port: u16,
    plain_rtcp_port: u16,
}

struct AppState {
    listen: SfuListen,
    worker_manager: WorkerManager,
    /// Lazily initialized on first request that needs it.
    router: tokio::sync::OnceCell<Router>,
    /// Plain producer fed by the upstream RTP source (Neko). Created once via
    /// POST /v1/plain-producer; subsequent calls are idempotent.
    plain_producer: tokio::sync::OnceCell<PlainProducerHandle>,
    /// Shared DirectTransport used to attach DataConsumers that observe
    /// viewer-side DataProducers (input channel). DirectTransport runs in
    /// process and exposes on_message callbacks; SCTP DataProducers do not.
    direct_transport: tokio::sync::OnceCell<DirectTransport>,
}

struct PlainProducerHandle {
    producer: Producer,
    /// Held to keep the transport alive — dropping it kills the producer.
    _transport: PlainTransport,
}

// ────────────────────────────────────────────────────────────────────────────
// HTTP types
// ────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlainProducerInfo {
    producer_id: ProducerId,
    rtp_listen_ip: IpAddr,
    rtp_listen_port: u16,
    rtcp_listen_port: u16,
    payload_type: u8,
    clock_rate: u32,
    encoding_name: String,
    ssrc: u32,
}

// ────────────────────────────────────────────────────────────────────────────
// WebSocket protocol — identical to upstream `echo.rs`
// ────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TransportOptions {
    id: TransportId,
    dtls_parameters: DtlsParameters,
    ice_candidates: Vec<IceCandidate>,
    ice_parameters: IceParameters,
}

#[derive(Serialize, Message)]
#[serde(tag = "action")]
#[rtype(result = "()")]
#[allow(clippy::large_enum_variant)]
enum ServerMessage {
    #[serde(rename_all = "camelCase")]
    Init {
        consumer_transport_options: TransportOptions,
        /// Separate transport options for the viewer's SendTransport (used
        /// only for the `neko-input` DataChannel). mediasoup-client requires
        /// distinct send and recv transports.
        input_transport_options: TransportOptions,
        router_rtp_capabilities: RtpCapabilitiesFinalized,
        plain_producer_id: Option<ProducerId>,
        sctp_parameters: Option<SctpParameters>,
        input_sctp_parameters: Option<SctpParameters>,
    },
    ConnectedConsumerTransport,
    ConnectedInputTransport,
    #[serde(rename_all = "camelCase")]
    Consumed {
        id: ConsumerId,
        producer_id: ProducerId,
        kind: MediaKind,
        rtp_parameters: RtpParameters,
    },
    /// Echo back to the producer so the loadtest harness can verify SFU
    /// actually saw the DataChannel message. Phase 1 will replace this with
    /// "forwarded to Neko" ack.
    #[serde(rename_all = "camelCase")]
    InputAck {
        sequence: u64,
        received_bytes: usize,
    },
}

#[derive(Deserialize, Message)]
#[serde(tag = "action")]
#[rtype(result = "()")]
enum ClientMessage {
    #[serde(rename_all = "camelCase")]
    Init { rtp_capabilities: RtpCapabilities },
    #[serde(rename_all = "camelCase")]
    ConnectConsumerTransport { dtls_parameters: DtlsParameters },
    #[serde(rename_all = "camelCase")]
    ConnectInputTransport { dtls_parameters: DtlsParameters },
    #[serde(rename_all = "camelCase")]
    Consume { producer_id: ProducerId },
    #[serde(rename_all = "camelCase")]
    ConsumerResume { id: ConsumerId },
    /// Viewer announces its DataProducer for `neko-input` channel. SFU
    /// `consumeData` and forwards each message to Neko via the configured
    /// bridge (Phase 1+ — for PoC we just log + ack).
    #[serde(rename_all = "camelCase")]
    ProduceInput {
        sctp_stream_parameters: SctpStreamParameters,
        label: String,
        protocol: String,
    },
}

#[derive(Message)]
#[rtype(result = "()")]
enum InternalMessage {
    SaveConsumer(Consumer),
    SaveInputProducer(DataProducer),
    SaveInputConsumer(DataConsumer),
    InputBroadcast { sequence: u64, bytes: usize },
    Stop,
}

// ────────────────────────────────────────────────────────────────────────────
// Bootstrap: Worker + Router
// ────────────────────────────────────────────────────────────────────────────

fn media_codecs() -> Vec<RtpCodecCapability> {
    vec![
        RtpCodecCapability::Audio {
            mime_type: MimeTypeAudio::Opus,
            preferred_payload_type: None,
            clock_rate: NonZeroU32::new(48_000).unwrap(),
            channels: NonZeroU8::new(2).unwrap(),
            parameters: RtpCodecParametersParameters::from([("useinbandfec", 1_u32.into())]),
            rtcp_feedback: vec![RtcpFeedback::TransportCc],
        },
        RtpCodecCapability::Video {
            mime_type: MimeTypeVideo::Vp8,
            preferred_payload_type: None,
            clock_rate: NonZeroU32::new(90_000).unwrap(),
            parameters: RtpCodecParametersParameters::default(),
            // NOTE PoC: KHÔNG khai báo `RtcpFeedback::Nack` ở đây vì source RTP
            // từ Neko (PlainTransport) không có retransmit cache — pipeline
            // GStreamer recreate lại mỗi lần Neko UI session reconnect khiến
            // RTP sequence numbers reset, làm browser gửi NACK request trên các
            // packet đã chìm trong rolling SRTP counter ⇒ "replay check failed
            // (index too old)" flood. Giữ NackPli (chỉ keyframe request, không
            // retransmit RTP) + CcmFir + REMB + TransportCc.
            rtcp_feedback: vec![
                RtcpFeedback::NackPli,
                RtcpFeedback::CcmFir,
                RtcpFeedback::GoogRemb,
                RtcpFeedback::TransportCc,
            ],
        },
    ]
}

async fn ensure_router(state: &AppState) -> anyhow::Result<&Router> {
    state
        .router
        .get_or_try_init(|| async {
            let mut settings = WorkerSettings::default();
            settings.log_level = WorkerLogLevel::Warn;
            settings.log_tags = vec![
                WorkerLogTag::Info,
                WorkerLogTag::Ice,
                WorkerLogTag::Dtls,
                WorkerLogTag::Rtp,
                WorkerLogTag::Srtp,
                WorkerLogTag::Rtcp,
            ];
            let worker = state
                .worker_manager
                .create_worker(settings)
                .await
                .map_err(|e| anyhow::anyhow!("create worker: {e}"))?;
            let router = worker
                .create_router(RouterOptions::new(media_codecs()))
                .await
                .map_err(|e| anyhow::anyhow!("create router: {e}"))?;
            log::info!("router ready id={}", router.id());
            anyhow::Ok(router)
        })
        .await
}

async fn ensure_direct_transport(state: &AppState) -> anyhow::Result<&DirectTransport> {
    let router = ensure_router(state).await?;
    state
        .direct_transport
        .get_or_try_init(|| async {
            let dt = router
                .create_direct_transport(DirectTransportOptions::default())
                .await
                .map_err(|e| anyhow::anyhow!("create direct transport: {e}"))?;
            log::info!("direct transport ready id={}", dt.id());
            anyhow::Ok(dt)
        })
        .await
}


// ────────────────────────────────────────────────────────────────────────────
// HTTP route: POST /v1/plain-producer
// ────────────────────────────────────────────────────────────────────────────

const PLAIN_PAYLOAD_TYPE: u8 = 96;
const PLAIN_CLOCK_RATE: u32 = 90_000;
const PLAIN_SSRC: u32 = 22_222_222;

async fn plain_producer_handler(state: Data<AppState>) -> Result<HttpResponse, Error> {
    let listen = state.listen;
    match ensure_plain_producer(&state, listen).await {
        Ok(info) => Ok(HttpResponse::Ok().json(info)),
        Err(e) => {
            log::error!("plain producer init failed: {e:?}");
            Ok(HttpResponse::InternalServerError().body(format!("{e}")))
        }
    }
}

async fn ensure_plain_producer(
    state: &AppState,
    listen: SfuListen,
) -> anyhow::Result<PlainProducerInfo> {
    let router = ensure_router(state).await?;

    let handle = state
        .plain_producer
        .get_or_try_init(|| async {
            // Comedia mode = mediasoup auto-detects the remote RTP source from
            // the first packet that arrives. Without this we'd have to call
            // `transport.connect({ ip, port })` first, which is awkward when
            // the source is GStreamer in another container with an ephemeral
            // source port. Comedia is the right choice for any
            // server-pushes-RTP scenario (Neko → SFU here).
            let mut plain_options = PlainTransportOptions::new(ListenInfo {
                protocol: Protocol::Udp,
                ip: listen.listen_ip,
                announced_address: Some(listen.announced_ip.to_string()),
                expose_internal_ip: false,
                port: Some(listen.plain_rtp_port),
                port_range: None,
                flags: None,
                send_buffer_size: None,
                recv_buffer_size: None,
            });
            plain_options.comedia = true;
            let transport = router
                .create_plain_transport(plain_options)
                .await
                .map_err(|e| anyhow::anyhow!("create plain transport: {e}"))?;

            log::info!(
                "plain transport listening rtp=:{} rtcp=:{}",
                transport.tuple().local_port(),
                transport
                    .rtcp_tuple()
                    .map(|t| t.local_port().to_string())
                    .unwrap_or_else(|| "(mux)".into()),
            );

            // RTP parameters describing what the upstream gstreamer pipeline emits.
            // Must match Neko's `rtpvp8pay pt=96` configuration — payload type 96,
            // clock rate 90000, fixed SSRC.
            let rtp_parameters = RtpParameters {
                mid: None,
                codecs: vec![RtpCodecParameters::Video {
                    mime_type: MimeTypeVideo::Vp8,
                    payload_type: PLAIN_PAYLOAD_TYPE,
                    clock_rate: NonZeroU32::new(PLAIN_CLOCK_RATE).unwrap(),
                    parameters: RtpCodecParametersParameters::default(),
                    rtcp_feedback: vec![],
                }],
                header_extensions: vec![],
                encodings: vec![RtpEncodingParameters {
                    ssrc: Some(PLAIN_SSRC),
                    ..RtpEncodingParameters::default()
                }],
                rtcp: RtcpParameters::default(),
                msid: None,
            };

            let producer = transport
                .produce(ProducerOptions::new(MediaKind::Video, rtp_parameters))
                .await
                .map_err(|e| anyhow::anyhow!("produce on plain transport: {e}"))?;

            log::info!("plain producer id={} kind=video", producer.id());

            // Subscribe to producer score updates — when score is non-zero we
            // know mediasoup is actively receiving RTP packets from Neko.
            producer
                .on_score(|scores| {
                    log::info!(
                        "plain producer score update: {}",
                        scores
                            .iter()
                            .map(|s| format!("ssrc={} score={}", s.ssrc, s.score))
                            .collect::<Vec<_>>()
                            .join(", "),
                    );
                })
                .detach();

            // Periodically poll producer stats so we can confirm RTP is
            // actually being received even when score events don't fire.
            // get_stats() returns RtpStreamRecv stats including packet/byte
            // counters which we log at INFO so it's visible in the SFU log.
            let producer_for_stats = producer.clone();
            actix::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
                loop {
                    tick.tick().await;
                    match producer_for_stats.get_stats().await {
                        Ok(stats) => {
                            if stats.is_empty() {
                                log::info!("plain producer stats: (no streams seen yet)");
                            } else {
                                for s in stats {
                                    log::info!("plain producer stats: {s:?}");
                                }
                            }
                        }
                        Err(e) => log::warn!("plain producer get_stats: {e}"),
                    }
                }
            });

            anyhow::Ok(PlainProducerHandle {
                producer,
                _transport: transport,
            })
        })
        .await?;

    Ok(PlainProducerInfo {
        producer_id: handle.producer.id(),
        rtp_listen_ip: listen.announced_ip,
        rtp_listen_port: listen.plain_rtp_port,
        rtcp_listen_port: listen.plain_rtcp_port,
        payload_type: PLAIN_PAYLOAD_TYPE,
        clock_rate: PLAIN_CLOCK_RATE,
        encoding_name: "VP8".into(),
        ssrc: PLAIN_SSRC,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// WebSocket actor — viewer-only Consumer
// ────────────────────────────────────────────────────────────────────────────

struct ViewerConnection {
    state: Arc<AppState>,
    client_caps: Option<RtpCapabilities>,
    consumer_transport: Option<WebRtcTransport>,
    /// Separate WebRtcTransport for the viewer's `neko-input` SendTransport.
    /// Required because mediasoup-client SendTransport is the only side that
    /// can call `produceData()`.
    input_transport: Option<WebRtcTransport>,
    consumers: HashMap<ConsumerId, Consumer>,
    /// DataProducer authored by the viewer for input forwarding (mouse/key).
    input_data_producer: Option<DataProducer>,
    /// DataConsumer on the shared DirectTransport that observes incoming
    /// messages from `input_data_producer`. Held to keep it alive.
    input_data_consumer: Option<DataConsumer>,
    /// Counter of `neko-input` messages observed (just for the PoC log).
    input_messages_seen: u64,
    input_bytes_seen: u64,
}

impl ViewerConnection {
    fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            client_caps: None,
            consumer_transport: None,
            input_transport: None,
            consumers: HashMap::new(),
            input_data_producer: None,
            input_data_consumer: None,
            input_messages_seen: 0,
            input_bytes_seen: 0,
        }
    }
}

impl Actor for ViewerConnection {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        log::info!("ws viewer opened");
        let address = ctx.address();
        let state = self.state.clone();
        actix::spawn(async move {
            let router = match ensure_router(&state).await {
                Ok(r) => r.clone(),
                Err(e) => {
                    log::error!("router init: {e:?}");
                    address.do_send(InternalMessage::Stop);
                    return;
                }
            };

            let listen = state.listen;
            let make_transport_opts = || -> WebRtcTransportOptions {
                let mut opts = WebRtcTransportOptions::new(
                    WebRtcTransportListenInfos::new(ListenInfo {
                        protocol: Protocol::Udp,
                        ip: listen.listen_ip,
                        announced_address: Some(listen.announced_ip.to_string()),
                        expose_internal_ip: false,
                        port: None,
                        port_range: Some(listen.rtc_min_port..=listen.rtc_max_port),
                        flags: None,
                        send_buffer_size: None,
                        recv_buffer_size: None,
                    }),
                );
                opts.enable_sctp = true;
                opts.max_send_message_size = 262_144;
                opts
            };

            let consumer_transport = match router
                .create_webrtc_transport(make_transport_opts())
                .await
            {
                Ok(t) => t,
                Err(e) => {
                    log::error!("create consumer transport: {e}");
                    address.do_send(InternalMessage::Stop);
                    return;
                }
            };

            // Separate SendTransport for the viewer's neko-input DataChannel.
            // mediasoup-client gates produceData() on SendTransport only;
            // RecvTransport can only consume.
            let input_transport = match router
                .create_webrtc_transport(make_transport_opts())
                .await
            {
                Ok(t) => t,
                Err(e) => {
                    log::error!("create input transport: {e}");
                    address.do_send(InternalMessage::Stop);
                    return;
                }
            };

            let plain_producer_id = state
                .plain_producer
                .get()
                .map(|h| h.producer.id());

            let sctp_parameters = consumer_transport.sctp_parameters();
            let input_sctp_parameters = input_transport.sctp_parameters();

            address.do_send(StoreTransportThenInit {
                consumer_transport,
                input_transport,
                router_caps: router.rtp_capabilities().clone(),
                plain_producer_id,
                sctp_parameters,
                input_sctp_parameters,
            });
        });
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        log::info!("ws viewer closed");
    }
}

#[derive(Message)]
#[rtype(result = "()")]
struct StoreTransportThenInit {
    consumer_transport: WebRtcTransport,
    input_transport: WebRtcTransport,
    router_caps: RtpCapabilitiesFinalized,
    plain_producer_id: Option<ProducerId>,
    sctp_parameters: Option<SctpParameters>,
    input_sctp_parameters: Option<SctpParameters>,
}

impl Handler<StoreTransportThenInit> for ViewerConnection {
    type Result = ();

    fn handle(&mut self, msg: StoreTransportThenInit, ctx: &mut Self::Context) {
        let init = ServerMessage::Init {
            consumer_transport_options: TransportOptions {
                id: msg.consumer_transport.id(),
                dtls_parameters: msg.consumer_transport.dtls_parameters(),
                ice_candidates: msg.consumer_transport.ice_candidates().clone(),
                ice_parameters: msg.consumer_transport.ice_parameters().clone(),
            },
            input_transport_options: TransportOptions {
                id: msg.input_transport.id(),
                dtls_parameters: msg.input_transport.dtls_parameters(),
                ice_candidates: msg.input_transport.ice_candidates().clone(),
                ice_parameters: msg.input_transport.ice_parameters().clone(),
            },
            router_rtp_capabilities: msg.router_caps,
            plain_producer_id: msg.plain_producer_id,
            sctp_parameters: msg.sctp_parameters,
            input_sctp_parameters: msg.input_sctp_parameters,
        };
        self.consumer_transport = Some(msg.consumer_transport);
        self.input_transport = Some(msg.input_transport);
        ctx.address().do_send(init);
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for ViewerConnection {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        match msg {
            Ok(ws::Message::Ping(b)) => ctx.pong(&b),
            Ok(ws::Message::Pong(_)) => {}
            Ok(ws::Message::Text(t)) => match serde_json::from_str::<ClientMessage>(&t) {
                Ok(c) => ctx.address().do_send(c),
                Err(e) => log::warn!("parse: {e} — payload {t}"),
            },
            Ok(ws::Message::Close(reason)) => {
                ctx.close(reason);
                ctx.stop();
            }
            _ => ctx.stop(),
        }
    }
}

impl Handler<ClientMessage> for ViewerConnection {
    type Result = ();

    fn handle(&mut self, msg: ClientMessage, ctx: &mut Self::Context) {
        match msg {
            ClientMessage::Init { rtp_capabilities } => {
                self.client_caps = Some(rtp_capabilities);
            }
            ClientMessage::ConnectConsumerTransport { dtls_parameters } => {
                let Some(transport) = self.consumer_transport.clone() else {
                    log::warn!("ConnectConsumerTransport before transport ready");
                    return;
                };
                let address = ctx.address();
                actix::spawn(async move {
                    match transport
                        .connect(WebRtcTransportRemoteParameters { dtls_parameters })
                        .await
                    {
                        Ok(_) => address.do_send(ServerMessage::ConnectedConsumerTransport),
                        Err(e) => {
                            log::error!("consumer connect: {e}");
                            address.do_send(InternalMessage::Stop);
                        }
                    }
                });
            }
            ClientMessage::ConnectInputTransport { dtls_parameters } => {
                let Some(transport) = self.input_transport.clone() else {
                    log::warn!("ConnectInputTransport before transport ready");
                    return;
                };
                let address = ctx.address();
                actix::spawn(async move {
                    match transport
                        .connect(WebRtcTransportRemoteParameters { dtls_parameters })
                        .await
                    {
                        Ok(_) => address.do_send(ServerMessage::ConnectedInputTransport),
                        Err(e) => {
                            log::error!("input connect: {e}");
                            address.do_send(InternalMessage::Stop);
                        }
                    }
                });
            }
            ClientMessage::Consume { producer_id } => {
                let Some(transport) = self.consumer_transport.clone() else {
                    log::warn!("Consume before transport ready");
                    return;
                };
                let Some(caps) = self.client_caps.clone() else {
                    log::warn!("Consume before client Init");
                    return;
                };
                let address = ctx.address();
                actix::spawn(async move {
                    let mut options = ConsumerOptions::new(producer_id, caps);
                    options.paused = true;
                    match transport.consume(options).await {
                        Ok(consumer) => {
                            let id = consumer.id();
                            let kind = consumer.kind();
                            let rtp_parameters = consumer.rtp_parameters().clone();
                            address.do_send(ServerMessage::Consumed {
                                id,
                                producer_id,
                                kind,
                                rtp_parameters,
                            });
                            address.do_send(InternalMessage::SaveConsumer(consumer));
                            log::info!("{kind:?} consumer id={id}");
                        }
                        Err(e) => {
                            log::error!("create consumer: {e}");
                            address.do_send(InternalMessage::Stop);
                        }
                    }
                });
            }
            ClientMessage::ConsumerResume { id } => {
                if let Some(consumer) = self.consumers.get(&id).cloned() {
                    actix::spawn(async move {
                        if let Err(e) = consumer.resume().await {
                            log::error!("resume {id}: {e}");
                        }
                    });
                }
            }
            ClientMessage::ProduceInput {
                sctp_stream_parameters,
                label,
                protocol,
            } => {
                let Some(transport) = self.input_transport.clone() else {
                    log::warn!("ProduceInput before input transport ready");
                    return;
                };
                let address = ctx.address();
                let app_state = self.state.clone();
                actix::spawn(async move {
                    let mut opts = DataProducerOptions::new_sctp(sctp_stream_parameters);
                    opts.label = label.clone();
                    opts.protocol = protocol.clone();
                    let producer = match transport.produce_data(opts).await {
                        Ok(p) => p,
                        Err(e) => {
                            log::error!("produce_data: {e}");
                            return;
                        }
                    };
                    log::info!(
                        "input data producer ready id={} label={} proto={}",
                        producer.id(),
                        label,
                        protocol,
                    );

                    // Bridge the SCTP DataProducer into the shared
                    // DirectTransport so we can observe each payload via
                    // on_message. Phase 1+ replaces the observer with a
                    // forward to Neko WebSocket admin (signal/keyboard,
                    // signal/mouse).
                    let dt = match ensure_direct_transport(&app_state).await {
                        Ok(t) => t.clone(),
                        Err(e) => {
                            log::error!("direct transport: {e:?}");
                            return;
                        }
                    };
                    let consumer_opts =
                        DataConsumerOptions::new_sctp_ordered(producer.id());
                    let consumer = match dt.consume_data(consumer_opts).await {
                        Ok(c) => c,
                        Err(e) => {
                            log::error!("consume_data on direct transport: {e}");
                            return;
                        }
                    };
                    let counter = Arc::new(StdMutex::new(0u64));
                    let addr_for_obs = address.clone();
                    consumer
                        .on_message({
                            let counter = counter.clone();
                            move |msg| {
                                let bytes = match &msg {
                                    WebRtcMessage::String(s) => s.len(),
                                    WebRtcMessage::Binary(b) => b.len(),
                                    WebRtcMessage::EmptyString
                                    | WebRtcMessage::EmptyBinary => 0,
                                };
                                let mut g = counter.lock().unwrap();
                                *g += 1;
                                let seq = *g;
                                addr_for_obs.do_send(InternalMessage::InputBroadcast {
                                    sequence: seq,
                                    bytes,
                                });
                            }
                        })
                        .detach();
                    address.do_send(InternalMessage::SaveInputProducer(producer));
                    address.do_send(InternalMessage::SaveInputConsumer(consumer));
                });
            }
        }
    }
}

impl Handler<ServerMessage> for ViewerConnection {
    type Result = ();
    fn handle(&mut self, message: ServerMessage, ctx: &mut Self::Context) {
        ctx.text(serde_json::to_string(&message).unwrap());
    }
}

impl Handler<InternalMessage> for ViewerConnection {
    type Result = ();
    fn handle(&mut self, message: InternalMessage, ctx: &mut Self::Context) {
        match message {
            InternalMessage::Stop => ctx.stop(),
            InternalMessage::SaveConsumer(c) => {
                self.consumers.insert(c.id(), c);
            }
            InternalMessage::SaveInputProducer(p) => {
                self.input_data_producer = Some(p);
            }
            InternalMessage::SaveInputConsumer(c) => {
                self.input_data_consumer = Some(c);
            }
            InternalMessage::InputBroadcast { sequence, bytes } => {
                self.input_messages_seen = sequence;
                self.input_bytes_seen += bytes as u64;
                if sequence % 10 == 1 || sequence < 5 {
                    log::info!(
                        "input received seq={} bytes={} total_msgs={} total_bytes={}",
                        sequence,
                        bytes,
                        self.input_messages_seen,
                        self.input_bytes_seen,
                    );
                }
                ctx.address().do_send(ServerMessage::InputAck {
                    sequence,
                    received_bytes: bytes,
                });
            }
        }
    }
}

async fn ws_index(
    request: HttpRequest,
    state: Data<AppState>,
    stream: Payload,
) -> Result<HttpResponse, Error> {
    ws::start(ViewerConnection::new(state.into_inner()), &request, stream)
}

fn parse_env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(
        env_logger::Env::new().default_filter_or("info,sfu_main=debug,mediasoup=info"),
    );

    let bind_host = env::var("SFU_LISTEN_HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let bind_port: u16 = parse_env_or("SFU_LISTEN_PORT", 4443u16);
    let listen = SfuListen {
        listen_ip: bind_host.parse().unwrap_or(IpAddr::from([0, 0, 0, 0])),
        announced_ip: env::var("SFU_PUBLIC_IP")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(IpAddr::from([127, 0, 0, 1])),
        rtc_min_port: parse_env_or("SFU_RTC_PORT_MIN", 40_000u16),
        rtc_max_port: parse_env_or("SFU_RTC_PORT_MAX", 40_100u16),
        plain_rtp_port: parse_env_or("SFU_PLAIN_RTP_PORT", 5004u16),
        plain_rtcp_port: parse_env_or("SFU_PLAIN_RTCP_PORT", 5005u16),
    };

    let state = Data::new(AppState {
        listen,
        worker_manager: WorkerManager::new(),
        router: tokio::sync::OnceCell::new(),
        plain_producer: tokio::sync::OnceCell::new(),
        direct_transport: tokio::sync::OnceCell::new(),
    });

    log::info!(
        "PoC SFU listening :{} | announced_ip={} | rtc={}-{} | plain_rtp=:{}",
        bind_port,
        listen.announced_ip,
        listen.rtc_min_port,
        listen.rtc_max_port,
        listen.plain_rtp_port,
    );

    HttpServer::new(move || {
        App::new()
            .wrap(
                Cors::default()
                    .allow_any_origin()
                    .allow_any_method()
                    .allow_any_header()
                    .max_age(3600),
            )
            .app_data(state.clone())
            .route("/ws", web::get().to(ws_index))
            .route(
                "/v1/plain-producer",
                web::post().to(plain_producer_handler),
            )
    })
    .workers(2)
    .bind((bind_host.as_str(), bind_port))?
    .run()
    .await
}

// keep clippy happy: StdMutex unused, kept for potential future expansion.
#[allow(dead_code)]
fn _unused_marker(_x: StdMutex<()>) {}
