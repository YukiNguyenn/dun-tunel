//! Transport setup helpers for Producer + Consumer (R8.2, R8.3).
//!
//! These functions are pure config — no async work — so the
//! `RouterManager` can call them inside locks without worrying about
//! re-entrancy. Listen IPs + port ranges come from env via
//! `RouterListenInfo::from_env`.

use mediasoup::prelude::*;
use mediasoup_types::data_structures::Protocol;
use std::env;
use std::net::IpAddr;
use std::num::{NonZeroU32, NonZeroU8};

const DEFAULT_RTC_MIN_PORT: u16 = 50_000;
const DEFAULT_RTC_MAX_PORT: u16 = 60_000;

/// PlainTransport per-session UDP port pool, **shared across the whole
/// Edge_Server** (not per-user, not per-session).
///
/// Each ShareSession provisions its own PlainTransport for the Neko →
/// SFU GStreamer pipeline. Mediasoup binds an exclusive UDP port per
/// transport so we MUST give it a range to pick from — using a single
/// fixed port causes the second concurrent session to fail with
/// `uv_udp_bind() failed: address already in use` (observed on
/// 2026-06-04 after smoke #1 left a lingering bind).
///
/// Capacity sizing:
///   pool_size = max concurrent share sessions on this Edge_Server
///
/// 5000 ports comfortably covers ~165 Enterprise users (30 sessions
/// each), ~1000 Pro users (2 each), or any realistic mix until the
/// real bottleneck (CPU / bandwidth) bites. Range deliberately ≥ 4×
/// the WebRTC-Consumer range (50000-60000) to make the two pools
/// trivial to grow independently.
///
/// Operators tuning a constrained VPS can shrink via env vars
/// `SFU_PLAIN_RTP_MIN_PORT` / `SFU_PLAIN_RTP_MAX_PORT`.
const DEFAULT_PLAIN_RTP_MIN_PORT: u16 = 5_000;
const DEFAULT_PLAIN_RTP_MAX_PORT: u16 = 9_999;

/// RTP payload type the PlainTransport Producer expects from Neko's
/// `rtpvp8pay pt=96`. Exposed so edge-control can echo it back to dun-api
/// → dun-app → the owner's GStreamer pipeline, keeping a single source of
/// truth for the VP8 wire shape.
pub const PLAIN_PAYLOAD_TYPE: u8 = 96;
const PLAIN_CLOCK_RATE: u32 = 90_000;
/// Fixed RTP SSRC the Producer is bound to (Neko `udpsink ssrc=22222222`).
pub const PLAIN_SSRC: u32 = 22_222_222;

/// RTP payload type for the Opus audio producer fed by the GStreamer
/// `rtpopuspay pt=111` branch. Audio + video share ONE PlainTransport
/// (one UDP port, comedia mode) and are demultiplexed by SSRC + payload
/// type — so the audio PT/SSRC MUST differ from the video ones above.
pub const PLAIN_AUDIO_PAYLOAD_TYPE: u8 = 111;
const PLAIN_AUDIO_CLOCK_RATE: u32 = 48_000;
/// Fixed RTP SSRC for the audio producer (GStreamer
/// `rtpopuspay ... ssrc=22222223`). One more than the video SSRC so the
/// two streams never collide on the shared transport.
pub const PLAIN_AUDIO_SSRC: u32 = 22_222_223;

#[derive(Debug, Clone, Copy)]
pub struct RouterListenInfo {
    pub listen_ip: IpAddr,
    pub announced_ip: IpAddr,
    /// Optional second announced address advertised as an additional ICE
    /// candidate on viewer WebRtcTransports. Set to the edge VPS's
    /// LAN-private IP (e.g. 192.168.20.6) so a viewer on the SAME LAN can
    /// reach the SFU directly instead of hairpinning off the public
    /// `announced_ip`. Unset (None) in production — public IP only.
    pub lan_announced_ip: Option<IpAddr>,
    pub rtc_min_port: u16,
    pub rtc_max_port: u16,
    /// Lower bound of the per-session PlainTransport UDP port range.
    /// Mediasoup picks an unused port from `[plain_min..=plain_max]`
    /// per `create_plain_transport`.
    pub plain_rtp_min_port: u16,
    pub plain_rtp_max_port: u16,
}

