//! Persist `session_id → subdomain` mapping to disk so the deprovision
//! handler can locate the registered Caddy route after a restart.
//!
//! Without persistence, edge-control restart with active sessions
//! would lose the in-memory `DashMap` and the subsequent DELETE
//! requests would silently skip Caddy cleanup, leaking routes until
//! the State Reconciliation Job (R22) catches up.
//!
//! Wire format mirrors `SequenceStore` in `edge-bandwidth`: one tiny
//! file per session at `<PERSISTENT_QUEUE_DIR>/subdomains/<session_id>`
//! containing the subdomain as ASCII. Atomic write (tmp + rename).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct SubdomainStore {
    root: PathBuf,
}

impl SubdomainStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Sanitise the session id to a path-safe filename. Mirrors the
    /// allowlist used in `SequenceStore` so the same id always maps to
    /// the same file across the two stores.
    fn session_path(&self, session_id: &str) -> PathBuf {
        let safe = session_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect::<String>();
        self.root.join("subdomains").join(safe)
    }

    pub async fn ensure_dir(&self) -> anyhow::Result<()> {
        let dir = self.root.join("subdomains");
        tokio::fs::create_dir_all(&dir).await?;
        Ok(())
    }

    pub async fn save(&self, session_id: &str, subdomain: &str) -> anyhow::Result<()> {
        let path = self.session_path(session_id);
        atomic_write(&path, subdomain).await
    }

    pub async fn remove(&self, session_id: &str) -> anyhow::Result<()> {
        let path = self.session_path(session_id);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    /// Bulk reload at boot. Skips empty/garbled files so a single
    /// corrupt entry never prevents the service from starting; the
    /// State Reconciliation Job will eventually heal the gap.
    pub async fn load_all(&self) -> anyhow::Result<HashMap<String, String>> {
        let dir = self.root.join("subdomains");
        if !dir.exists() {
            return Ok(HashMap::new());
        }
        let mut out = HashMap::new();
        let mut iter = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = iter.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.is_empty() {
                continue;
            }
            let contents = match tokio::fs::read_to_string(entry.path()).await {
                Ok(s) => s,
                Err(_) => continue,
            };
            let trimmed = contents.trim();
            if trimmed.is_empty() {
                continue;
            }
            out.insert(name, trimmed.to_string());
        }
        Ok(out)
    }
}

async fn atomic_write(path: &Path, contents: &str) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("subdomain path has no parent: {path:?}"))?;
    tokio::fs::create_dir_all(dir).await?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("subdomain path has no file name: {path:?}"))?
        .to_string_lossy()
        .into_owned();
    let tmp = dir.join(format!(".{file_name}.tmp"));
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
    if path.exists() {
        let _ = tokio::fs::remove_file(path).await;
    }
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubdomainStore::new(dir.path().to_path_buf());
        store.ensure_dir().await.unwrap();
        store.save("sess1", "abc.sin.dun-studio.xyz").await.unwrap();

        let map = store.load_all().await.unwrap();
        assert_eq!(map.get("sess1").map(String::as_str), Some("abc.sin.dun-studio.xyz"));
    }

    #[tokio::test]
    async fn remove_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubdomainStore::new(dir.path().to_path_buf());
        store.ensure_dir().await.unwrap();
        store.save("s", "x.dun-studio.xyz").await.unwrap();
        store.remove("s").await.unwrap();
        store.remove("s").await.unwrap();
        let map = store.load_all().await.unwrap();
        assert!(!map.contains_key("s"));
    }

    #[tokio::test]
    async fn load_all_skips_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubdomainStore::new(dir.path().to_path_buf());
        store.ensure_dir().await.unwrap();
        tokio::fs::write(dir.path().join("subdomains").join("empty"), "")
            .await
            .unwrap();
        store.save("good", "x.dun-studio.xyz").await.unwrap();
        let map = store.load_all().await.unwrap();
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("good"));
    }

    #[test]
    fn session_path_strips_traversal() {
        let store = SubdomainStore::new(PathBuf::from("/var/lib/foo"));
        let p = store.session_path("../../etc/passwd");
        assert!(!p.to_string_lossy().contains(".."));
    }
}
