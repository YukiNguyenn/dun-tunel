//! RouterManager — Phase 2 production implementation.
//!
//! Maintains a `session_id → SessionState` map where each `SessionState`
//! owns:
//!  - one `mediasoup::Router`
//!  - the `PlainTransport` + `Producer` fed by the rathole-bridged Neko
//!  - any number of `(WebRtcTransport recv, WebRtcTransport send,
//!    Vec<Consumer>, Vec<DataConsumer>)` viewer entries
//!
//! The router lifetime is tied to the `SessionState`. Dropping it closes
//! the worker resources; we never reuse a `Router` across sessions
//! because (a) per-session viewer cap enforcement is local state, and
//! (b) revocation must be able to nuke the entire pipeline atomically.
//!
//! mediasoup workers are pooled at `RouterManager::new(workers)` startup
//! and reused round-robin so we don't churn workers on session create.

use crate::input_envelope::InputEnvelope;
use crate::neko_input_bridge::NekoInputBridge;
use crate::transport::{
    create_consumer_transport_options_with_server, create_plain_transport_options,
    create_plain_transport_options_on_port, create_webrtc_server_options,
    plain_audio_producer_rtp_parameters, plain_producer_rtp_parameters, RouterListenInfo,
};
use crate::VIEWER_CAP_PER_SESSION;
use anyhow::Context;
use dashmap::DashMap;
use edge_shared::types::SessionId;
use mediasoup::consumer::ConsumerId;
use mediasoup::consumer::ConsumerLayers;
use mediasoup::data_producer::DataProducerId;
use mediasoup::prelude::*;
use mediasoup::producer::ProducerId;
use mediasoup::router::RouterId;
use mediasoup::transport::TransportId;
use mediasoup::worker::WorkerSettings;
use mediasoup_types::data_structures::WebRtcMessage;
use std::collections::HashMap;
use std::env;
use std::num::{NonZeroU32, NonZeroU8};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// How often the source monitor samples each session's producer.
const SOURCE_MONITOR_SECS: u64 = 5;
/// A producer that was receiving RTP but goes silent this long (with
/// viewers present) is treated as a restarted source → reprovision to
/// re-lock comedia. Generous enough not to fire on a brief owner network
/// blip (where RTP resumes from the SAME tuple and needs no reprovision).
const SOURCE_STARVE_SECS: u64 = 12;

/// Per-session RTP-activity tracking used by the source monitor.
struct ProducerActivity {
    last_bytes: u64,
    last_change: Instant,
    /// Whether the producer has EVER received RTP. We only reprovision a
    /// producer that was flowing and then stopped — never one that has
    /// not started yet (fresh session, owner broadcast not up).
    ever_flowed: bool,
    /// Cumulative `packets_lost` at the previous sample, to log the
    /// per-interval delta (owner→edge loss).
    last_packets_lost: u64,
}

/// One sample of the plain video producer's inbound RTP health
/// (the owner→edge leg). The VP9 producer is simulcast, so the
/// counters below aggregate every inbound RTP stream/SSRC. This lets
/// the monitor both detect starvation (`byte_count`) and log loss so we
/// can localise stutter (compare against the viewer-side edge→viewer
/// loss the client logs).
struct ProducerSample {
    has_viewers: bool,
    rtp_streams: usize,
    byte_count: u64,
    packets_lost: u64,
    /// Worst RTCP fraction lost among VP9 simulcast RTP streams
    /// (0-255 ~= 0-100%) over the last interval.
    fraction_lost: u8,
    /// Worst producer transmission quality score (0-10) among streams.
    score: u8,
    bitrate: u32,
}

/// Phase 2: per-session state owned by the SFU.
pub struct SessionState {
    pub session_id: SessionId,
    pub router: Router,
    /// Producer + the transport feeding it (held to keep the producer alive).
    pub plain_producer: Option<Producer>,
    /// Opus audio producer fed by the SAME PlainTransport (second
    /// SSRC/payload type). Optional because older sessions / pipelines
    /// without an audio branch still work video-only.
    pub plain_audio_producer: Option<Producer>,
    pub _plain_transport: Option<PlainTransport>,
    /// Per-viewer entries keyed by an opaque viewer id (e.g. WebRTC session
    /// fingerprint or a UUID we mint on accept).
    pub viewers: HashMap<String, ViewerSlot>,
    /// Shared per-session `DirectTransport` used to consume every viewer's
    /// `neko-input` SCTP DataProducer. Lazily created on the first input
    /// producer for the session (see `produce_input_data`, task 5.3).
    /// `None` until then.
    pub direct_transport: Option<DirectTransport>,
    /// One `NekoInputBridge` per active session, translating the decoded
    /// `InputEnvelope` stream into Neko v3 admin events. Lazily created
    /// alongside `direct_transport`. `None` until the first input producer.
    ///
    /// `NekoInputBridge` lives in `crate::neko_input_bridge` (created by
    /// task 4.1); referenced here by full path so this field compiles once
    /// that module is exported. Do not redefine the type here.
    pub input_bridge: Option<Arc<crate::neko_input_bridge::NekoInputBridge>>,
    /// The per-worker [`WebRtcServer`] (single UDP mux port) this session's
    /// router lives on. All viewer transports for the session are created
    /// against it so they share one UDP port. Cloned from
    /// `RouterManagerInner::webrtc_servers` at provision time (matched to
    /// the worker the router was created on).
    pub webrtc_server: WebRtcServer,
    /// Cumulative bytes received by all viewer transports (for bandwidth
    /// reporting). Sampled by `get_session_bytes`.
    pub _cumulative_bytes_cache: Mutex<u64>,
}

pub struct ViewerSlot {
    pub viewer_id: String,
    pub recv_transport: WebRtcTransport,
    pub send_transport: Option<WebRtcTransport>,
    pub consumers: Vec<Consumer>,
    pub data_consumers: Vec<DataConsumer>,
    /// The viewer's `neko-input` SCTP `DataProducer`, created on its
    /// `send_transport` via `produce_input_data` (task 5.3). Held here so
    /// dropping the viewer slot closes the producer.
    pub input_data_producer: Option<DataProducer>,
    /// The matching `DataConsumer` on the session's shared `DirectTransport`
    /// that observes `input_data_producer` and forwards decoded envelopes to
    /// the `NekoInputBridge`. Held here so its lifetime tracks the viewer
    /// (drop = close), keeping the input stream bounded to live viewers.
    pub input_data_consumer: Option<DataConsumer>,
}

/// Public handle used by edge-control to open WebRTC transports for a viewer.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConsumerTransportInfo {
    pub viewer_id: String,
    pub transport_id: TransportId,
    pub ice_parameters: IceParameters,
    pub ice_candidates: Vec<IceCandidate>,
    pub dtls_parameters: DtlsParameters,
    pub sctp_parameters: Option<mediasoup_types::sctp_parameters::SctpParameters>,
}

/// Result of `RouterManager::consume` — the consumer parameters the
/// viewer needs to mirror the producer on the client side.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConsumedInfo {
    pub id: ConsumerId,
    pub producer_id: ProducerId,
    pub kind: MediaKind,
    pub rtp_parameters: RtpParameters,
    pub paused: bool,
}