impl RouterListenInfo {
    /// Pull listen config from env. Defaults match the Phase 0 PoC.
    pub fn from_env() -> anyhow::Result<Self> {
        let listen_ip = env_ip("SFU_LISTEN_IP", "0.0.0.0")?;
        let announced_ip = env_ip("SFU_ANNOUNCED_IP", "127.0.0.1")?;
        // Optional LAN candidate for same-network (hairpin) testing.
        // Absent/empty → None (production default).
        let lan_announced_ip = match std::env::var("SFU_ANNOUNCED_IP_LAN") {
            Ok(s) if !s.trim().is_empty() => Some(
                s.trim()
                    .parse::<IpAddr>()
                    .map_err(|e| anyhow::anyhow!("invalid SFU_ANNOUNCED_IP_LAN '{s}': {e}"))?,
            ),
            _ => None,
        };
        let rtc_min_port = env_u16("SFU_RTC_MIN_PORT", DEFAULT_RTC_MIN_PORT)?;
        let rtc_max_port = env_u16("SFU_RTC_MAX_PORT", DEFAULT_RTC_MAX_PORT)?;
        let plain_rtp_min_port = env_u16("SFU_PLAIN_RTP_MIN_PORT", DEFAULT_PLAIN_RTP_MIN_PORT)?;
        let plain_rtp_max_port = env_u16("SFU_PLAIN_RTP_MAX_PORT", DEFAULT_PLAIN_RTP_MAX_PORT)?;
        if rtc_min_port >= rtc_max_port {
            anyhow::bail!(
                "SFU_RTC_MIN_PORT ({rtc_min_port}) must be < SFU_RTC_MAX_PORT ({rtc_max_port})"
            );
        }
        if plain_rtp_min_port >= plain_rtp_max_port {
            anyhow::bail!(
                "SFU_PLAIN_RTP_MIN_PORT ({plain_rtp_min_port}) must be < SFU_PLAIN_RTP_MAX_PORT ({plain_rtp_max_port})"
            );
        }
        Ok(Self {
            listen_ip,
            announced_ip,
            lan_announced_ip,
            rtc_min_port,
            rtc_max_port,
            plain_rtp_min_port,
            plain_rtp_max_port,
        })
    }
}

/// Build PlainTransport options for the Neko-fed RTP source. Comedia mode
/// auto-detects the remote source from the first packet so we don't have
/// to plumb gstreamer's ephemeral source port through dun-api.
pub fn create_plain_transport_options(listen: &RouterListenInfo) -> PlainTransportOptions {
    let mut opts = PlainTransportOptions::new(ListenInfo {
        protocol: Protocol::Udp,
        ip: listen.listen_ip,
        announced_address: Some(listen.announced_ip.to_string()),
        expose_internal_ip: false,
        // `port: None` + `port_range`: mediasoup picks a free UDP port
        // per session. Required for ≥ 2 concurrent sessions; previously
        // we hard-coded 5004 and the second session hit `EADDRINUSE`.
        port: None,
        port_range: Some(listen.plain_rtp_min_port..=listen.plain_rtp_max_port),
        flags: None,
        send_buffer_size: None,
        recv_buffer_size: None,
    });
    opts.comedia = true;
    opts
}

