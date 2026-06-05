//! Config from env. Fail loud on missing required vars.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    /// Bind address. Default `127.0.0.1:9444` (loopback only — Caddy
    /// is the only intended caller; expose externally would let
    /// anyone forge auth decisions).
    pub bind_addr: SocketAddr,

    /// JWKS URL on dun-api. Must include scheme + path. The sidecar
    /// fetches this at startup and refreshes every
    /// `refresh_interval`.
    pub jwks_url: String,

    /// How often to refresh the JWKS document. Default 24h.
    pub refresh_interval: Duration,

    /// Revocation list URL on dun-api (optional). When set, the
    /// sidecar polls every `revocation_poll_interval` to mirror the
    /// jti revocation set into RAM. Without it we still verify
    /// signatures but cannot block already-issued cookies until
    /// they expire naturally.
    pub revocation_url: Option<String>,

    /// API key used as `X-Edge-Api-Key` for the revocation pull.
    /// Must match dun-api `EDGE_CALLBACK_API_KEY`.
    pub revocation_api_key: Option<String>,

    /// Poll interval for the revocation list. Default 5s.
    pub revocation_poll_interval: Duration,

    /// Strict mode: when `true` the verifier rejects every cookie if
    /// the revocation list is **stale** (last successful poll older
    /// than `revocation_max_staleness`). Used for high-trust
    /// deployments where a leaked cookie surviving its 10-minute
    /// natural TTL is unacceptable. When `false` (default) a stale
    /// list falls back to "no extra revocations known" — the cookie
    /// signature + exp still gate access, so a fail-OPEN is no worse
    /// than the cookie's TTL window.
    pub revocation_required: bool,

    /// Max age (seconds) of the last successful revocation poll
    /// before strict mode trips and starts rejecting. Should be a
    /// small multiple of `revocation_poll_interval` so transient
    /// hiccups don't immediately fail-CLOSED. Default 30s
    /// (= 6× the default 5s poll).
    pub revocation_max_staleness: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let bind_addr = std::env::var("BIND_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:9444".to_string())
            .parse::<SocketAddr>()
            .context("BIND_ADDR is not a valid SocketAddr")?;

        let jwks_url = std::env::var("JWKS_URL")
            .context("JWKS_URL env var is required (dun-api /api/viewer/jwks)")?;

        let refresh_interval = std::env::var("JWKS_REFRESH_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(24 * 60 * 60));

        let revocation_url = std::env::var("REVOCATION_URL").ok();
        let revocation_api_key = std::env::var("REVOCATION_API_KEY").ok();
        let revocation_poll_interval = std::env::var("REVOCATION_POLL_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(5));
        // Default OFF for backward compat with dev / staging stacks
        // that don't run a revocation feed. Set to "true"/"1"/"yes"
        // in production where instant revocation is required.
        let revocation_required = std::env::var("REVOCATION_REQUIRED")
            .ok()
            .map(|s| {
                let s = s.trim().to_ascii_lowercase();
                s == "true" || s == "1" || s == "yes" || s == "on"
            })
            .unwrap_or(false);
        let revocation_max_staleness = std::env::var("REVOCATION_MAX_STALENESS_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(30));

        Ok(Self {
            bind_addr,
            jwks_url,
            refresh_interval,
            revocation_url,
            revocation_api_key,
            revocation_poll_interval,
            revocation_required,
            revocation_max_staleness,
        })
    }
}
