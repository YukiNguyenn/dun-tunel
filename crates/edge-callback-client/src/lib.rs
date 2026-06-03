//! edge-callback-client — outbound HTTP client tới dun-api `/tunnels/edge-callback`.
//! Retry exponential backoff + persistent queue khi dun-api unreachable.

pub mod client;
pub mod queue;

pub use client::Client;
pub use edge_shared::types::EdgeCallbackEvent;
