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
use std::num::NonZeroU32;

const DEFAULT_RTC_MIN_PORT: u16 = 50_000;
const DEFAULT_RTC_MAX_PORT: u16 = 60_000;

/// PlainTransport per-session port pool (R8.1).
///
/// Each ShareSession provisions its own PlainTransport for the Neko →
/// SFU GStreamer pipeline. Mediasoup binds an exclusive UDP port per
/// transport so we MUST give it a range to pick from — using a single
/// fixed port causes the second concurrent session to fail with
/// `uv_udp_bind() failed: address already in use` (observed on
/// 2026-06-04 after smoke #1 left a lingering bind).
///
/// Range chosen to sit between conventional VoIP ports (5004 was the
/// PoC default) and the WebRTC ephemeral range (50000+). 100 ports is
/// 50× headroom over our viewer-cap target so we never run out.
const DEFAULT_PLAIN_RTP_MIN_PORT: u16 = 5_000;
const DEFAULT_PLAIN_RTP_MAX_PORT: u16 = 5_099;

const PLAIN_PAYLOAD_TYPE: u8 = 96;
const PLAIN_CLOCK_RATE: u32 = 90_000;
const PLAIN_SSRC: u32 = 22_222_222;

#[derive(Debug, Clone, Copy)]
pub struct RouterListenInfo {
    pub listen_ip: IpAddr,
    pub announced_ip: IpAddr,
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
    let mut opts = WebRtcTransportOptions::new(WebRtcTransportListenInfos::new(ListenInfo {
        protocol: Protocol::Udp,
        ip: listen.listen_ip,
        announced_address: Some(listen.announced_ip.to_string()),
        expose_internal_ip: false,
        port: None,
        port_range: Some(listen.rtc_min_port..=listen.rtc_max_port),
        flags: None,
        send_buffer_size: None,
        recv_buffer_size: None,
    }));
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
