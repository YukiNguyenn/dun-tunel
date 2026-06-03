//! ServiceRegistry — track active rathole service entries (session_id → port)
//! and persist+reload the rathole TOML config on every change.
//!
//! Reload strategies (in priority order):
//! 1. **PID file** (Unix): SIGHUP rathole pid (if `RATHOLE_PID_FILE` set).
//! 2. **Reload command** (Windows / fallback): exec a user-configured shell
//!    command like `rathole-reload.sh` that handles platform specifics.
//! 3. **No-op**: log warning and rely on rathole picking up next reconnect.
//!
//! All persist+reload calls are serialized through an internal mutex to avoid
//! interleaved writes producing inconsistent files.

use crate::config_writer::{atomic_write, render_toml};
use dashmap::DashMap;
use edge_shared::errors::{EdgeError, EdgeResult};
use edge_shared::types::{RatholeService, SessionId};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

const DEFAULT_SERVER_BIND: &str = "0.0.0.0:2333";

#[derive(Clone)]
pub struct ServiceRegistry {
    inner: Arc<ServiceRegistryInner>,
}

struct ServiceRegistryInner {
    services: DashMap<SessionId, RatholeService>,
    config_path: PathBuf,
    write_lock: Mutex<()>,
    server_bind: String,
}

impl ServiceRegistry {
    pub fn new(config_path: PathBuf) -> Self {
        let server_bind = std::env::var("RATHOLE_SERVER_BIND")
            .unwrap_or_else(|_| DEFAULT_SERVER_BIND.to_string());
        Self {
            inner: Arc::new(ServiceRegistryInner {
                services: DashMap::new(),
                config_path,
                write_lock: Mutex::new(()),
                server_bind,
            }),
        }
    }

    pub async fn register(&self, svc: RatholeService) -> EdgeResult<()> {
        self.inner.services.insert(svc.name.clone(), svc);
        self.persist_and_reload().await
    }

    pub async fn deregister(&self, session_id: &str) -> EdgeResult<()> {
        self.inner.services.remove(session_id);
        self.persist_and_reload().await
    }

    pub async fn list(&self) -> Vec<RatholeService> {
        self.inner.services.iter().map(|e| e.value().clone()).collect()
    }

    async fn persist_and_reload(&self) -> EdgeResult<()> {
        // Serialize writes; the lock guard is dropped at function end.
        let _guard = self.inner.write_lock.lock().await;

        let services: Vec<RatholeService> = self
            .inner
            .services
            .iter()
            .map(|e| e.value().clone())
            .collect();

        let toml = render_toml(&self.inner.server_bind, &services)?;
        atomic_write(&self.inner.config_path, &toml).await?;
        tracing::debug!(
            path = ?self.inner.config_path,
            count = services.len(),
            "rathole config written"
        );

        if let Err(e) = trigger_reload().await {
            // Reload failure is non-fatal — rathole will pick up the new config
            // on next client reconnect. Log and continue.
            tracing::warn!(error = ?e, "rathole reload trigger failed (will pick up on reconnect)");
        }
        Ok(())
    }
}

/// Try to reload rathole. Returns Ok if no reload mechanism is configured.
async fn trigger_reload() -> EdgeResult<()> {
    if let Ok(pid_file) = std::env::var("RATHOLE_PID_FILE") {
        return reload_via_pid_file(&pid_file).await;
    }
    if let Ok(cmd) = std::env::var("RATHOLE_RELOAD_CMD") {
        return reload_via_command(&cmd).await;
    }
    tracing::debug!("no rathole reload mechanism configured; skipping");
    Ok(())
}

#[cfg(unix)]
async fn reload_via_pid_file(pid_file: &str) -> EdgeResult<()> {
    let pid_str = tokio::fs::read_to_string(pid_file).await?;
    let pid: i32 = pid_str
        .trim()
        .parse()
        .map_err(|e| EdgeError::Config(format!("invalid pid file: {e}")))?;
    // SAFETY: kill is FFI but with a constrained signal value.
    let ret = unsafe { libc::kill(pid, libc::SIGHUP) };
    if ret != 0 {
        return Err(EdgeError::Config(format!(
            "kill SIGHUP {pid} failed: errno={}",
            std::io::Error::last_os_error()
        )));
    }
    tracing::info!(pid, "rathole SIGHUP sent");
    Ok(())
}

#[cfg(not(unix))]
async fn reload_via_pid_file(_pid_file: &str) -> EdgeResult<()> {
    Err(EdgeError::Config(
        "RATHOLE_PID_FILE reload not supported on this platform; use RATHOLE_RELOAD_CMD".into(),
    ))
}

async fn reload_via_command(cmd: &str) -> EdgeResult<()> {
    let mut parts = cmd.split_whitespace();
    let exe = parts
        .next()
        .ok_or_else(|| EdgeError::Config("RATHOLE_RELOAD_CMD is empty".into()))?;
    let args: Vec<&str> = parts.collect();
    let status = tokio::process::Command::new(exe)
        .args(&args)
        .status()
        .await?;
    if !status.success() {
        return Err(EdgeError::Config(format!(
            "rathole reload command exited non-zero: {status:?}"
        )));
    }
    tracing::info!(%cmd, "rathole reload command success");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_then_list_returns_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rathole.toml");
        let reg = ServiceRegistry::new(path.clone());

        reg.register(RatholeService {
            name: "sess1".into(),
            token_hash: "hash1".into(),
            bind_addr: "0.0.0.0:11001".into(),
        })
        .await
        .unwrap();

        let listed = reg.list().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "sess1");

        // File must exist and contain the service.
        let toml = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(toml.contains("sess1"));
        assert!(toml.contains("hash1"));
    }

    #[tokio::test]
    async fn deregister_removes_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rathole.toml");
        let reg = ServiceRegistry::new(path.clone());

        reg.register(RatholeService {
            name: "sess1".into(),
            token_hash: "hash1".into(),
            bind_addr: "0.0.0.0:11001".into(),
        })
        .await
        .unwrap();
        reg.deregister("sess1").await.unwrap();

        assert_eq!(reg.list().await.len(), 0);
        let toml = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(!toml.contains("sess1"));
    }
}
