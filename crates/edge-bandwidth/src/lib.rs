//! edge-bandwidth — sequence-based idempotent bandwidth reporter (R3.5).
//!
//! Mỗi 60s đo cumulative bytes per session, tính delta, gửi callback với
//! sequence counter monotonic (persisted to disk for restart-safety) để
//! dun-api dedup được khi retry.

pub mod delta;
pub mod persistence;
pub mod reporter;

pub use persistence::SequenceStore;
pub use reporter::BandwidthReporter;
