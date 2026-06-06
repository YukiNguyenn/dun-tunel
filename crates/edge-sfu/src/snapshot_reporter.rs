//! 30-second session snapshot reporter.
//!
//! Pushes the authoritative `viewers.len()` for every active session
//! to dun-api so the host UI's `viewerCount` (and equivalently,
//! `activeConnections`) self-heals. Compared to the per-event
//! `viewer_connected/disconnected` callbacks, this loop:
//!
//!   - REPLACES the count instead of incrementing — any drift caused
//!     by a dropped event collapses back to the truth in ≤ 30s.
//!   - Runs unconditionally; doesn't depend on the event-driven path
//!     succeeding. If dun-api restarts and the in-memory `viewerCount`
//!     was 0 from boot, the next snapshot pushes the correct value.
//!   - Lives in the SFU crate because the source of truth for the
//!     count is `RouterManager::viewer_count`. Bandwidth uses the
//!     same pattern but with delta + sequence; here a flat count is
//!     enough since the consumer side overwrites.
//!
//! Failure handling: each session's send is best-effort (Client::send
//! retries 3× internally). A whole-tick failure (network outage)
//! drops the snapshot without queuing — the next tick (30s later)
//! retries. Drift bound: 30s + retry budget.

use crate::RouterManager;
use edge_callback_client::Client as CallbackClient;
use edge_shared::types::EdgeCallbackEvent;
use std::time::Duration;

const SNAPSHOT_INTERVAL_SECS: u64 = 30;

#[derive(Clone)]
pub struct SessionSnapshotReporter;

impl SessionSnapshotReporter {
    /// Start the reporter loop and return a handle. The loop runs
    /// for the lifetime of the spawned tokio task; dropping the
    /// returned `SessionSnapshotReporter` does NOT stop it, by design
    /// — Edge process exit is the only reason to terminate snapshot
    /// pushes.
    pub fn start(sfu: RouterManager, callback: CallbackClient) -> Self {
        let runner = Runner { sfu, callback };
        tokio::spawn(runner.run_loop());
        Self
    }
}

struct Runner {
    sfu: RouterManager,
    callback: CallbackClient,
}

impl Runner {
    async fn run_loop(self) {
        let mut tick = tokio::time::interval(Duration::from_secs(SNAPSHOT_INTERVAL_SECS));
        // First tick fires immediately; skip it so a freshly-started
        // Edge waits a full interval before reporting (gives sessions
        // time to finalise). Subsequent ticks fire every 30s.
        tick.tick().await;
        loop {
            tick.tick().await;
            let active = self.sfu.list_active_sessions().await;
            for session_id in active {
                let count = self.sfu.viewer_count(&session_id).await;
                let event = EdgeCallbackEvent::SessionSnapshot {
                    session_id: session_id.clone(),
                    active_connections: count,
                };
                if let Err(err) = self.callback.send(event).await {
                    tracing::warn!(
                        error = ?err,
                        %session_id,
                        active_connections = count,
                        "session_snapshot send failed"
                    );
                }
            }
        }
    }
}