/// Like [`create_plain_transport_options`] but pins the UDP port to
/// `port` instead of picking from the range. Used when RE-creating a
/// session's PlainTransport after the RTP source restarts: comedia locks
/// onto the first packet's source tuple and then drops packets from any
/// other tuple (mediasoup `PlainTransport.cpp`: "ignoring RTP packet from
/// unknown IP:port"), so a container restart (new Docker SNAT source
/// port) starves the producer forever. Recreating the transport re-locks
/// comedia onto the new source — and reusing the SAME local port means
/// the owner's udpsink target stays valid (no owner-side change needed).
pub fn create_plain_transport_options_on_port(
    listen: &RouterListenInfo,
    port: u16,
) -> PlainTransportOptions {
    let mut opts = PlainTransportOptions::new(ListenInfo {
        protocol: Protocol::Udp,
        ip: listen.listen_ip,
        announced_address: Some(listen.announced_ip.to_string()),
        expose_internal_ip: false,
        port: Some(port),
        port_range: None,
        flags: None,
        send_buffer_size: None,
        recv_buffer_size: None,
    });
    opts.comedia = true;
    opts
}
/// Offset added to a worker's public mux port to derive its LAN hairpin
/// mux port when `SFU_ANNOUNCED_IP_LAN` is configured. mediasoup binds
/// one socket per listen info, so the public and LAN candidates MUST sit
/// on distinct UDP ports (the same `ip:port` twice is `EADDRINUSE`).
/// 1000 comfortably exceeds any realistic worker count, so per-worker
/// public ports (`rtc_min + idx`) and LAN ports (`rtc_min + idx + 1000`)
/// never overlap.
const LAN_MUX_PORT_OFFSET: u16 = 1000;

/// Build the per-worker [`WebRtcServer`] options bound to a SINGLE UDP
/// port (`port`). Every viewer `WebRtcTransport` created against this
/// server multiplexes onto that one port (mediasoup is ICE-Lite and
/// demuxes peers by ICE ufrag), so the edge firewall only needs ONE UDP
/// port per worker open instead of the whole `rtc_min..rtc_max` range —
/// the symmetric counterpart to Neko's `NEKO_WEBRTC_UDPMUX` on the owner
/// side.
///
/// Same-LAN hairpin: when `SFU_ANNOUNCED_IP_LAN` is set (the edge VPS and
/// the viewer share one NAT / public IP — typical self-host / dev), we
/// add a SECOND listen info announcing the LAN-private address on a
/// distinct mux port (`port + LAN_MUX_PORT_OFFSET`). mediasoup then emits
/// both ICE candidates per viewer transport: a remote viewer uses the
/// public `announced_ip`, while a same-LAN viewer — whose router usually
/// can't NAT-loopback to the edge's own public IP — picks the private
/// candidate and connects directly. Costs one extra UDP port per worker,
/// and ONLY when the env var is set (empty in production → single port).
pub fn create_webrtc_server_options(listen: &RouterListenInfo, port: u16) -> WebRtcServerOptions {
    let mut infos = WebRtcServerListenInfos::new(ListenInfo {
        protocol: Protocol::Udp,
        ip: listen.listen_ip,
        announced_address: Some(listen.announced_ip.to_string()),
        expose_internal_ip: false,
        port: Some(port),
        port_range: None,
        flags: None,
        send_buffer_size: None,
        recv_buffer_size: None,
    });
    if let Some(lan_ip) = listen.lan_announced_ip {
        infos = infos.insert(ListenInfo {
            protocol: Protocol::Udp,
            ip: listen.listen_ip,
            announced_address: Some(lan_ip.to_string()),
            expose_internal_ip: false,
            port: Some(port.saturating_add(LAN_MUX_PORT_OFFSET)),
            port_range: None,
            flags: None,
            send_buffer_size: None,
            recv_buffer_size: None,
        });
    }
    WebRtcServerOptions::new(infos)
}

/// Build viewer transport options bound to a shared [`WebRtcServer`]
/// (single UDP mux port) instead of a per-transport port range. SCTP is
/// enabled so the viewer can author the `neko-input` DataChannel.
pub fn create_consumer_transport_options_with_server(
    server: WebRtcServer,
) -> WebRtcTransportOptions {
    let mut opts = WebRtcTransportOptions::new_with_server(server);
    opts.enable_sctp = true;
    opts.max_send_message_size = 262_144;
    opts
}

