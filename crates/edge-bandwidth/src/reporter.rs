//! 60-second bandwidth reporter loop.
//! Per session: track cumulative_bytes, next_sequence, last_reported_at.
//! Skip delta < 0.1 MB to reduce noise.
//!
//! Restart safety (R3.5): the next-sequence counter is bootstrapped
//! from disk via `SequenceStore::load_all` and persisted after every
//! successful callback send. A crash between send and persist replays
//! the same sequence — dun-api dedupes.

use crate::persistence::SequenceStore;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use edge_callback_client::Client as CallbackClient;
use edge_sfu::RouterManager;
use edge_shared::types::{EdgeCallbackEvent, SessionId};
use std::sync::Arc;
use std::time::Duration;

const REPORT_INTERVAL_SECS: u64 = 60;
const MIN_DELTA_MB: f64 = 0.1;

#[derive(Clone)]
pub struct BandwidthReporter {
    #[allow(dead_code)]
    state: Arc<DashMap<SessionId, SessionState>>,
}

struct SessionState {
    cumulative_bytes: u64,
    next_sequence: u64,
    last_reported_at: DateTime<Utc>,
}

impl BandwidthReporter {
    pub fn start(
        sfu: RouterManager,
        callback: CallbackClient,
        region: String,
        store: SequenceStore,
    ) -> Self {
        let state: Arc<DashMap<SessionId, SessionState>> = Arc::new(DashMap::new());
        let runner = ReporterRunner {
            sfu,
            callback,
            region,
            state: Arc::clone(&state),
            store,
        };
        tokio::spawn(runner.run_loop());
        Self { state }
    }
}

struct ReporterRunner {
    sfu: RouterManager,
    callback: CallbackClient,
    region: String,
    state: Arc<DashMap<SessionId, SessionState>>,
    store: SequenceStore,
}

impl ReporterRunner {
    async fn run_loop(self) {
        if let Err(err) = self.store.ensure_dir().await {
            tracing::warn!(error = ?err, "sequence store dir create failed");
        }
        match self.store.load_all().await {
            Ok(map) => {
                let now = Utc::now();
                for (session_id, sequence) in map {
                    self.state.insert(
                        session_id,
                        SessionState {
                            cumulative_bytes: 0,
                            next_sequence: sequence,
                            last_reported_at: now,
                        },
                    );
                }
            }
            Err(err) => tracing::warn!(error = ?err, "sequence store load_all failed"),
        }

        let mut tick = tokio::time::interval(Duration::from_secs(REPORT_INTERVAL_SECS));
        loop {
            tick.tick().await;
            let active = self.sfu.list_active_sessions().await;
            for session_id in &active {
                if let Err(e) = self.report_one(session_id).await {
                    tracing::warn!(error = ?e, %session_id, "bandwidth_report_failed");
                }
            }
            let active_set: std::collections::HashSet<_> = active.into_iter().collect();
            let ended: Vec<String> = self
                .state
                .iter()
                .map(|e| e.key().clone())
                .filter(|k| !active_set.contains(k))
                .collect();
            for id in ended {
                self.state.remove(&id);
                if let Err(err) = self.store.remove(&id).await {
                    tracing::warn!(error = ?err, %id, "sequence file cleanup failed");
                }
            }
        }
    }

    async fn report_one(&self, session_id: &str) -> anyhow::Result<()> {
        let cumulative = self.sfu.get_session_bytes(session_id).await?;
        let now = Utc::now();
        let mut entry = self
            .state
            .entry(session_id.to_string())
            .or_insert_with(|| SessionState {
                cumulative_bytes: 0,
                next_sequence: 0,
                last_reported_at: now,
            });
        let delta_bytes = cumulative.saturating_sub(entry.cumulative_bytes);
        let delta_mb = delta_bytes as f64 / 1_048_576.0;
        if delta_mb < MIN_DELTA_MB {
            return Ok(());
        }
        let event = EdgeCallbackEvent::BandwidthDelta {
            session_id: session_id.to_string(),
            delta_mb,
            interval_start: entry.last_reported_at,
            interval_end: now,
            sequence: entry.next_sequence,
        };
        self.callback.send(event).await?;
        entry.next_sequence += 1;
        entry.cumulative_bytes = cumulative;
        entry.last_reported_at = now;
        let next_sequence = entry.next_sequence;
        drop(entry);
        if let Err(err) = self.store.save(session_id, next_sequence).await {
            tracing::warn!(error = ?err, %session_id, "sequence persist failed");
        }
        let _ = &self.region;
        Ok(())
    }
}
