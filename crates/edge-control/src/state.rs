//! AppState injected vào axum routes.
//!
//! Wires in the JwtVerifier with HttpRevocationOracle so all token verification
//! across rathole-bridge, viewer endpoints, and snapshot APIs goes through a
//! single source-of-truth (R6.4, 5b.7).

use crate::config::EdgeConfig;
use anyhow::Result;
use edge_bandwidth::persistence::SequenceStore;
use edge_bandwidth::BandwidthReporter;
use edge_caddy_bridge::AdminClient;
use edge_callback_client::Client as CallbackClient;
use edge_rathole_bridge::{PortAllocator, ServiceRegistry};
use edge_sfu::RouterManager;
use edge_shared::jwt::JwtVerifier;
use edge_shared::HttpRevocationOracle;
use std::sync::Arc;
use std::time::Instant;

pub struct AppState {
    pub region: String,
    pub started_at: Instant,
    pub sfu: RouterManager,
    pub rathole: ServiceRegistry,
    pub port_allocator: Arc<PortAllocator>,
    pub caddy: AdminClient,
    pub callback: CallbackClient,
    pub bandwidth: BandwidthReporter,
    pub jwt: JwtVerifier,
}

impl AppState {
    pub async fn initialize(cfg: &EdgeConfig) -> Result<Self> {
        let sfu = RouterManager::new(cfg.sfu_workers).await?;
        let rathole = ServiceRegistry::new(cfg.rathole_config_path.clone());
        let port_allocator = Arc::new(PortAllocator::new());
        let caddy = AdminClient::new(cfg.caddy_admin_url.clone());
        let callback = CallbackClient::new(
            cfg.dun_api_endpoint.clone(),
            cfg.mtls_cert_path.clone(),
            cfg.persistent_queue_dir.clone(),
        );
        let bandwidth = BandwidthReporter::start(
            sfu.clone_handle(),
            callback.clone(),
            cfg.region.clone(),
            SequenceStore::new(cfg.persistent_queue_dir.clone()),
        );

        let mut jwt = JwtVerifier::new();
        if let Some(secret) = &cfg.jwt_secret_v1 {
            jwt.add_key("v1", secret.as_bytes());
        }
        if let Some(secret) = &cfg.jwt_secret_v2 {
            jwt.add_key("v2", secret.as_bytes());
        }
        // Wire revocation oracle if API key is configured. In dev/test
        // environments without DUN_API_KEY we leave it unset — verifier
        // falls through to allow (only for local development).
        if let Some(api_key) = &cfg.dun_api_key {
            let oracle = Arc::new(HttpRevocationOracle::new(
                cfg.dun_api_endpoint.clone(),
                api_key.clone(),
            ));
            jwt = jwt.with_revocation(oracle);
            tracing::info!("revocation oracle enabled");
        } else {
            tracing::warn!("DUN_API_KEY not set — revocation NOT enforced (dev mode only)");
        }

        Ok(Self {
            region: cfg.region.clone(),
            started_at: Instant::now(),
            sfu,
            rathole,
            port_allocator,
            caddy,
            callback,
            bandwidth,
            jwt,
        })
    }
}