/// RTP parameters of the producer fed by the gstreamer pipeline. Must
/// match Neko's `rtpvp8pay pt=96` exactly — payload type, clock rate,
/// fixed SSRC.
///
/// Codec contract (Data Model M2, single source of truth): VP8,
/// `pt=96`, `ssrc=22222222`, `clockRate=90000`. The producer carries an
/// EMPTY `rtcp_feedback` list — it MUST NEVER include
/// `RtcpFeedback::Nack`. PlainTransport has no retransmit cache, so a
/// negotiated Nack triggers an SRTP replay flood ("index too old") when
/// the GStreamer pipeline resets sequence numbers on Neko reconnect (see
/// `poc/neko-sfu/RESULTS.md`). The viewer-facing feedback (NackPli,
/// CcmFir, GoogRemb, TransportCc) is declared on the router capability
/// in `router_manager.rs`, not here on the PlainTransport producer.
pub fn plain_producer_rtp_parameters() -> RtpParameters {
    RtpParameters {
        mid: None,
        codecs: vec![RtpCodecParameters::Video {
            mime_type: MimeTypeVideo::Vp8,
            payload_type: PLAIN_PAYLOAD_TYPE,
            clock_rate: NonZeroU32::new(PLAIN_CLOCK_RATE).unwrap(),
            parameters: RtpCodecParametersParameters::default(),
            // No `RtcpFeedback::Nack` — Data Model M2 / RESULTS.md.
            rtcp_feedback: vec![],
        }],
        header_extensions: vec![],
        encodings: vec![RtpEncodingParameters {
            ssrc: Some(PLAIN_SSRC),
            // NOTE: a `scalabilityMode = "L1T3"` was declared here to
            // pair with a temporal-scalability GStreamer pipeline, but
            // that pipeline was reverted (the image's vp8enc rejected the
            // value-array props → Neko HTTP 500 / no video). With a flat
            // single-layer source, declaring L1T3 here would lie to
            // mediasoup about layers that don't exist. Leave it at the
            // default (L1T1). Re-add ONLY together with a verified
            // temporal-scalability pipeline in `container_service.rs`.
            ..RtpEncodingParameters::default()
        }],
        rtcp: RtcpParameters::default(),
        msid: None,
    }
}

/// RTP parameters of the Opus audio producer fed by the GStreamer
/// `rtpopuspay pt=111 ssrc=22222223` branch on the SAME PlainTransport.
/// Stereo, 48 kHz, in-band FEC — mirrors Neko's own opus encode so the
/// viewer hears the same audio as the host.
///
/// Codec contract (Data Model M2, single source of truth): Opus,
/// `pt=111`, `ssrc=22222223`, `clockRate=48000`. Like the video
/// producer, the `rtcp_feedback` list is EMPTY and MUST NEVER include
/// `RtcpFeedback::Nack` on this PlainTransport-backed producer. The
/// viewer-facing audio feedback (TransportCc) is declared on the router
/// capability in `router_manager.rs`.
pub fn plain_audio_producer_rtp_parameters() -> RtpParameters {
    RtpParameters {
        mid: None,
        codecs: vec![RtpCodecParameters::Audio {
            mime_type: MimeTypeAudio::Opus,
            payload_type: PLAIN_AUDIO_PAYLOAD_TYPE,
            clock_rate: NonZeroU32::new(PLAIN_AUDIO_CLOCK_RATE).unwrap(),
            channels: NonZeroU8::new(2).unwrap(),
            parameters: RtpCodecParametersParameters::from([("useinbandfec", 1_u32.into())]),
            // No `RtcpFeedback::Nack` — Data Model M2 / RESULTS.md.
            rtcp_feedback: vec![],
        }],
        header_extensions: vec![],
        encodings: vec![RtpEncodingParameters {
            ssrc: Some(PLAIN_AUDIO_SSRC),
            ..RtpEncodingParameters::default()
        }],
        rtcp: RtcpParameters::default(),
        msid: None,
    }
}

fn env_ip(name: &str, default: &str) -> anyhow::Result<IpAddr> {
    env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .parse()
        .map_err(|e| anyhow::anyhow!("{name}: invalid IP — {e}"))
}