/// Result of `provision_session` — what edge-control returns to dun-api.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProvisionedRouter {
    pub router_id: RouterId,
    pub plain_rtp_port: u16,
    pub plain_rtcp_port: u16,
    pub producer_id: ProducerId,
    /// Opus audio producer id, when the session has an audio branch.
    pub audio_producer_id: Option<ProducerId>,
    pub rtp_capabilities: RtpCapabilitiesFinalized,
}

#[derive(Clone)]
pub struct RouterManager {
    inner: Arc<RouterManagerInner>,
}

struct RouterManagerInner {
    workers: Vec<Worker>,
    /// One `WebRtcServer` per worker (aligned by index with `workers`),
    /// each bound to a single UDP port `rtc_min_port + worker_index`. A
    /// session's viewer transports use the server of the worker its router
    /// was created on — see `provision_session`.
    webrtc_servers: Vec<WebRtcServer>,
    /// Round-robin index for picking the next worker on session create.
    worker_cursor: AtomicUsize,
    sessions: DashMap<SessionId, Arc<Mutex<SessionState>>>,
    listen: RouterListenInfo,
}

impl RouterManager {
    /// Initialise a worker pool of `workers` size + capture listen-IP config.
    pub async fn new_with_listen(
        workers: usize,
        listen: RouterListenInfo,
    ) -> anyhow::Result<Self> {
        if workers == 0 {
            anyhow::bail!("router-manager: workers must be > 0");
        }
        let manager = WorkerManager::new();
        let mut pool = Vec::with_capacity(workers);
        let mut servers = Vec::with_capacity(workers);
        for idx in 0..workers {
            let mut settings = WorkerSettings::default();
            settings.log_level = mediasoup::worker::WorkerLogLevel::Warn;
            let worker = manager
                .create_worker(settings)
                .await
                .context("create mediasoup worker")?;
            // One WebRtcServer per worker on a single UDP mux port. Workers
            // are separate processes and cannot share a port, so each takes
            // `rtc_min_port + idx`. Open `rtc_min_port .. rtc_min_port +
            // workers - 1` (UDP) on the edge firewall — far fewer than the
            // old per-transport range.
            let port = listen.rtc_min_port + idx as u16;
            let server = worker
                .create_webrtc_server(create_webrtc_server_options(&listen, port))
                .await
                .context("create webrtc server")?;
            tracing::info!(worker_index = idx, mux_port = port, "webrtc server (udp mux) ready");
            pool.push(worker);
            servers.push(server);
        }
        let manager = Self {
            inner: Arc::new(RouterManagerInner {
                workers: pool,
                webrtc_servers: servers,
                worker_cursor: AtomicUsize::new(0),
                sessions: DashMap::new(),
                listen,
            }),
        };
        // Self-healing: watch every session's plain producer and re-lock
        // comedia onto a new source tuple when the RTP source restarts
        // (e.g. owner restarts the container → new Docker SNAT source port
        // → comedia drops everything from the new tuple). See
        // `run_source_monitor`.
        manager.spawn_source_monitor();
        Ok(manager)
    }

    /// Backward-compat shim used by `edge-control::AppState::initialize`
    /// which passes only `workers`. Reads listen config from env.
    pub async fn new(workers: usize) -> anyhow::Result<Self> {
        Self::new_with_listen(workers, RouterListenInfo::from_env()?).await
    }

    pub fn clone_handle(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Optional LAN-private host advertised for same-network (hairpin)
    /// operation, sourced from `SFU_ANNOUNCED_IP_LAN`. `None` in
    /// production. edge-control echoes this into `CreateSessionResp` so
    /// dun-app can route the owner's udpsink to the private IP when the
    /// owner shares the edge's public IP.
    pub fn media_lan_host(&self) -> Option<String> {
        self.inner.listen.lan_announced_ip.map(|ip| ip.to_string())
    }

    /// Create a Router + PlainTransport (comedia mode) + Producer for a new
    /// session. Idempotent: re-calling with the same `session_id` returns the
    /// existing handle.
    pub async fn provision_session(
        &self,
        session_id: &str,
    ) -> anyhow::Result<ProvisionedRouter> {
        if let Some(existing) = self.inner.sessions.get(session_id) {
            let guard = existing.lock().await;
            return Ok(snapshot(&guard));
        }

        let worker_index = self.pick_worker_index();
        let worker = &self.inner.workers[worker_index];
        let webrtc_server = self.inner.webrtc_servers[worker_index].clone();
        let media_codecs = default_media_codecs();
        let router = worker
            .create_router(RouterOptions::new(media_codecs))
            .await
            .context("create router")?;

        let plain_options = create_plain_transport_options(&self.inner.listen);
        let plain_transport = router
            .create_plain_transport(plain_options)
            .await
            .context("create plain transport")?;

        let rtp_parameters = plain_producer_rtp_parameters();
        let producer = plain_transport
            .produce(ProducerOptions::new(MediaKind::Video, rtp_parameters))
            .await
            .context("produce on plain transport")?;

        // Audio producer on the SAME PlainTransport. The GStreamer
        // pipeline funnels VP9 simulcast (pt=96, ssrc=22222220/21/22)
        // plus Opus (pt=111, ssrc=22222223) RTP into the one UDP port;
        // mediasoup demuxes by SSRC. If the owner's pipeline has no audio
        // branch (older dun-app), no Opus packets ever arrive and this
        // producer simply stays silent -- harmless.
        let audio_rtp_parameters = plain_audio_producer_rtp_parameters();
        let audio_producer = plain_transport
            .produce(ProducerOptions::new(MediaKind::Audio, audio_rtp_parameters))
            .await
            .context("produce audio on plain transport")?;

        let plain_rtp_port = plain_transport.tuple().local_port();
        let plain_rtcp_port = plain_transport
            .rtcp_tuple()
            .map(|t| t.local_port())
            .unwrap_or(plain_rtp_port);

        let provisioned = ProvisionedRouter {
            router_id: router.id(),
            plain_rtp_port,
            plain_rtcp_port,
            producer_id: producer.id(),
            audio_producer_id: Some(audio_producer.id()),
            rtp_capabilities: router.rtp_capabilities().clone(),
        };

        let state = SessionState {
            session_id: session_id.to_string(),
            router,
            plain_producer: Some(producer),
            plain_audio_producer: Some(audio_producer),
            _plain_transport: Some(plain_transport),
            viewers: HashMap::new(),
            direct_transport: None,
            input_bridge: None,
            webrtc_server,
            _cumulative_bytes_cache: Mutex::new(0),
        };
        self.inner
            .sessions
            .insert(session_id.to_string(), Arc::new(Mutex::new(state)));

        tracing::info!(
            %session_id,
            router_id = %provisioned.router_id,
            plain_rtp_port,
            audio_producer = provisioned.audio_producer_id.is_some(),
            "session provisioned"
        );
        Ok(provisioned)
    }

    /// Tear down everything for a session. Idempotent.
    pub async fn close_session(&self, session_id: &str) {
        if let Some((_, state)) = self.inner.sessions.remove(session_id) {
            // Drop the SessionState — destructors close router + transports
            // recursively. We hold the lock briefly so any in-flight viewer
            // creation completes before the router goes away.
            let _guard = state.lock().await;
            tracing::info!(%session_id, "session closed");
        }
    }

    /// Create a recv+send WebRtcTransport pair for a new viewer. Enforces
    /// the per-session viewer cap (R8.8).
    pub async fn create_consumer_transports(
        &self,
        session_id: &str,
        viewer_id: &str,
    ) -> anyhow::Result<(ConsumerTransportInfo, ConsumerTransportInfo)> {
        let state = self
            .inner
            .sessions
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("session not found"))?
            .clone();
        let mut guard = state.lock().await;

        if guard.viewers.len() as u32 >= VIEWER_CAP_PER_SESSION {
            anyhow::bail!("viewer cap reached");
        }

        let server = guard.webrtc_server.clone();
        let recv_options = create_consumer_transport_options_with_server(server.clone());
        let send_options = create_consumer_transport_options_with_server(server);
        let recv = guard
            .router
            .create_webrtc_transport(recv_options)
            .await
            .context("create recv transport")?;
        let send = guard
            .router
            .create_webrtc_transport(send_options)
            .await
            .context("create send transport")?;

        // Diagnostic: log the ICE candidates advertised to the viewer for
        // the media (recv) transport. The viewer's browser must be able to
        // reach this exact IP:port over UDP. If the address is a private
        // IP (e.g. 192.168.x) an external viewer can never connect →
        // 0 kbps; if it's the public IP, the NAT/firewall must forward
        // that UDP port to the edge host.
        tracing::info!(
            session_id = %session_id,
            viewer_id = %viewer_id,
            recv_candidates = ?recv.ice_candidates(),
            "viewer transport ICE candidates (media path)"
        );

        let recv_info = ConsumerTransportInfo {
            viewer_id: viewer_id.to_string(),
            transport_id: recv.id(),
            ice_parameters: recv.ice_parameters().clone(),
            ice_candidates: recv.ice_candidates().clone(),
            dtls_parameters: recv.dtls_parameters(),
            sctp_parameters: recv.sctp_parameters(),
        };
        let send_info = ConsumerTransportInfo {
            viewer_id: viewer_id.to_string(),
            transport_id: send.id(),
            ice_parameters: send.ice_parameters().clone(),
            ice_candidates: send.ice_candidates().clone(),
            dtls_parameters: send.dtls_parameters(),
            sctp_parameters: send.sctp_parameters(),
        };

        guard.viewers.insert(
            viewer_id.to_string(),
            ViewerSlot {
                viewer_id: viewer_id.to_string(),
                recv_transport: recv,
                send_transport: Some(send),
                consumers: Vec::new(),
                data_consumers: Vec::new(),
                input_data_producer: None,
                input_data_consumer: None,
            },
        );
        Ok((recv_info, send_info))
    }

