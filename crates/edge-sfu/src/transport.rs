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

/// Build WebRtcTransport options for a viewer Consumer/Producer pair.
/// SCTP enabled so the viewer can author the `neko-input` DataChannel.
pub fn create_consumer_transport_options(listen: &RouterListenInfo) -> WebRtcTransportOptions {
    let mut listen_infos = WebRtcTransportListenInfos::new(ListenInfo {
        protocol: Protocol::Udp,
        ip: listen.listen_ip,
        announced_address: Some(listen.announced_ip.to_string()),
        expose_internal_ip: false,
        port: None,
        port_range: Some(listen.rtc_min_port..=listen.rtc_max_port),
        flags: None,
        send_buffer_size: None,
        recv_buffer_size: None,
    });

    // Optional second ICE candidate for same-LAN (hairpin) testing.
    // When `SFU_ANNOUNCED_IP_LAN` is set, advertise the edge's private
    // IP as an additional host candidate so a viewer on the same LAN can
    // connect directly instead of hairpinning off the public IP. The
    // browser's ICE agent picks whichever candidate pair works (public
    // for remote viewers, private for same-LAN), so this is safe to send
    // to everyone — remote viewers simply fail the private pair and fall
    // back to the public one. Unset in production.
    if let Some(lan_ip) = listen.lan_announced_ip {
        listen_infos = listen_infos.insert(ListenInfo {
            protocol: Protocol::Udp,
            ip: listen.listen_ip,
            announced_address: Some(lan_ip.to_string()),
            expose_internal_ip: false,
            port: None,
            port_range: Some(listen.rtc_min_port..=listen.rtc_max_port),
            flags: None,
            send_buffer_size: None,
            recv_buffer_size: None,
        });
    }

    let mut opts = WebRtcTransportOptions::new(listen_infos);
    opts.enable_sctp = true;
    opts.max_send_message_size = 262_144;
    opts
}

/// RTP parameters of the producer fed by the gstreamer pipeline. Must
/// match Neko's `rtpvp8pay pt=96` exactly — payload type, clock rate,
/// fixed SSRC.
pub fn plain_producer_rtp_parameters() -> RtpParameters {
    RtpParameters {
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
    }
}

/// RTP parameters of the Opus audio producer fed by the GStreamer
/// `rtpopuspay pt=111 ssrc=22222223` branch on the SAME PlainTransport.
/// Stereo, 48 kHz, in-band FEC — mirrors Neko's own opus encode so the
/// viewer hears the same audio as the host.
pub fn plain_audio_producer_rtp_parameters() -> RtpParameters {
    RtpParameters {
        mid: None,
        codecs: vec![RtpCodecParameters::Audio {
            mime_type: MimeTypeAudio::Opus,
            payload_type: PLAIN_AUDIO_PAYLOAD_TYPE,
            clock_rate: NonZeroU32::new(PLAIN_AUDIO_CLOCK_RATE).unwrap(),
            channels: NonZeroU8::new(2).unwrap(),
            parameters: RtpCodecParametersParameters::from([("useinbandfec", 1_u32.into())]),
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
