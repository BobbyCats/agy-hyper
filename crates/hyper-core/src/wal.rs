use crate::config::{sanitize_id, HyperConfig};
use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalFileRecord {
    pub abs_path: String,
    pub rel_path: String,
    pub existed_before: bool,
    pub etag_before: Option<String>,
    pub etag_after: String,
    pub backup_blob: Option<String>,
    pub mode_before: Option<u32>,
}

fn default_tx_source() -> String {
    "hyper_edit".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalManifest {
    pub tx_id: String,
    pub timestamp_ms: u64,
    pub workspace_root: String,
    pub rolled_back: bool,
    #[serde(default = "default_tx_source")]
    pub source: String,
    pub files: Vec<WalFileRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackResult {
    pub tx_id: String,
    pub restored_files: Vec<String>,
    pub removed_created_files: Vec<String>,
    pub skipped_or_conflicts: Vec<String>,
}

pub fn blake3_etag(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex()[..16].to_string()
}

pub fn file_etag(path: &Path) -> Result<String> {
    Ok(blake3_etag(&fs::read(path)?))
}

pub fn default_wal_root() -> PathBuf {
    HyperConfig::wal_dir()
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

static TX_SEQ: AtomicU64 = AtomicU64::new(0);

pub(crate) fn generate_tx_id() -> String {
    let ms = now_ms();
    let pid = std::process::id();
    let seq = TX_SEQ.fetch_add(1, Ordering::Relaxed);
    let seed = format!("{}:{}:{}:{:p}", ms, pid, seq, &ms as *const u64);
    let h = blake3::hash(seed.as_bytes());
    format!("tx_{}_{}", ms, &h.to_hex()[..6])
}

pub(crate) fn with_wal_lock<T>(
    wal_root: &Path,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    fs::create_dir_all(wal_root).context("create WAL root dir")?;
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(wal_root.join(".tx.lock"))
        .context("open transaction lock")?;
    lock_file
        .lock_exclusive()
        .context("acquire exclusive transaction lock")?;
    let res = f();
    let _ = lock_file.unlock();
    res
}

pub(crate) fn write_and_sync(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut f = File::create(path)?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}

pub(crate) fn atomic_restore_bytes(target: &Path, data: &[u8], mode: Option<u32>) -> Result<()> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let fname = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let tmp = parent.join(format!(
        ".agy-hyper.restore.{}.{}.tmp",
        fname,
        std::process::id()
    ));
    write_and_sync(&tmp, data)?;
    #[cfg(unix)]
    if let Some(m) = mode {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(m));
    }
    fs::rename(&tmp, target)?;
    Ok(())
}

pub(crate) fn prune_old_wal(wal_root: &Path, max_entries: usize) {
    let limit = max_entries.max(10);
    let Ok(entries) = fs::read_dir(wal_root) else {
        return;
    };
    let mut items: Vec<(u64, String, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            let raw = fs::read_to_string(path.join("manifest.json")).ok()?;
            let m: WalManifest = serde_json::from_str(&raw).ok()?;
            Some((m.timestamp_ms, m.tx_id, path))
        })
        .collect();

    if items.len() <= limit {
        return;
    }
    items.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let remove_count = items.len().saturating_sub(limit);
    for (_, _, oldest) in items.into_iter().take(remove_count) {
        let _ = fs::remove_dir_all(oldest);
    }
}

pub fn rollback_transaction(
    wal_root: &Path,
    tx_id: &str,
    force: bool,
) -> Result<RollbackResult> {
    let Some(clean_id) = sanitize_id(tx_id) else {
        return Err(anyhow!("invalid tx_id '{}'", tx_id));
    };
    if !clean_id.starts_with("tx_") {
        return Err(anyhow!("invalid tx_id '{}'", tx_id));
    }
    with_wal_lock(wal_root, || rollback_transaction_locked(wal_root, clean_id, force))
}

fn rollback_transaction_locked(
    wal_root: &Path,
    tx_id: &str,
    force: bool,
) -> Result<RollbackResult> {
    let tx_dir = wal_root.join(tx_id);
    let manifest_path = tx_dir.join("manifest.json");
    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("transaction '{}' not found in WAL", tx_id))?;
    let mut manifest: WalManifest = serde_json::from_str(&raw)?;

    let mut restored_files = Vec::new();
    let mut removed_created_files = Vec::new();
    let mut skipped_or_conflicts = Vec::new();

    for rec in &manifest.files {
        let target = PathBuf::from(&rec.abs_path);
        if !force && target.exists() {
            if let Ok(cur_etag) = file_etag(&target) {
                if cur_etag != rec.etag_after {
                    skipped_or_conflicts.push(format!(
                        "{} (modified since {}; pass force=true to override)",
                        rec.rel_path, tx_id
                    ));
                    continue;
                }
            }
        }

        if rec.existed_before {
            let Some(blob_name) = &rec.backup_blob else {
                skipped_or_conflicts.push(format!("{} (missing backup blob)", rec.rel_path));
                continue;
            };
            let orig = fs::read(tx_dir.join(blob_name))
                .with_context(|| format!("read backup blob for {}", rec.rel_path))?;
            atomic_restore_bytes(&target, &orig, rec.mode_before)?;
            restored_files.push(rec.rel_path.clone());
        } else if target.exists() {
            fs::remove_file(&target)?;
            removed_created_files.push(rec.rel_path.clone());
        }
    }

    if skipped_or_conflicts.is_empty() {
        manifest.rolled_back = true;
        let _ = write_and_sync(&manifest_path, &serde_json::to_vec_pretty(&manifest)?);
    }

    Ok(RollbackResult {
        tx_id: tx_id.to_string(),
        restored_files,
        removed_created_files,
        skipped_or_conflicts,
    })
}

pub fn list_transactions(wal_root: &Path, limit: usize) -> Vec<WalManifest> {
    let Ok(entries) = fs::read_dir(wal_root) else {
        return Vec::new();
    };
    let mut manifests: Vec<WalManifest> = entries
        .flatten()
        .filter_map(|entry| {
            let raw = fs::read_to_string(entry.path().join("manifest.json")).ok()?;
            serde_json::from_str::<WalManifest>(&raw).ok()
        })
        .collect();
    manifests.sort_by(|a, b| b.timestamp_ms.cmp(&a.timestamp_ms));
    manifests.truncate(limit.clamp(1, 200));
    manifests
}