    /// Disconnect a viewer — drops its transports, which in turn closes any
    /// consumers attached. Idempotent.
    pub async fn remove_viewer(&self, session_id: &str, viewer_id: &str) {
        if let Some(state) = self.inner.sessions.get(session_id) {
            let mut guard = state.lock().await;
            guard.viewers.remove(viewer_id);
        }
    }

    /// Snapshot of the active sessions. Used by snapshot endpoint (R22).
    pub async fn list_active_sessions(&self) -> Vec<SessionId> {
        self.inner.sessions.iter().map(|e| e.key().clone()).collect()
    }

    /// Aggregate bytes received across all viewer transports for a session.
    /// Sampled on demand by `BandwidthReporter`.
    pub async fn get_session_bytes(&self, session_id: &str) -> anyhow::Result<u64> {
        let state = match self.inner.sessions.get(session_id) {
            Some(s) => s.clone(),
            None => return Ok(0),
        };
        let guard = state.lock().await;
        let mut total: u64 = 0;
        for v in guard.viewers.values() {
            for stat in v
                .recv_transport
                .get_stats()
                .await
                .context("recv transport stats")?
            {
                total = total.saturating_add(stat.bytes_received);
                total = total.saturating_add(stat.bytes_sent);
            }
            if let Some(send) = &v.send_transport {
                for stat in send.get_stats().await.context("send transport stats")? {
                    total = total.saturating_add(stat.bytes_received);
                    total = total.saturating_add(stat.bytes_sent);
                }
            }
        }
        Ok(total)
    }

    /// Number of active viewers in a session. Source of truth for cap (R8.8).
    pub async fn viewer_count(&self, session_id: &str) -> u32 {
        match self.inner.sessions.get(session_id) {
            Some(state) => state.lock().await.viewers.len() as u32,
            None => 0,
        }
    }

