//! Edge configuration loaded from env vars.
//! Tham chiếu README env vars table.

use anyhow::{Context, Result};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct EdgeConfig {
    pub region: String,
    pub bind_port: u16,
    pub dun_api_endpoint: String,
    pub dun_api_key: Option<String>,
    pub mtls_cert_path: Option<PathBuf>,
    pub mtls_key_path: Option<PathBuf>,
    pub mtls_ca_path: Option<PathBuf>,
    pub caddy_admin_url: String,
    pub rathole_config_path: PathBuf,
    pub persistent_queue_dir: PathBuf,
    pub sfu_workers: usize,
    pub jwt_secret_v1: Option<String>,
    pub jwt_secret_v2: Option<String>,
    /// Root domain for viewer subdomains (e.g. `dun-studio.xyz`).
    /// Used at startup to bootstrap the Caddy TLS automation policy
    /// for the `*.<region>.<domain>` wildcard cert. Optional —
    /// edge-control still works without it but Caddy will not auto-
    /// renew the wildcard cert.
    pub share_tunnel_domain: Option<String>,
    /// Cloudflare API token with DNS:Edit on the share-tunnel zone.
    /// Required when `share_tunnel_domain` is set so Caddy can
    /// satisfy the DNS-01 challenge.
    pub cloudflare_api_token: Option<String>,
    /// Optional `host:port` of the `edge-viewer-gate` sidecar, as seen
    /// from inside the Caddy container's loopback. When `Some`, every
    /// per-session viewer route installs a `forward_auth` block in
    /// front of the rathole tunnel so non-public-asset requests must
    /// pass an EdDSA cookie verification before reaching the
    /// container's WS / HTTP endpoints. When `None`, viewer subdomains
    /// stay open to anyone with the URL — only acceptable for dev.
    pub viewer_gate_upstream: Option<String>,
}

impl EdgeConfig {
    pub fn from_env() -> Result<Self> {
        let region = std::env::var("REGION_ID").context("REGION_ID env var required")?;
        let bind_port = std::env::var("EDGE_BIND_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8443);
        let dun_api_endpoint =
            std::env::var("DUN_API_ENDPOINT").context("DUN_API_ENDPOINT required")?;
        let dun_api_key = std::env::var("DUN_API_KEY").ok();

        let mtls_cert_path = std::env::var("EDGE_MTLS_CERT_PATH").ok().map(PathBuf::from);
        let mtls_key_path = std::env::var("EDGE_MTLS_KEY_PATH").ok().map(PathBuf::from);
        let mtls_ca_path = std::env::var("EDGE_MTLS_CA_PATH").ok().map(PathBuf::from);

        let caddy_admin_url = std::env::var("CADDY_ADMIN_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:2019".to_string());
        let rathole_config_path = std::env::var("RATHOLE_CONFIG_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/etc/rathole/server.toml"));
        let persistent_queue_dir = std::env::var("PERSISTENT_QUEUE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/var/lib/dun-tunel/queue"));
        let sfu_workers = std::env::var("SFU_WORKERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(num_cpus_or_default);

        let jwt_secret_v1 = std::env::var("TUNNEL_JWT_SECRET_V1").ok();
        let jwt_secret_v2 = std::env::var("TUNNEL_JWT_SECRET_V2").ok();

        let share_tunnel_domain = std::env::var("SHARE_TUNNEL_DOMAIN").ok();
        let cloudflare_api_token = std::env::var("CLOUDFLARE_API_TOKEN").ok();
        // Default to the canonical loopback `127.0.0.1:9444` matching
        // the `edge-viewer-gate` sidecar bind address. Operators can
        // override with `EDGE_VIEWER_GATE_UPSTREAM=disabled` to skip
        // the auth gate entirely (dev / staging without the sidecar
        // running) — any falsy value disables the forward_auth shim.
        let viewer_gate_upstream = std::env::var("EDGE_VIEWER_GATE_UPSTREAM")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| {
                !s.is_empty()
                    && !s.eq_ignore_ascii_case("disabled")
                    && !s.eq_ignore_ascii_case("off")
                    && !s.eq_ignore_ascii_case("none")
            })
            .or_else(|| Some("127.0.0.1:9444".to_string()));

        Ok(Self {
            region,
            bind_port,
            dun_api_endpoint,
            dun_api_key,
            mtls_cert_path,
            mtls_key_path,
            mtls_ca_path,
            caddy_admin_url,
            rathole_config_path,
            persistent_queue_dir,
            sfu_workers,
            jwt_secret_v1,
            jwt_secret_v2,
            share_tunnel_domain,
            cloudflare_api_token,
            viewer_gate_upstream,
        })
    }
}

fn num_cpus_or_default() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}
