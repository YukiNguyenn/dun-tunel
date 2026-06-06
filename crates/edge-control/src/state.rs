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
use edge_sfu::{RouterManager, SessionSnapshotReporter};
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
        // Pass dun-api's loopback upstream into the Caddy admin client
        // so every per-session route inserts a split-route block for
        // viewer-cookie endpoints (R9.4). Path: parse `host:port` out
        // of `DUN_API_ENDPOINT`. We accept the canonical
        // `http://host:port[/path]` form and tolerate trailing slashes.
        let dun_api_upstream = parse_dun_api_upstream(&cfg.dun_api_endpoint);
        if dun_api_upstream.is_none() {
            tracing::warn!(
                endpoint = %cfg.dun_api_endpoint,
                "could not parse host:port from DUN_API_ENDPOINT — viewer cookie endpoints will 404 \
                 because Caddy will route `/viewer/exchange` into the rathole tunnel instead of dun-api"
            );
        }
        let auth_gate_upstream = cfg.viewer_gate_upstream.clone();
        if auth_gate_upstream.is_none() {
            tracing::warn!(
                "EDGE_VIEWER_GATE_UPSTREAM disabled — viewer subdomains will NOT enforce cookie auth. \
                 Anyone with the URL can hit the container's WS / HTTP endpoints. Dev mode only."
            );
        } else if let Some(ref gate) = auth_gate_upstream {
            tracing::info!(%gate, "viewer cookie auth gate enabled (forward_auth → edge-viewer-gate)");
        }
        // edge-control loopback for the SFU signalling split-route.
        // edge-control binds `0.0.0.0:<bind_port>` (typically 9443),
        // and Caddy in `network_mode: host` reaches it via loopback.
        // The split forwards `/v1/sfu/*` to this upstream so the
        // viewer mediasoup-client opens its WS on the same origin
        // as the share page (R9.4 cookie domain pinning).
        let edge_control_upstream = Some(format!("127.0.0.1:{}", cfg.bind_port));
        tracing::info!(
            upstream = %edge_control_upstream.as_deref().unwrap_or("(disabled)"),
            "SFU signalling split-route enabled (/v1/sfu/* → edge-control loopback)"
        );
        let caddy = AdminClient::with_upstreams(
            cfg.caddy_admin_url.clone(),
            dun_api_upstream,
            auth_gate_upstream,
            edge_control_upstream,
        );

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

                // Register the `edge.<region>.<domain>` reverse-proxy
                // route via the admin API. Done after the TLS policy
                // so the wildcard cert is already in flight when the
                // route comes online (Caddy will obtain on demand
                // either way; ordering just makes the first request
                // less likely to race).
                if let Err(e) = caddy
                    .ensure_edge_admin_route(&cfg.region, domain, cfg.bind_port)
                    .await
                {
                    tracing::warn!(
                        error = ?e,
                        region = %cfg.region,
                        domain = %domain,
                        port = cfg.bind_port,
                        "ensure_edge_admin_route failed; dun-api → edge admin calls will 404 until next retry"
                    );
                }

                // Append a tail-of-routes 410 fallback for any
                // `*.<region>.<domain>` host that lacks a per-session
                // route. Without this, viewers landing on an expired
                // / revoked URL would see Caddy's bare default 404
                // body — confusing because they can't tell whether
                // the session ended or the tunnel itself is broken.
                // The 410 page is i18n-friendly Vietnamese and
                // explicitly says "session ended" so users know to
                // ask for a fresh share link.
                if let Err(e) = caddy
                    .ensure_session_ended_fallback(&cfg.region, domain)
                    .await
                {
                    tracing::warn!(
                        error = ?e,
                        region = %cfg.region,
                        domain = %domain,
                        "ensure_session_ended_fallback failed; expired viewer URLs will fall back to default Caddy response"
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

        // Authoritative session snapshot every 30s. Self-healing for
        // viewer count drift — see `SessionSnapshotReporter` doc
        // comment. We don't keep the handle in `AppState` because
        // the loop runs unconditionally for the process lifetime;
        // there's nothing to call from the routes.
        let _snapshot = SessionSnapshotReporter::start(
            sfu.clone_handle(),
            callback.clone(),
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

/// Extract `host:port` from a `DUN_API_ENDPOINT` like
/// `http://localhost:3010` or `http://localhost:3010/api`. Caddy
/// reverse_proxy `dial` only wants the authority part — including the
/// scheme would make Caddy try to connect to `tcp://http`. Returns
/// `None` when the endpoint is malformed or missing a port. Caller
/// surfaces a warning rather than failing startup so dev / unit-test
/// setups that don't run dun-api on the same host can still come up
/// (just with the legacy "everything goes through the tunnel" route
/// shape — which means viewer cookie endpoints will 404).
fn parse_dun_api_upstream(endpoint: &str) -> Option<String> {
    let trimmed = endpoint.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Strip scheme prefix if present.
    let after_scheme = trimmed
        .splitn(2, "://")
        .nth(1)
        .unwrap_or(trimmed);
    // Authority is everything before the first '/'.
    let authority = after_scheme.split('/').next().unwrap_or("");
    if authority.is_empty() {
        return None;
    }
    // Require an explicit port — otherwise Caddy would try the dial
    // on port 0 which fails immediately.
    if !authority.contains(':') {
        return None;
    }
    Some(authority.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dun_api_upstream_strips_scheme_and_path() {
        assert_eq!(
            parse_dun_api_upstream("http://localhost:3010"),
            Some("localhost:3010".to_string())
        );
        assert_eq!(
            parse_dun_api_upstream("http://localhost:3010/api"),
            Some("localhost:3010".to_string())
        );
        assert_eq!(
            parse_dun_api_upstream("https://api.dun-studio.xyz:8443/v1/"),
            Some("api.dun-studio.xyz:8443".to_string())
        );
    }

    #[test]
    fn parse_dun_api_upstream_rejects_missing_port() {
        assert_eq!(parse_dun_api_upstream("http://localhost"), None);
        assert_eq!(parse_dun_api_upstream(""), None);
    }
}
