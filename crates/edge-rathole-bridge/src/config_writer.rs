//! TOML config writer cho rathole server.
//!
//! Rathole server config schema (v0.5+):
//! ```toml
//! [server]
//! bind_addr = "0.0.0.0:2333"
//! default_token = "global-fallback"   # not used; per-service tokens override
//!
//! [server.services.<session_id>]
//! token = "<sha256-hex of tunnel jwt>"
//! bind_addr = "0.0.0.0:11042"        # Edge-side port viewers proxy to
//! ```
//!
//! Atomic write strategy:
//! 1. Write to `<config>.tmp` in the same dir
//! 2. fsync the temp file
//! 3. Atomically rename over the target — on POSIX guaranteed by rename(2)
//!
//! After rename we trigger reload via SIGHUP (Unix) or restart hint
//! (Windows; rathole on Windows uses control socket for hot reload).

use edge_shared::errors::{EdgeError, EdgeResult};
use edge_shared::types::{RatholeService, RatholeTransport};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct RatholeConfig<'a> {
    pub server: ServerConfig<'a>,
}

#[derive(Debug, Serialize)]
pub struct ServerConfig<'a> {
    pub bind_addr: &'a str,
    /// Per-service map keyed by session_id (= service name).
    pub services: BTreeMap<&'a str, ServiceEntry<'a>>,
}

#[derive(Debug, Serialize)]
pub struct ServiceEntry<'a> {
    pub token: &'a str,
    pub bind_addr: &'a str,
    /// Skipped when `None` (TCP default) so existing service blocks
    /// stay byte-identical and don't trigger reload no-ops on upgrade.
    /// `Some("udp")` adds `type = "udp"` for the SFU media tunnel.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub service_type: Option<&'a str>,
}

/// Render `RatholeService` list into a TOML string with stable ordering.
///
/// Stable ordering is important for change detection: two equal service sets
/// must produce byte-identical output so we can skip reload no-ops.
pub fn render_toml(server_bind: &str, services: &[RatholeService]) -> EdgeResult<String> {
    let map: BTreeMap<&str, ServiceEntry> = services
        .iter()
        .map(|s| {
            let service_type = match s.transport {
                // Default TCP — emit nothing so Phase 1 deployments
                // produce identical TOML to before.
                None | Some(RatholeTransport::Tcp) => None,
                Some(RatholeTransport::Udp) => Some("udp"),
            };
            (
                s.name.as_str(),
                ServiceEntry {
                    token: s.token_hash.as_str(),
                    bind_addr: s.bind_addr.as_str(),
                    service_type,
                },
            )
        })
        .collect();

    let cfg = RatholeConfig {
        server: ServerConfig {
            bind_addr: server_bind,
            services: map,
        },
    };

    toml::to_string(&cfg).map_err(|e| EdgeError::Config(format!("toml encode: {e}")))
}

/// Atomically write `contents` to `path`. Caller is responsible for triggering
/// reload after this returns Ok.
pub async fn atomic_write(path: &Path, contents: &str) -> EdgeResult<()> {
    use tokio::io::AsyncWriteExt;

    let dir = path
        .parent()
        .ok_or_else(|| EdgeError::Config(format!("config path has no parent: {path:?}")))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| EdgeError::Config(format!("config path has no file name: {path:?}")))?
        .to_string_lossy()
        .into_owned();
    let tmp = dir.join(format!(".{file_name}.tmp"));

    // Open with write+create+truncate, write, then fsync via the same handle.
    // Doing fsync via a re-opened handle races on Windows (handle from write
    // not yet fully released → ERROR_ACCESS_DENIED).
    let mut f = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .await?;
    f.write_all(contents.as_bytes()).await?;
    f.flush().await?;
    f.sync_all().await?;
    drop(f);

    // On Windows, rename fails if target exists. Remove first (best-effort).
    if path.exists() {
        let _ = tokio::fs::remove_file(path).await;
    }
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_toml_stable_ordering() {
        let svcs = vec![
            RatholeService {
                name: "zzz".into(),
                token_hash: "tokz".into(),
                bind_addr: "0.0.0.0:11001".into(),
                transport: None,
            },
            RatholeService {
                name: "aaa".into(),
                token_hash: "toka".into(),
                bind_addr: "0.0.0.0:11002".into(),
                transport: None,
            },
        ];
        let out1 = render_toml("0.0.0.0:2333", &svcs).unwrap();
        let out2 = render_toml("0.0.0.0:2333", &svcs).unwrap();
        assert_eq!(out1, out2);
        // BTreeMap → "aaa" must appear before "zzz"
        let aaa_pos = out1.find("services.aaa").unwrap();
        let zzz_pos = out1.find("services.zzz").unwrap();
        assert!(aaa_pos < zzz_pos);
    }

    #[test]
    fn render_toml_tcp_default_omits_type_field() {
        // Backward-compat: every existing Phase 1 deployment must
        // continue producing identical TOML to before — type field
        // is suppressed when transport is Tcp/None so reload doesn't
        // think the file changed across an in-place upgrade.
        let svcs = vec![RatholeService {
            name: "abc".into(),
            token_hash: "tok".into(),
            bind_addr: "0.0.0.0:11042".into(),
            transport: Some(RatholeTransport::Tcp),
        }];
        let out = render_toml("0.0.0.0:2333", &svcs).unwrap();
        assert!(!out.contains("type"), "TCP default must not emit `type` field; got:\n{out}");
        assert!(out.contains("services.abc"));
    }

    #[test]
    fn render_toml_udp_emits_type_field() {
        let svcs = vec![RatholeService {
            name: "session-rtp".into(),
            token_hash: "tok".into(),
            bind_addr: "0.0.0.0:50042".into(),
            transport: Some(RatholeTransport::Udp),
        }];
        let out = render_toml("0.0.0.0:2333", &svcs).unwrap();
        assert!(out.contains("type = \"udp\""), "UDP services must emit `type = \"udp\"`; got:\n{out}");
    }

    #[tokio::test]
    async fn atomic_write_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rathole.toml");
        atomic_write(&path, "hello").await.unwrap();
        let read = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(read, "hello");
    }
}
