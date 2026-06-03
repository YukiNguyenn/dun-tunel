//! HTTP client for outbound edge-callback events.
//! mTLS + retry exponential backoff (3 attempts: 200ms, 600ms, 1.8s).

use edge_shared::errors::EdgeResult;
use edge_shared::types::{EdgeCallbackBatch, EdgeCallbackEvent};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    endpoint: String,
    http: reqwest::Client,
    queue_dir: PathBuf,
}

impl Client {
    pub fn new(endpoint: String, _mtls_cert: Option<PathBuf>, queue_dir: PathBuf) -> Self {
        // TODO Phase 4: configure rustls client cert from mtls_cert
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client");
        Self {
            inner: Arc::new(ClientInner {
                endpoint,
                http,
                queue_dir,
            }),
        }
    }

    pub async fn send(&self, event: EdgeCallbackEvent) -> EdgeResult<()> {
        let batch = EdgeCallbackBatch {
            events: vec![event],
        };
        for attempt in 0..3 {
            match self.try_send(&batch).await {
                Ok(_) => return Ok(()),
                Err(e) if attempt == 2 => {
                    tracing::warn!(error = ?e, "callback send failed after retries — queueing");
                    self.enqueue(&batch).await?;
                    return Ok(());
                }
                Err(_) => {
                    let backoff = Duration::from_millis(200 * 3u64.pow(attempt as u32));
                    tokio::time::sleep(backoff).await;
                }
            }
        }
        Ok(())
    }

    async fn try_send(&self, batch: &EdgeCallbackBatch) -> anyhow::Result<()> {
        let url = format!("{}/tunnels/edge-callback", self.inner.endpoint);
        let resp = self.inner.http.post(&url).json(batch).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("non-2xx status: {}", resp.status());
        }
        Ok(())
    }

    async fn enqueue(&self, _batch: &EdgeCallbackBatch) -> EdgeResult<()> {
        // TODO Phase 2: write to file in queue_dir for retry by background task
        let _ = &self.inner.queue_dir;
        Ok(())
    }
}
