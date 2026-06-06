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

/// Maximum viewers per session (R8.8 — flat cap 30).
pub const VIEWER_CAP_PER_SESSION: u32 = 30;