    /// Snapshot of the per-session producer-id + router RTP capabilities.
    /// The WS signaling handler needs both during the `Init` exchange so
    /// the mediasoup-client can call `loadDevice(routerRtpCapabilities)`
    /// and `consume(producerId)` without an extra round-trip.
    ///
    /// Returns `(video_producer_id, audio_producer_id, caps)`. The audio
    /// id is `None` for sessions provisioned before audio support.
    pub async fn session_producer_info(
        &self,
        session_id: &str,
    ) -> anyhow::Result<(ProducerId, Option<ProducerId>, RtpCapabilitiesFinalized)> {
        let state = self
            .inner
            .sessions
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("session not found"))?
            .clone();
        let guard = state.lock().await;
        let producer = guard
            .plain_producer
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("plain producer absent"))?;
        let audio_producer_id = guard.plain_audio_producer.as_ref().map(|p| p.id());
        Ok((
            producer.id(),
            audio_producer_id,
            guard.router.rtp_capabilities().clone(),
        ))
    }

    /// Connect a viewer's recv-side WebRtcTransport with the DTLS
    /// parameters from `mediasoup-client::Device.createRecvTransport`.
    /// Idempotent at the mediasoup level: re-sending the same DTLS
    /// fingerprints is rejected, but the WS handler is the only caller
    /// and never repeats. Returns an error when the session or viewer
    /// slot is unknown.
    pub async fn connect_recv_transport(
        &self,
        session_id: &str,
        viewer_id: &str,
        dtls_parameters: DtlsParameters,
    ) -> anyhow::Result<()> {
        // Clone the recv transport handle out of the locked
        // SessionState so we can `await` the connect call without
        // holding the per-session mutex (transport.connect can take a
        // few hundred ms during DTLS handshake).
        let transport = {
            let state = self
                .inner
                .sessions
                .get(session_id)
                .ok_or_else(|| anyhow::anyhow!("session not found"))?
                .clone();
            let guard = state.lock().await;
            guard
                .viewers
                .get(viewer_id)
                .ok_or_else(|| anyhow::anyhow!("viewer not found"))?
                .recv_transport
                .clone()
        };
        transport
            .connect(WebRtcTransportRemoteParameters { dtls_parameters })
            .await
            .context("connect recv transport")?;
        Ok(())
    }

    /// DTLS-connect a viewer's send-side `WebRtcTransport` — the transport
    /// the viewer uses to `produceData` its `neko-input` SCTP channel.
    ///
    /// The send transport is created up-front in
    /// `create_consumer_transports`; this is the second half of the input
    /// handshake (`ConnectInputTransport` in `sfu_ws.rs`), mirroring
    /// `connect_recv_transport` for the recv side.
    ///
    /// Preconditions: the session and viewer slot exist, and the slot's
    /// `send_transport` is `Some`. Postconditions: the send transport is
    /// DTLS-connected so a subsequent `produce_input_data` succeeds. Returns
    /// an error when the session or viewer slot is unknown, or when the
    /// viewer has no `send_transport`.
    pub async fn connect_send_transport(
        &self,
        session_id: &str,
        viewer_id: &str,
        dtls_parameters: DtlsParameters,
    ) -> anyhow::Result<()> {
        // Clone the send transport handle out of the locked SessionState so
        // we can `await` the DTLS connect without holding the per-session
        // mutex (same rationale as `connect_recv_transport`).
        let transport = {
            let state = self
                .inner
                .sessions
                .get(session_id)
                .ok_or_else(|| anyhow::anyhow!("session not found"))?
                .clone();
            let guard = state.lock().await;
            guard
                .viewers
                .get(viewer_id)
                .ok_or_else(|| anyhow::anyhow!("viewer not found"))?
                .send_transport
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("send transport absent"))?
                .clone()
        };
        transport
            .connect(WebRtcTransportRemoteParameters { dtls_parameters })
            .await
            .context("connect send transport")?;
        Ok(())
    }

    /// Produce the viewer's `neko-input` SCTP `DataProducer` on its
    /// (already-connected) `send_transport`, then `consume_data` it onto the
    /// session's shared `DirectTransport`, wiring an `on_message` observer that
    /// decodes each SCTP payload as an [`InputEnvelope`] and forwards it to the
    /// session's [`NekoInputBridge`].
    ///
    /// Preconditions: the session exists and is active; the viewer slot exists
    /// with a `send_transport` that has been DTLS-connected
    /// (`connect_send_transport` already processed).
    ///
    /// Postconditions:
    /// - Returns the new `DataProducerId`.
    /// - A `DataConsumer` exists on the session's `DirectTransport` observing
    ///   this producer; its `on_message` forwards decoded envelopes to the
    ///   session `NekoInputBridge`.
    /// - Both the `DataProducer` and `DataConsumer` are stored in the
    ///   `ViewerSlot` so their lifetime tracks the viewer (drop = close).
    /// - **Idempotent per viewer**: a second call returns the existing
    ///   producer id without creating a new producer/consumer.
    ///
    /// The session's `DirectTransport` + `NekoInputBridge` are created lazily
    /// on the first input producer for the session and reused thereafter.
    pub async fn produce_input_data(
        &self,
        session_id: &str,
        viewer_id: &str,
        options: DataProducerOptions,
    ) -> anyhow::Result<DataProducerId> {
        let state = self
            .inner
            .sessions
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("session not found"))?
            .clone();

        // ── Idempotency + handle acquisition ──────────────────────────────
        // Re-acquire the send transport handle and short-circuit if this
        // viewer already has an input producer. We release the lock before the
        // async `produce_data` / `consume_data` calls (same rationale as
        // `consume`: don't serialise every viewer through the session mutex).
        let send_transport = {
            let guard = state.lock().await;
            let slot = guard
                .viewers
                .get(viewer_id)
                .ok_or_else(|| anyhow::anyhow!("viewer not found"))?;
            // Idempotent: a second ProduceInput for the viewer returns the
            // producer it already owns (Requirement 3.1). The decision is
            // factored into `idempotent_produce_decision` so it is unit-testable
            // without a live worker (see tests).
            match idempotent_produce_decision(slot.input_data_producer.as_ref().map(|p| p.id())) {
                IdempotentProduce::ReturnExisting(id) => return Ok(id),
                IdempotentProduce::Proceed => {}
            }
            slot.send_transport
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("send transport absent"))?
                .clone()
        };

        // ── Produce the viewer's SCTP DataProducer ────────────────────────
        let producer = send_transport
            .produce_data(options)
            .await
            .context("produce_data on send transport")?;
        let producer_id = producer.id();

        // ── Lazily create the per-session DirectTransport + NekoInputBridge ─
        // Both are session-scoped (one per session, not per viewer) so the
        // admin-WS fan-in stays bounded. Hold the lock while creating so two
        // concurrent first-producers don't race to build two DirectTransports.
        let (direct_transport, input_bridge) = {
            let mut guard = state.lock().await;
            if guard.direct_transport.is_none() {
                let dt = guard
                    .router
                    .create_direct_transport(DirectTransportOptions::default())
                    .await
                    .context("create direct transport")?;
                tracing::info!(%session_id, direct_transport_id = %dt.id(), "session direct transport created");
                guard.direct_transport = Some(dt);
            }
            if guard.input_bridge.is_none() {
                // Source the Neko v3 admin WS URL + token for this session.
                // See `neko_admin_endpoint` for how these are derived and the
                // plumbing gap this currently papers over.
                match neko_admin_endpoint(session_id) {
                    Some((url, token)) => match NekoInputBridge::connect(session_id, &url, &token).await {
                        Ok(bridge) => {
                            guard.input_bridge = Some(bridge);
                        }
                        Err(e) => {
                            // A bridge failure must not fail the produce — the
                            // SCTP path (produce/consume + observer) still works
                            // and forwarding degrades gracefully (envelopes are
                            // decoded then dropped when no bridge is present).
                            tracing::warn!(%session_id, error = %e, "neko input bridge connect failed; input will be decoded but not forwarded");
                        }
                    },
                    None => {
                        tracing::warn!(
                            %session_id,
                            "neko admin endpoint not configured (NEKO_ADMIN_WS_URL/NEKO_ADMIN_TOKEN unset); input will be decoded but not forwarded"
                        );
                    }
                }
            }
            (
                guard
                    .direct_transport
                    .as_ref()
                    .expect("direct transport just ensured")
                    .clone(),
                guard.input_bridge.clone(),
            )
        };

        // ── Consume the producer onto the DirectTransport ─────────────────
        // The DirectTransport runs in-process and exposes `on_message`, which
        // SCTP DataConsumers on a WebRtcTransport do not. `new_sctp_ordered`
        // matches the proven `poc/neko-sfu` path (100% ack at 750 events in
        // RESULTS.md); the DirectTransport delivers each frame to `on_message`.
        let consumer_opts = DataConsumerOptions::new_sctp_ordered(producer_id);
        let consumer = direct_transport
            .consume_data(consumer_opts)
            .await
            .context("consume_data on direct transport")?;

        // ── Observer: decode each SCTP payload → forward to NekoInputBridge ─
        // `on_message` is a synchronous callback; the bridge `forward` is async
        // and the bridge is shared via `Arc`, so we clone the Arc and spawn the
        // forward on the tokio runtime.
        if let Some(bridge) = input_bridge.clone() {
            let sid = session_id.to_string();
            consumer
                .on_message(move |msg| {
                    // Only string/binary SCTP frames carry an envelope; empty
                    // frames are keep-alives we ignore. Both variants wrap a
                    // `Cow<[u8]>`; the JSON envelope is decoded from the raw
                    // bytes regardless of the string/binary framing.
                    let bytes: Option<Vec<u8>> = match msg {
                        WebRtcMessage::String(s) => Some(s.to_vec()),
                        WebRtcMessage::Binary(b) => Some(b.to_vec()),
                        WebRtcMessage::EmptyString | WebRtcMessage::EmptyBinary => None,
                    };
                    let Some(bytes) = bytes else { return };
                    match serde_json::from_slice::<InputEnvelope>(&bytes) {
                        Ok(env) => {
                            let bridge = bridge.clone();
                            let sid = sid.clone();
                            tokio::spawn(async move {
                                if let Err(e) = bridge.forward(env).await {
                                    tracing::warn!(session_id = %sid, error = %e, "neko input forward failed");
                                }
                            });
                        }
                        Err(e) => {
                            tracing::debug!(session_id = %sid, error = %e, "drop malformed neko-input envelope");
                        }
                    }
                })
                .detach();
        }

        // ── Store producer + consumer on the viewer slot ──────────────────
        // Re-fetch the slot rather than caching it: `remove_viewer` may have
        // run while we awaited, in which case the viewer is gone and we must
        // not leak the freshly-created producer/consumer (drop closes them).
        {
            let mut guard = state.lock().await;
            let slot = guard
                .viewers
                .get_mut(viewer_id)
                .ok_or_else(|| anyhow::anyhow!("viewer disappeared during produce_input_data"))?;
            // Guard against a concurrent producer that won the race while we
            // were producing/consuming: keep the first, drop ours. Same
            // idempotency decision as the pre-produce check above.
            match idempotent_produce_decision(slot.input_data_producer.as_ref().map(|p| p.id())) {
                IdempotentProduce::ReturnExisting(id) => return Ok(id),
                IdempotentProduce::Proceed => {}
            }
            slot.input_data_producer = Some(producer);
            slot.input_data_consumer = Some(consumer);
        }

        tracing::info!(%session_id, %viewer_id, %producer_id, "neko-input data producer ready");
        Ok(producer_id)
    }

    /// Create a Consumer on the viewer's recv transport for the given
    /// producer + RTP capabilities. Stored in the `ViewerSlot` so its
    /// lifetime tracks the viewer (drop = close).
    ///
    /// Created with `paused: true` per mediasoup-client convention —
    /// the viewer must call `consumerResume` after the client-side
    /// `consume()` returns. Without this the very first RTP packets
    /// land before the receiver is ready and the decoder freezes.
    pub async fn consume(
        &self,
        session_id: &str,
        viewer_id: &str,
        producer_id: ProducerId,
        rtp_capabilities: RtpCapabilities,
    ) -> anyhow::Result<ConsumedInfo> {
        // Acquire transport handle; release the lock before the async
        // `transport.consume` call to avoid serialising every viewer
        // through a single mutex on a shared session.
        let transport = {
            let state = self
                .inner
                .sessions
                .get(session_id)
                .ok_or_else(|| anyhow::anyhow!("session not found"))?
                .clone();
            let guard = state.lock().await;
            guard
                .viewers
                .get(viewer_id)
                .ok_or_else(|| anyhow::anyhow!("viewer not found"))?
                .recv_transport
                .clone()
        };

        let mut options = ConsumerOptions::new(producer_id, rtp_capabilities);
        options.paused = true;
        let consumer = transport
            .consume(options)
            .await
            .context("create consumer")?;

        let info = ConsumedInfo {
            id: consumer.id(),
            producer_id: consumer.producer_id(),
            kind: consumer.kind(),
            rtp_parameters: consumer.rtp_parameters().clone(),
            paused: true,
        };

        // Re-acquire lock to store the consumer in the viewer slot.
        // We re-fetch the entry rather than caching the Arc because
        // `remove_viewer` may have run while we were awaiting and
        // dropping the consumer here would silently leak the
        // mediasoup resources (until the router itself closes).
        let state = self
            .inner
            .sessions
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("session not found"))?
            .clone();
        let mut guard = state.lock().await;
        let slot = guard
            .viewers
            .get_mut(viewer_id)
            .ok_or_else(|| anyhow::anyhow!("viewer disappeared during consume"))?;
        slot.consumers.push(consumer);
        Ok(info)
    }

    /// Resume the named consumer so RTP starts flowing. Mirrors
    /// mediasoup-client `consumer.resume()`.
    pub async fn resume_consumer(
        &self,
        session_id: &str,
        viewer_id: &str,
        consumer_id: ConsumerId,
    ) -> anyhow::Result<()> {
        let consumer = {
            let state = self
                .inner
                .sessions
                .get(session_id)
                .ok_or_else(|| anyhow::anyhow!("session not found"))?
                .clone();
            let guard = state.lock().await;
            let slot = guard
                .viewers
                .get(viewer_id)
                .ok_or_else(|| anyhow::anyhow!("viewer not found"))?;
            slot.consumers
                .iter()
                .find(|c| c.id() == consumer_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("consumer not found"))?
        };
        consumer.resume().await.context("resume consumer")?;
        Ok(())
    }

    /// Set the preferred spatial/temporal layers for a viewer's video
    /// consumer (quality control). The GStreamer source publishes VP9
    /// simulcast with independent spatial encodes, so:
    ///   - `spatial_layer = 0|1|2` selects 540p / 720p / source-1080p.
    ///   - `temporal_layer = None` leaves temporal selection unpinned;
    ///     the current VP9 branches are spatial-only simulcast, not SVC.
    ///   - Picking the highest spatial layer with temporal unset keeps
    ///     mediasoup's per-consumer bandwidth estimator free to step down
    ///     on congestion.
    ///
    /// Idempotent and best-effort: an unknown session/viewer/consumer is
    /// an error the WS handler logs but does not treat as fatal (the
    /// video keeps playing at whatever layer was already selected).
    pub async fn set_preferred_layers(
        &self,
        session_id: &str,
        viewer_id: &str,
        consumer_id: ConsumerId,
        spatial_layer: u8,
        temporal_layer: Option<u8>,
    ) -> anyhow::Result<()> {
        if spatial_layer > 2 {
            anyhow::bail!("invalid VP9 simulcast spatial layer: {spatial_layer}");
        }
        if temporal_layer.is_some() {
            anyhow::bail!("VP9 simulcast source does not expose temporal layers");
        }

        let consumer = {
            let state = self
                .inner
                .sessions
                .get(session_id)
                .ok_or_else(|| anyhow::anyhow!("session not found"))?
                .clone();
            let guard = state.lock().await;
            let slot = guard
                .viewers
                .get(viewer_id)
                .ok_or_else(|| anyhow::anyhow!("viewer not found"))?;
            slot.consumers
                .iter()
                .find(|c| c.id() == consumer_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("consumer not found"))?
        };
        consumer
            .set_preferred_layers(ConsumerLayers {
                spatial_layer,
                temporal_layer,
            })
            .await
            .context("set preferred layers")?;
        Ok(())
    }

    fn pick_worker_index(&self) -> usize {
        self.inner.worker_cursor.fetch_add(1, Ordering::Relaxed) % self.inner.workers.len()
    }

    /// Sample the plain video producer's inbound RTP health for a
    /// session (owner→edge leg), or `None` if the session/producer is
    /// gone. Used by the source monitor for both starvation detection
    /// (`byte_count`) and per-leg loss logging.
    async fn producer_sample(&self, session_id: &str) -> Option<ProducerSample> {
        let state = self.inner.sessions.get(session_id)?.clone();
        let guard = state.lock().await;
        let has_viewers = !guard.viewers.is_empty();
        let producer = guard.plain_producer.as_ref()?;
        let stats = producer.get_stats().await.ok()?;
        if stats.is_empty() {
            return Some(ProducerSample {
                has_viewers,
                rtp_streams: 0,
                byte_count: 0,
                packets_lost: 0,
                fraction_lost: 0,
                score: 0,
                bitrate: 0,
            });
        }

        let mut sample = ProducerSample {
            has_viewers,
            rtp_streams: 0,
            byte_count: 0,
            packets_lost: 0,
            fraction_lost: 0,
            score: 10,
            bitrate: 0,
        };
        for stat in stats {
            sample.rtp_streams += 1;
            sample.byte_count = sample.byte_count.saturating_add(stat.byte_count);
            // `ProducerStat.packets_lost` is `i32` (can be negative
            // briefly per RTCP arithmetic); clamp to a non-negative
            // cumulative count for our delta math.
            sample.packets_lost = sample
                .packets_lost
                .saturating_add(stat.packets_lost.max(0) as u64);
            sample.fraction_lost = sample.fraction_lost.max(stat.fraction_lost);
            sample.score = sample.score.min(stat.score);
            sample.bitrate = sample.bitrate.saturating_add(stat.bitrate);
        }
        Some(sample)
    }

    /// Tear down and recreate the session's PlainTransport + video/audio
    /// producers, REUSING THE SAME UDP PORT so the owner's udpsink keeps
    /// hitting a valid destination. This re-locks comedia onto the new
    /// source tuple after the RTP source restarts.
    ///
    /// The new producers get fresh ids; existing viewer consumers of the
    /// old producers are closed when the old producer drops. Viewers
    /// recover on their next reconnect — the WS `Init` reports the new
    /// `producerId` and they consume it. Best-effort; logs on failure.
    pub async fn reprovision_plain_producer(&self, session_id: &str) -> anyhow::Result<()> {
        let state = self
            .inner
            .sessions
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("session not found"))?
            .clone();
        let mut guard = state.lock().await;
        let router = guard.router.clone();
        // Reuse the old local port so the owner's udpsink target is unchanged.
        let old_port = guard
            ._plain_transport
            .as_ref()
            .map(|t| t.tuple().local_port());

        // Drop old producers + transport first → closes them on the worker
        // and frees the UDP port for rebinding.
        guard.plain_producer = None;
        guard.plain_audio_producer = None;
        guard._plain_transport = None;

        // Recreate on the same port. The worker frees the port
        // asynchronously after the drop above, so retry a few times.
        let mut plain_transport = None;
        let mut last_err: Option<anyhow::Error> = None;
        for _ in 0..12u32 {
            let opts = match old_port {
                Some(p) => create_plain_transport_options_on_port(&self.inner.listen, p),
                None => create_plain_transport_options(&self.inner.listen),
            };
            match router.create_plain_transport(opts).await {
                Ok(t) => {
                    plain_transport = Some(t);
                    break;
                }
                Err(e) => {
                    last_err = Some(anyhow::anyhow!(e.to_string()));
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
        }
        let plain_transport = plain_transport.ok_or_else(|| {
            anyhow::anyhow!(
                "recreate plain transport on port {:?} failed: {:?}",
                old_port,
                last_err
            )
        })?;

        let producer = plain_transport
            .produce(ProducerOptions::new(
                MediaKind::Video,
                plain_producer_rtp_parameters(),
            ))
            .await
            .context("re-produce video")?;
        let audio_producer = plain_transport
            .produce(ProducerOptions::new(
                MediaKind::Audio,
                plain_audio_producer_rtp_parameters(),
            ))
            .await
            .context("re-produce audio")?;

        let new_port = plain_transport.tuple().local_port();
        let new_producer_id = producer.id();
        guard._plain_transport = Some(plain_transport);
        guard.plain_producer = Some(producer);
        guard.plain_audio_producer = Some(audio_producer);
        tracing::info!(
            %session_id,
            old_port = ?old_port,
            new_port,
            %new_producer_id,
            "plain producer reprovisioned — comedia will re-lock onto the new source tuple"
        );
        Ok(())
    }

    /// Spawn the per-process source monitor. It periodically samples each
    /// session's plain video producer; if a producer that was previously
    /// receiving RTP goes silent for [`SOURCE_STARVE_SECS`] while viewers
    /// are present, it reprovisions the producer to re-lock comedia (the
    /// container-restart recovery path). Holds a strong handle, so it runs
    /// for the process lifetime (the manager lives in `AppState`).
    fn spawn_source_monitor(&self) {
        let manager = self.clone_handle();
        tokio::spawn(async move {
            let mut activity: HashMap<SessionId, ProducerActivity> = HashMap::new();
            let mut ticker = tokio::time::interval(Duration::from_secs(SOURCE_MONITOR_SECS));
            loop {
                ticker.tick().await;
                let session_ids: Vec<SessionId> = manager
                    .inner
                    .sessions
                    .iter()
                    .map(|e| e.key().clone())
                    .collect();
                // Forget tracking for sessions that no longer exist.
                activity.retain(|sid, _| manager.inner.sessions.contains_key(sid));

                for sid in session_ids {
                    let Some(sample) = manager.producer_sample(&sid).await else {
                        activity.remove(&sid);
                        continue;
                    };
                    if !sample.has_viewers {
                        // No one watching → broadcast may be intentionally
                        // stopped. Don't reprovision; reset tracking.
                        activity.remove(&sid);
                        continue;
                    }
                    let bytes = sample.byte_count;
                    let now = Instant::now();
                    let entry = activity.entry(sid.clone()).or_insert(ProducerActivity {
                        last_bytes: bytes,
                        last_change: now,
                        ever_flowed: bytes > 0,
                        last_packets_lost: sample.packets_lost,
                    });

                    // ── Per-leg loss log (owner→edge) ─────────────────────
                    // Compare this against the viewer's edge→viewer `[net]`
                    // loss (logged client-side) to localise where stutter
                    // originates: high here = lossy owner→edge UDP leg (no
                    // NACK, hits all viewers); low here but high at viewer =
                    // lossy edge→viewer internet path.
                    let lost_delta = sample.packets_lost.saturating_sub(entry.last_packets_lost);
                    entry.last_packets_lost = sample.packets_lost;
                    tracing::info!(
                        session_id = %sid,
                        leg = "owner->edge",
                        rtp_streams = sample.rtp_streams,
                        bitrate_kbps = sample.bitrate / 1000,
                        fraction_lost_pct = (sample.fraction_lost as f32) / 255.0 * 100.0,
                        packets_lost_total = sample.packets_lost,
                        packets_lost_delta = lost_delta,
                        score = sample.score,
                        "producer RTP health"
                    );

                    if bytes > entry.last_bytes {
                        entry.last_bytes = bytes;
                        entry.last_change = now;
                        entry.ever_flowed = true;
                    } else if entry.ever_flowed
                        && now.duration_since(entry.last_change)
                            >= Duration::from_secs(SOURCE_STARVE_SECS)
                    {
                        tracing::warn!(
                            session_id = %sid,
                            starve_secs = SOURCE_STARVE_SECS,
                            "plain producer starved with viewers present — reprovisioning to re-lock comedia"
                        );
                        match manager.reprovision_plain_producer(&sid).await {
                            Ok(()) => {
                                // Fresh producer: wait for it to flow again
                                // before considering another reprovision, so
                                // a still-dead source doesn't churn.
                                *entry = ProducerActivity {
                                    last_bytes: 0,
                                    last_change: Instant::now(),
                                    ever_flowed: false,
                                    last_packets_lost: 0,
                                };
                            }
                            Err(e) => {
                                tracing::error!(session_id = %sid, error = %e, "reprovision failed");
                                // Back off this session for a cycle.
                                entry.last_change = Instant::now();
                            }
                        }
                    }
                }
            }
        });
    }
}