fn env_u16(name: &str, default: u16) -> anyhow::Result<u16> {
    env::var(name)
        .ok()
        .map(|s| {
            s.parse::<u16>()
                .map_err(|e| anyhow::anyhow!("{name}: invalid port — {e}"))
        })
        .transpose()
        .map(|opt| opt.unwrap_or(default))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Pull the payload type, clock rate, SSRC and rtcp_feedback out of a
    /// single-codec producer `RtpParameters`. Returns `None` if the shape
    /// ever deviates from the locked "one codec + one encoding" contract so
    /// the property fails loudly instead of silently skipping the assert.
    fn dissect(
        params: &RtpParameters,
    ) -> Option<(u8, u32, Option<u32>, Vec<RtcpFeedback>)> {
        // Data Model M2: exactly one codec and one encoding on the producer.
        if params.codecs.len() != 1 || params.encodings.len() != 1 {
            return None;
        }
        let ssrc = params.encodings[0].ssrc;
        match &params.codecs[0] {
            RtpCodecParameters::Video {
                payload_type,
                clock_rate,
                rtcp_feedback,
                ..
            }
            | RtpCodecParameters::Audio {
                payload_type,
                clock_rate,
                rtcp_feedback,
                ..
            } => Some((
                *payload_type,
                clock_rate.get(),
                ssrc,
                rtcp_feedback.clone(),
            )),
        }
    }

    proptest! {
        /// Property 5: Codec lock-step (video).
        ///
        /// However many times we rebuild the params, the video producer
        /// always carries `pt=96 ssrc=22222222 clockRate=90000` and an
        /// `rtcp_feedback` list that never contains `RtcpFeedback::Nack`
        /// (Data Model M2 — empty list on the PlainTransport producer).
        ///
        /// **Validates: Requirements 2.1, 2.2**
        #[test]
        fn video_producer_holds_codec_lock_step(_seed in any::<u64>()) {
            let params = plain_producer_rtp_parameters();
            let (pt, clock_rate, ssrc, rtcp_feedback) =
                dissect(&params).expect("video producer must be single codec + single encoding");

            prop_assert_eq!(pt, PLAIN_PAYLOAD_TYPE);
            prop_assert_eq!(pt, 96);
            prop_assert_eq!(clock_rate, PLAIN_CLOCK_RATE);
            prop_assert_eq!(clock_rate, 90_000);
            prop_assert_eq!(ssrc, Some(PLAIN_SSRC));
            prop_assert_eq!(ssrc, Some(22_222_222));
            // Never any Nack on the PlainTransport-backed producer.
            prop_assert!(!rtcp_feedback.contains(&RtcpFeedback::Nack));
            // M2: the producer feedback list is empty entirely.
            prop_assert!(rtcp_feedback.is_empty());
        }

        /// Property 5: Codec lock-step (audio).
        ///
        /// The Opus producer always carries `pt=111 ssrc=22222223
        /// clockRate=48000` and never negotiates `RtcpFeedback::Nack`.
        ///
        /// **Validates: Requirements 2.1, 2.2**
        #[test]
        fn audio_producer_holds_codec_lock_step(_seed in any::<u64>()) {
            let params = plain_audio_producer_rtp_parameters();
            let (pt, clock_rate, ssrc, rtcp_feedback) =
                dissect(&params).expect("audio producer must be single codec + single encoding");

            prop_assert_eq!(pt, PLAIN_AUDIO_PAYLOAD_TYPE);
            prop_assert_eq!(pt, 111);
            prop_assert_eq!(clock_rate, PLAIN_AUDIO_CLOCK_RATE);
            prop_assert_eq!(clock_rate, 48_000);
            prop_assert_eq!(ssrc, Some(PLAIN_AUDIO_SSRC));
            prop_assert_eq!(ssrc, Some(22_222_223));
            prop_assert!(!rtcp_feedback.contains(&RtcpFeedback::Nack));
            prop_assert!(rtcp_feedback.is_empty());
        }
    }

    /// Deterministic companion to the proptest: a plain assertion that the
    /// two builders satisfy the M2 contract exactly once, so a regression is
    /// obvious in a normal `cargo test` run even if proptest shrinking is
    /// noisy.
    #[test]
    fn builders_match_data_model_m2() {
        let (vpt, vclock, vssrc, vfb) =
            dissect(&plain_producer_rtp_parameters()).expect("video shape");
        assert_eq!((vpt, vclock, vssrc), (96, 90_000, Some(22_222_222)));
        assert!(vfb.is_empty());

        let (apt, aclock, assrc, afb) =
            dissect(&plain_audio_producer_rtp_parameters()).expect("audio shape");
        assert_eq!((apt, aclock, assrc), (111, 48_000, Some(22_222_223)));
        assert!(afb.is_empty());
    }
}
