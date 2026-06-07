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

use crate::transport::{
    create_consumer_transport_options, create_plain_transport_options,
    plain_audio_producer_rtp_parameters, plain_producer_rtp_parameters, RouterListenInfo,
};
use crate::VIEWER_CAP_PER_SESSION;
use anyhow::Context;
use dashmap::DashMap;
use edge_shared::types::SessionId;
use mediasoup::consumer::ConsumerId;
use mediasoup::prelude::*;
use mediasoup::producer::ProducerId;
use mediasoup::router::RouterId;
use mediasoup::transport::TransportId;
use mediasoup::worker::WorkerSettings;
use std::collections::HashMap;
use std::num::{NonZeroU32, NonZeroU8};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

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
        for _ in 0..workers {
            let mut settings = WorkerSettings::default();
            settings.log_level = mediasoup::worker::WorkerLogLevel::Warn;
            let worker = manager
                .create_worker(settings)
                .await
                .context("create mediasoup worker")?;
            pool.push(worker);
        }
        Ok(Self {
            inner: Arc::new(RouterManagerInner {
                workers: pool,
                worker_cursor: AtomicUsize::new(0),
                sessions: DashMap::new(),
                listen,
            }),
        })
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

        let worker = self.pick_worker();
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
        // pipeline funnels both VP8 (pt=96 ssrc=22222222) and Opus
        // (pt=111 ssrc=22222223) RTP into the one UDP port; mediasoup
        // demuxes by SSRC. If the owner's pipeline has no audio branch
        // (older dun-app), no Opus packets ever arrive and this
        // producer simply stays silent — harmless.
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
            _cumulative_bytes_cache: Mutex::new(0),
        };
        self.inner
            .sessions
            .insert(session_id.to_string(), Arc::new(Mutex::new(state)));

        tracing::info!(
            %session_id,
            router_id = %provisioned.router_id,
            plain_rtp_port,
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

        let recv_options = create_consumer_transport_options(&self.inner.listen);
        let send_options = create_consumer_transport_options(&self.inner.listen);
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

    fn pick_worker(&self) -> &Worker {
        let idx = self.inner.worker_cursor.fetch_add(1, Ordering::Relaxed)
            % self.inner.workers.len();
        &self.inner.workers[idx]
    }
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
            mime_type: MimeTypeVideo::Vp8,
            preferred_payload_type: None,
            clock_rate: NonZeroU32::new(90_000).unwrap(),
            parameters: RtpCodecParametersParameters::default(),
            // No `RtcpFeedback::Nack` — see Phase 0 RESULTS.md for SRTP
            // replay flood rationale.
            rtcp_feedback: vec![
                RtcpFeedback::NackPli,
                RtcpFeedback::CcmFir,
                RtcpFeedback::GoogRemb,
                RtcpFeedback::TransportCc,
            ],
        },
    ]
}