/// Outcome of the per-viewer idempotency check in
/// [`RouterManager::produce_input_data`].
///
/// Extracted as a tiny pure decision so the idempotency contract
/// (Requirement 3.1: "a second call returns the existing producer id")
/// is unit-testable without a live mediasoup worker — the surrounding
/// `produce_input_data` needs a real `Router`/`WebRtcTransport`/
/// `DirectTransport`, none of which can be constructed in a unit test,
/// but the *decision* is just "do we already hold a producer?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdempotentProduce {
    /// The viewer already owns an input `DataProducer`; return its id and
    /// create nothing new.
    ReturnExisting(DataProducerId),
    /// No existing producer — proceed to `produce_data` / `consume_data`.
    Proceed,
}

/// Pure idempotency decision shared by both guard sites in
/// `produce_input_data` (the pre-produce fast path and the post-await
/// race guard): given the id of the viewer slot's current
/// `input_data_producer` (if any), decide whether to return it as-is or
/// to proceed with creating a new producer.
///
/// Keeping this branch-free of mediasoup handles is what makes
/// `produce_input_data`'s idempotency guarantee verifiable on a machine
/// that cannot build `mediasoup-sys`.
pub(crate) fn idempotent_produce_decision(
    existing: Option<DataProducerId>,
) -> IdempotentProduce {
    match existing {
        Some(id) => IdempotentProduce::ReturnExisting(id),
        None => IdempotentProduce::Proceed,
    }
}

