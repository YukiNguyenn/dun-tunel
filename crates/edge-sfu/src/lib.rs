//! edge-sfu — mediasoup-rust wrapper.
//!
//! Quản lý Router/Transport/Producer/Consumer per session.
//! Source-of-truth cho viewer count cap (R8.8) — KHÔNG dựa vào MongoDB.
//!
//! See spec R8 + R23.2.

pub mod router_manager;
pub mod stats;
pub mod transport;
pub mod viewer_cap;

pub use router_manager::{
    ConsumedInfo, ConsumerTransportInfo, ProvisionedRouter, RouterManager, SessionState,
    ViewerSlot,
};
pub use transport::RouterListenInfo;

/// Re-export of mediasoup types needed by callers (edge-control's
/// SFU WS handler) so they don't have to depend on the heavy
/// `mediasoup` crate transitively. This keeps `mediasoup-sys`
/// (Python + C++ build) confined to `edge-sfu`.
///
/// All names below are pulled from `mediasoup::prelude` which
/// itself re-exports from `mediasoup_types::data_structures` /
/// `mediasoup_types::rtp_parameters` / submodules. Single source
/// path keeps the dependency surface symmetrical with the PoC
/// viewer code.
pub use mediasoup::prelude::{
    ConsumerId, DtlsParameters, IceCandidate, IceParameters, ProducerId,
    RtpCapabilities, RtpCapabilitiesFinalized, TransportId,
};

/// Maximum viewers per session (R8.8 — flat cap 30).
pub const VIEWER_CAP_PER_SESSION: u32 = 30;
