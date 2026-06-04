//! AppState injected vào axum routes.
//!
//! Wires in the JwtVerifier with HttpRevocationOracle so all token verification
//! across rathole-bridge, viewer endpoints, and snapshot APIs goes through a
//! single source-of-truth (R6.4, 5b.7).

use crate::config::EdgeConfig;
use crate::subdomain_store::SubdomainStore;
use anyhow::Result;
use dashmap::DashMap;
use edge_bandwidth::persistence::SequenceStore;
use edge_bandwidth::BandwidthReporter;
use edge_caddy_bridge::AdminClient;
use edge_callback_client::Client as CallbackClient;
use edge_rathole_bridge::{PortAllocator, ServiceRegistry};
use edge_sfu::RouterManager;
use edge_shared::jwt::JwtVerifier;
use edge_shared::types::SessionId;
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
    /// Maps `session_id` → Caddy route host (subdomain) so the
    /// deprovision handler can locate the registered route entry.
    /// Populated on `POST /v1/tunnels`, drained on `DELETE` or on
    /// failed provisioning rollback. Backed by `SubdomainStore` for
    /// restart safety — without persistence, a restart while sessions
    /// are active would lose the in-memory entries and subsequent
    /// DELETEs would silently leak Caddy routes.
    pub session_subdomains: Arc<DashMap<SessionId, String>>,
    pub subdomain_store: SubdomainStore,
}

impl AppState {
    pub async fn initialize(cfg: &EdgeConfig) -> Result<Self> {
        let sfu = RouterManager::new(cfg.sfu_workers).await?;
        let rathole = ServiceRegistry::new(cfg.rathole_config_path.clone());
        let port_allocator = Arc::new(PortAllocator::new());
        let caddy = AdminClient::new(cfg.caddy_admin_url.clone());

        // Bootstrap the wildcard TLS policy when both domain + CF
        // token are configured. This replaces the `*.<region>.<domain>:8443`
        // site block that used to live in `Caddyfile.tpl` — that
        // block was removed because the Caddyfile adapter emitted
        // it as a `terminal: true` route entry that shadowed every
        // dynamic per-session route, breaking viewer URLs with 404.
        // Without the policy here Caddy would still serve dynamic
        // routes but couldn't auto-issue the wildcard cert.
        match (&cfg.share_tunnel_domain, &cfg.cloudflare_api_token) {
            (Some(domain), Some(token)) => {
                if let Err(e) = caddy
                    .ensure_wildcard_tls_policy(&cfg.region, domain, token)
                    .await
                {
                    tracing::warn!(
                        error = ?e,
                        region = %cfg.region,
                        domain = %domain,
                        "ensure_wildcard_tls_policy failed; viewer subdomains will use untrusted cert until next retry"
                    );
                }
            }
            (Some(_), None) => tracing::warn!(
                "SHARE_TUNNEL_DOMAIN set but CLOUDFLARE_API_TOKEN missing — wildcard cert will NOT auto-renew"
            ),
            (None, _) => tracing::warn!(
                "SHARE_TUNNEL_DOMAIN not set — wildcard TLS policy bootstrap skipped (dev mode only)"
            ),
        }
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

        // Subdomain store: rebuild session_id → subdomain map at boot.
        // Survives restart so the deprovision handler can still
        // resolve the Caddy @id key (host) for sessions provisioned
        // before the restart.
        let subdomain_store = SubdomainStore::new(cfg.persistent_queue_dir.clone());
        if let Err(e) = subdomain_store.ensure_dir().await {
            tracing::warn!(error = ?e, "subdomain_store ensure_dir failed; continuing without persistence");
        }
        let session_subdomains = Arc::new(DashMap::new());
        match subdomain_store.load_all().await {
            Ok(map) => {
                let count = map.len();
                for (k, v) in map {
                    session_subdomains.insert(k, v);
                }
                if count > 0 {
                    tracing::info!(count, "rehydrated session→subdomain mapping from disk");
                }
            }
            Err(e) => tracing::warn!(error = ?e, "subdomain_store load_all failed; starting empty"),
        }

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
            session_subdomains,
            subdomain_store,
        })
    }
}