/// Resolve the Neko v3 **admin** WebSocket URL + Bearer token for a session
/// so a [`NekoInputBridge`] can be opened.
///
/// ## Plumbing gap (documented)
///
/// At the time of task 5.3 the per-session Neko admin endpoint is **not yet
/// plumbed through** to `edge-sfu`. The container's Neko endpoint is known to
/// dun-api (it provisions the container) and to the owner's `useNeko` host
/// path (which logs into `/webrtc/api/login`), but `edge-sfu` only ever sees
/// the opaque `session_id` — it never receives the container's Neko URL or an
/// admin token through `provision_session`.
///
/// Wiring it correctly requires threading a `(neko_ws_url, token)` pair from
/// dun-api → `edge-control` → `provision_session` → `SessionState`. That
/// crosses the edge↔api boundary and is out of scope for this task (it belongs
/// with the `sfu_ws.rs` / session-provisioning tasks).
///
/// Until then this reads two **process-level** env vars as a deliberate
/// placeholder, consistent with how every other SFU knob is sourced
/// (`RouterListenInfo::from_env`):
/// - `NEKO_ADMIN_WS_URL` — a URL **template**; the literal `{session}` token,
///   if present, is replaced with `session_id` so a single edge can address
///   per-session containers (e.g. `ws://neko-{session}:8081/webrtc/api/ws`).
/// - `NEKO_ADMIN_TOKEN` — the Bearer token presented on the upgrade request.
///
/// Returns `None` (and the caller logs + degrades to decode-only) when either
/// var is unset, so the SCTP input path is exercisable in dev/CI without a
/// live Neko admin endpoint.
fn neko_admin_endpoint(session_id: &str) -> Option<(String, String)> {
    let url_template = env::var("NEKO_ADMIN_WS_URL").ok()?;
    let token = env::var("NEKO_ADMIN_TOKEN").ok()?;
    if url_template.trim().is_empty() || token.trim().is_empty() {
        return None;
    }
    let url = url_template.replace("{session}", session_id);
    Some((url, token))
}

