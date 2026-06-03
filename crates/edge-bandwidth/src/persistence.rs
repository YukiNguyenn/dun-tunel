//! Persist bandwidth sequence counters to disk for restart safety
//! (browser-profile-public-tunnel R3.5).
//!
//! Wire format: one file per session at
//! `<PERSISTENT_QUEUE_DIR>/seq/<session_id>` with content `<sequence>`
//! as ASCII decimal. Atomic write (tmp + rename) so a crash mid-flush
//! never produces a partial value.
//!
//! Why per-file rather than a single ledger:
//! - Concurrent updates serialise per session — no global lock contention.
//! - File-level rename is atomic on POSIX.
//! - Easy to GC: when a session ends we just remove its file.
//!
//! Restart contract: on boot, scan the directory and rebuild the
//! per-session next-sequence map. Missing files (e.g. fresh deploy) are
//! treated as `0` so the first report uses sequence 0 and dun-api accepts
//! it (server-side `lastBandwidthSequence` defaults to `-1`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct SequenceStore {
    root: PathBuf,
}

impl SequenceStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn session_path(&self, session_id: &str) -> PathBuf {
        // Sanitise: refuse any session id that could break out of the dir.
        // session ids are MongoDB ObjectIds in practice (24 hex chars) so
        // a strict allowlist would be tighter — but we'd have to update
        // it whenever the id format changes. For Phase 2 we just reject
        // path-traversal characters.
        let safe = session_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect::<String>();
        self.root.join("seq").join(safe)
    }

    /// Ensure the seq directory exists. Idempotent.
    pub async fn ensure_dir(&self) -> anyhow::Result<()> {
        let dir = self.root.join("seq");
        tokio::fs::create_dir_all(&dir).await?;
        Ok(())
    }

    /// Write `sequence` for a session atomically. Uses tmp+rename so a
    /// crash never leaves a partial value on disk.
    pub async fn save(&self, session_id: &str, sequence: u64) -> anyhow::Result<()> {
        let path = self.session_path(session_id);
        atomic_write(&path, &sequence.to_string()).await
    }

    /// Read `sequence` for a session if present. Missing file → `None`.
    pub async fn load(&self, session_id: &str) -> anyhow::Result<Option<u64>> {
        let path = self.session_path(session_id);
        match tokio::fs::read_to_string(&path).await {
            Ok(contents) => match contents.trim().parse::<u64>() {
                Ok(n) => Ok(Some(n)),
                Err(_) => {
                    // Corrupt file (truncated, garbled). Treat as missing
                    // and let the reporter restart from 0; the dun-api
                    // dedup window absorbs a single replay.
                    tracing::warn!(%session_id, "sequence file corrupt, ignoring");
                    Ok(None)
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Drop the sequence file for an ended session.
    pub async fn remove(&self, session_id: &str) -> anyhow::Result<()> {
        let path = self.session_path(session_id);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    /// Bulk reload on boot. Returns `session_id → sequence` for every
    /// session file in the directory.
    pub async fn load_all(&self) -> anyhow::Result<HashMap<String, u64>> {
        let dir = self.root.join("seq");
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
            if let Ok(n) = contents.trim().parse::<u64>() {
                out.insert(name, n);
            }
        }
        Ok(out)
    }
}

async fn atomic_write(path: &Path, contents: &str) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("seq path has no parent: {path:?}"))?;
    tokio::fs::create_dir_all(dir).await?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("seq path has no file name: {path:?}"))?
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
        let store = SequenceStore::new(dir.path().to_path_buf());
        store.ensure_dir().await.unwrap();
        store.save("sess1", 42).await.unwrap();
        assert_eq!(store.load("sess1").await.unwrap(), Some(42));
    }

    #[tokio::test]
    async fn load_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = SequenceStore::new(dir.path().to_path_buf());
        assert_eq!(store.load("missing").await.unwrap(), None);
    }

    #[tokio::test]
    async fn remove_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = SequenceStore::new(dir.path().to_path_buf());
        store.ensure_dir().await.unwrap();
        store.save("s", 1).await.unwrap();
        store.remove("s").await.unwrap();
        store.remove("s").await.unwrap(); // no-op
        assert_eq!(store.load("s").await.unwrap(), None);
    }

    #[tokio::test]
    async fn load_all_skips_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let store = SequenceStore::new(dir.path().to_path_buf());
        store.ensure_dir().await.unwrap();
        store.save("good", 7).await.unwrap();
        // Drop a malformed file directly.
        tokio::fs::write(dir.path().join("seq").join("bad"), "not a number")
            .await
            .unwrap();
        let map = store.load_all().await.unwrap();
        assert_eq!(map.get("good").copied(), Some(7));
        assert!(!map.contains_key("bad"));
    }

    #[test]
    fn session_path_strips_traversal() {
        let store = SequenceStore::new(PathBuf::from("/var/lib/foo"));
        let p = store.session_path("../../etc/passwd");
        assert!(!p.to_string_lossy().contains(".."));
    }
}