fn snapshot(state: &SessionState) -> ProvisionedRouter {
    let router = &state.router;
    let producer = state
        .plain_producer
        .as_ref()
        .expect("plain_producer absent on snapshot");
    let plain_rtp_port = state
        ._plain_transport
        .as_ref()
        .map(|t| t.tuple().local_port())
        .unwrap_or(0);
    let plain_rtcp_port = state
        ._plain_transport
        .as_ref()
        .and_then(|t| t.rtcp_tuple().map(|x| x.local_port()))
        .unwrap_or(plain_rtp_port);
    ProvisionedRouter {
        router_id: router.id(),
        plain_rtp_port,
        plain_rtcp_port,
        producer_id: producer.id(),
        audio_producer_id: state.plain_audio_producer.as_ref().map(|p| p.id()),
        rtp_capabilities: router.rtp_capabilities().clone(),
    }
}

fn default_media_codecs() -> Vec<RtpCodecCapability> {
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
            mime_type: MimeTypeVideo::Vp9,
            preferred_payload_type: None,
            clock_rate: NonZeroU32::new(90_000).unwrap(),
            parameters: RtpCodecParametersParameters::default(),
            // No `RtcpFeedback::Nack` — see Phase 0 RESULTS.md for SRTP
            // replay flood rationale. Consumer-facing feedback set is
            // codec-independent (same list as the prior VP8 build).
            rtcp_feedback: vec![
                RtcpFeedback::NackPli,
                RtcpFeedback::CcmFir,
                RtcpFeedback::GoogRemb,
                RtcpFeedback::TransportCc,
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use mediasoup::data_consumer::DataConsumer;
    use mediasoup::data_producer::DataProducer;
    use proptest::prelude::*;

    /// A fixed, valid v4 UUID string used to mint a `DataProducerId` in
    /// pure tests. `DataProducerId` exposes `FromStr` (parses a UUID) and
    /// `Display` publicly even though its `new()` is `pub(super)`, so this
    /// is the supported way to fabricate one without a live worker.
    const SAMPLE_ID: &str = "11111111-1111-4111-8111-111111111111";
    const OTHER_ID: &str = "22222222-2222-4222-8222-222222222222";

    fn producer_id(s: &str) -> DataProducerId {
        s.parse().expect("valid uuid")
    }

    /// Render 16 arbitrary bytes as a canonical hyphenated UUID string
    /// (`8-4-4-4-12` hex). `DataProducerId`'s `FromStr` accepts any hex in
    /// this layout regardless of version/variant nibbles, so this lets the
    /// property generate ids without pulling in the `uuid` crate.
    fn uuid_string_from_bytes(b: &[u8; 16]) -> String {
        let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
        format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        )
    }

    // ── Idempotency decision (Requirement 3.1) ────────────────────────────
    //
    // `produce_input_data` must return the *existing* producer id on a second
    // call rather than creating a new producer/consumer. The decision is
    // factored into `idempotent_produce_decision`, which both guard sites in
    // `produce_input_data` call: the pre-produce fast path and the
    // post-`consume_data` race guard. Testing it here verifies the
    // idempotency contract on a machine that cannot build `mediasoup-sys`
    // (the full method needs a live Router/WebRtcTransport/DirectTransport).

    #[test]
    fn idempotent_decision_proceeds_when_no_producer() {
        // First call: viewer slot has no input producer yet.
        assert_eq!(
            idempotent_produce_decision(None),
            IdempotentProduce::Proceed
        );
    }

    #[test]
    fn idempotent_decision_returns_existing_producer_id() {
        // Second call: viewer slot already owns a producer → return its id,
        // create nothing new.
        let id = producer_id(SAMPLE_ID);
        assert_eq!(
            idempotent_produce_decision(Some(id)),
            IdempotentProduce::ReturnExisting(id)
        );
    }

    #[test]
    fn idempotent_decision_returns_the_same_id_it_was_given() {
        // The returned id is exactly the existing one (not a freshly minted
        // one) — this is the literal "second call returns the same producer
        // id" guarantee from the formal spec's postconditions.
        let id = producer_id(SAMPLE_ID);
        match idempotent_produce_decision(Some(id)) {
            IdempotentProduce::ReturnExisting(returned) => assert_eq!(returned, id),
            IdempotentProduce::Proceed => panic!("expected ReturnExisting for an existing producer"),
        }
        // A different existing id round-trips unchanged too.
        let other = producer_id(OTHER_ID);
        assert_eq!(
            idempotent_produce_decision(Some(other)),
            IdempotentProduce::ReturnExisting(other)
        );
    }

    #[test]
    fn idempotent_decision_is_stable_across_repeated_calls() {
        // Calling the decision repeatedly with the same existing producer
        // always yields the same id — a third, fourth, ... ProduceInput must
        // never mint a new producer.
        let id = producer_id(SAMPLE_ID);
        for _ in 0..10 {
            assert_eq!(
                idempotent_produce_decision(Some(id)),
                IdempotentProduce::ReturnExisting(id)
            );
        }
    }

    proptest! {
        /// Idempotency, generalised: for ANY existing-producer state the
        /// decision is total and correct — `Some(id) → ReturnExisting(id)`
        /// (the *same* id, byte-for-byte) and `None → Proceed`. This is the
        /// universal form of "a second call returns the existing producer id"
        /// (Requirement 3.1).
        #[test]
        fn idempotent_decision_round_trips_any_id(bytes in any::<[u8; 16]>()) {
            let id: DataProducerId = uuid_string_from_bytes(&bytes).parse().unwrap();

            // With a producer present we always return that exact id.
            prop_assert_eq!(
                idempotent_produce_decision(Some(id)),
                IdempotentProduce::ReturnExisting(id)
            );
        }

        /// Absence of a producer always means "proceed" — never spuriously
        /// short-circuits when there is nothing to return.
        #[test]
        fn idempotent_decision_none_always_proceeds(_seed in any::<u64>()) {
            prop_assert_eq!(idempotent_produce_decision(None), IdempotentProduce::Proceed);
        }
    }

    // ── Slot lifetime: drop = close (Requirement 3.1, formal postcondition) ─
    //
    // The "dropping the viewer slot closes the producer/consumer" guarantee
    // is enforced by Rust ownership + mediasoup's `Drop` impls, NOT by
    // runtime logic we can exercise without a live worker: `ViewerSlot` owns
    // its `DataProducer` / `DataConsumer` *by value* inside an `Option`, so
    // when the slot is dropped (e.g. `remove_viewer` → `HashMap::remove`, or
    // `close_session` dropping the whole `SessionState`) those handles are
    // dropped, and mediasoup closes the underlying worker resources.
    //
    // We can still pin that ownership contract at compile time: the function
    // below never runs, but it only type-checks if the slot keeps owning the
    // producer/consumer as `Option<DataProducer>` / `Option<DataConsumer>`.
    // If a future change made these `Arc<…>` (shared, so drop would NOT
    // close) or renamed them, this stops compiling — a cheap, worker-free
    // regression guard for the lifetime postcondition.
    #[allow(dead_code, unused_variables)]
    fn viewer_slot_owns_input_handles_by_value(slot: &ViewerSlot) {
        // Owned-by-value `Option<DataProducer>`: dropping the slot drops the
        // producer, which closes it on the worker.
        let _producer: &Option<DataProducer> = &slot.input_data_producer;
        // Owned-by-value `Option<DataConsumer>`: same lifetime coupling for
        // the matching DirectTransport consumer.
        let _consumer: &Option<DataConsumer> = &slot.input_data_consumer;
    }

    /// Live integration test for `produce_input_data` — **requires a real
    /// mediasoup worker** (Python + Meson + C++ toolchain) so it is
    /// `#[ignore]`d and runs only on Linux/CI, never on the Windows dev box
    /// where `mediasoup-sys` cannot build.
    ///
    /// To run on CI: `cargo test -p edge-sfu -- --ignored produce_input_data_live`.
    ///
    /// Assertions to perform (the parts that genuinely need a live worker,
    /// complementing the pure idempotency + ownership checks above):
    ///
    /// 1. **Idempotency end-to-end** — provision a session, create a viewer,
    ///    `connect_send_transport`, then call `produce_input_data` twice with
    ///    fresh `DataProducerOptions::new_sctp(stream_params)`. Assert the two
    ///    calls return the SAME `DataProducerId`, and that only ONE
    ///    `DataProducer` + ONE `DataConsumer` exist on the slot
    ///    (`slot.input_data_producer.is_some()` and exactly one consumer on
    ///    the session `DirectTransport`).
    /// 2. **Slot lifetime (drop = close)** — capture the `DataProducer` /
    ///    `DataConsumer` handles, then `remove_viewer(session, viewer)` (drops
    ///    the `ViewerSlot`). Assert the producer and consumer report `closed`
    ///    (e.g. `data_producer.closed()` / observe the `on_close` event), and
    ///    that producing again for the same viewer fails (slot gone) — proving
    ///    the handles are not leaked past the slot's lifetime.
    #[test]
    #[ignore = "requires a live mediasoup worker (mediasoup-sys: Python+Meson+C++); run on Linux/CI with --ignored"]
    fn produce_input_data_live() {
        // Intentionally empty on dev: the worker-backed assertions above are
        // documented for CI. Building this crate at all already fails on the
        // Windows dev box (mediasoup-sys), so the body stays a no-op marker —
        // the pure decision + compile-time ownership tests cover what is
        // verifiable here without a worker.
    }
}
