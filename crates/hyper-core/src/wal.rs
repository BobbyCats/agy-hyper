use crate::config::{sanitize_id, HyperConfig};
use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_POST_BLOB_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalFileRecord {
    pub abs_path: String,
    pub rel_path: String,
    pub existed_before: bool,
    pub etag_before: Option<String>,
    pub etag_after: String,
    pub backup_blob: Option<String>,
    #[serde(default)]
    pub post_blob: Option<String>,
    #[serde(default)]
    pub lines_added: usize,
    #[serde(default)]
    pub lines_removed: usize,
    pub mode_before: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxFileDiff {
    pub rel_path: String,
    #[serde(default)]
    pub abs_path: String,
    pub action: String,
    pub lines_added: usize,
    pub lines_removed: usize,
    pub unified_diff: String,
    pub truncated: bool,
    #[serde(default)]
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxDiffPayload {
    pub tx_id: String,
    pub timestamp_ms: u64,
    pub source: String,
    pub rolled_back: bool,
    pub files: Vec<TxFileDiff>,
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
    #[serde(default)]
    pub writer_id: Option<String>,
    pub files: Vec<WalFileRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteCollisionAlert {
    pub workspace_root: String,
    pub kind: String, // "same_file_race" | "shared_workspace_writers"
    pub rel_paths: Vec<String>,
    pub writer_count: usize,
    pub latest_ts_ms: u64,
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

pub fn format_epoch_ms_rfc3339(ts_ms: u64) -> String {
    let secs = (ts_ms / 1000) as i64;
    time::OffsetDateTime::from_unix_timestamp(secs)
        .ok()
        .and_then(|dt| dt.format(&time::format_description::well_known::Rfc3339).ok())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
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
        .truncate(false)
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
    #[cfg(not(unix))]
    let _ = mode;
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
    manifests.sort_by_key(|b| std::cmp::Reverse(b.timestamp_ms));
    manifests.truncate(limit.clamp(1, 200));
    manifests
}

pub fn count_diff_lines(before: &str, after: &str) -> (usize, usize) {
    let before_lf = before.replace("\r\n", "\n");
    let after_lf = after.replace("\r\n", "\n");
    let diff = TextDiff::from_lines(&before_lf, &after_lf);
    let (mut added, mut removed) = (0usize, 0usize);
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

/// Produce a line-level Unified Diff for a recorded WAL transaction (`hyper_edit` or native capture).
pub fn inspect_transaction_diff(
    wal_root: &Path,
    tx_id: &str,
    max_lines_per_file: usize,
) -> Result<TxDiffPayload> {
    let Some(clean_id) = sanitize_id(tx_id) else {
        return Err(anyhow!("invalid tx_id '{}'", tx_id));
    };
    if !clean_id.starts_with("tx_") {
        return Err(anyhow!("invalid tx_id '{}'", tx_id));
    }
    let tx_dir = wal_root.join(clean_id);
    let manifest_path = tx_dir.join("manifest.json");
    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("transaction '{}' not found in WAL", clean_id))?;
    let manifest: WalManifest = serde_json::from_str(&raw)?;
    let line_cap = max_lines_per_file.clamp(20, 600);

    let mut files = Vec::with_capacity(manifest.files.len());
    for rec in &manifest.files {
        let action = if rec.existed_before { "edited" } else { "created" }.to_string();
        let before_bytes = if rec.existed_before {
            match &rec.backup_blob {
                Some(blob) => fs::read(tx_dir.join(blob)).ok(),
                None => None,
            }
        } else {
            Some(Vec::new())
        };

        let after_bytes = if let Some(pb) = &rec.post_blob {
            fs::read(tx_dir.join(pb)).ok()
        } else {
            // Backwards-compatible fallback for pre-v0.4 WAL manifests: read live file iff ETag still matches.
            let live = PathBuf::from(&rec.abs_path);
            if live.is_file() && file_etag(&live).ok().as_deref() == Some(rec.etag_after.as_str()) {
                fs::read(&live).ok()
            } else {
                None
            }
        };

        let (Some(b_raw), Some(a_raw)) = (before_bytes, after_bytes) else {
            files.push(TxFileDiff {
                rel_path: rec.rel_path.clone(),
                abs_path: rec.abs_path.clone(),
                action,
                lines_added: rec.lines_added,
                lines_removed: rec.lines_removed,
                unified_diff: String::new(),
                truncated: false,
                unavailable_reason: Some(
                    "Historical snapshot predates post-image capture (pre-image still intact for rollback)"
                        .to_string(),
                ),
            });
            continue;
        };

        let (Ok(b_txt), Ok(a_txt)) = (String::from_utf8(b_raw), String::from_utf8(a_raw)) else {
            files.push(TxFileDiff {
                rel_path: rec.rel_path.clone(),
                abs_path: rec.abs_path.clone(),
                action,
                lines_added: rec.lines_added,
                lines_removed: rec.lines_removed,
                unified_diff: String::new(),
                truncated: false,
                unavailable_reason: Some("Binary or non-UTF-8 file".to_string()),
            });
            continue;
        };

        let before_lf = b_txt.replace("\r\n", "\n");
        let after_lf = a_txt.replace("\r\n", "\n");
        let diff = TextDiff::from_lines(&before_lf, &after_lf);
        let (mut added, mut removed) = (0usize, 0usize);
        for ch in diff.iter_all_changes() {
            match ch.tag() {
                ChangeTag::Insert => added += 1,
                ChangeTag::Delete => removed += 1,
                ChangeTag::Equal => {}
            }
        }
        let old_hdr = if rec.existed_before {
            format!("a/{}", rec.rel_path)
        } else {
            "/dev/null".to_string()
        };
        let new_hdr = format!("b/{}", rec.rel_path);
        let rendered = diff
            .unified_diff()
            .context_radius(3)
            .header(&old_hdr, &new_hdr)
            .to_string();
        let all_lines: Vec<&str> = rendered.lines().collect();
        let truncated = all_lines.len() > line_cap;
        let unified_diff = if truncated {
            all_lines[..line_cap].join("\n")
        } else {
            rendered.trim_end_matches('\n').to_string()
        };

        files.push(TxFileDiff {
            rel_path: rec.rel_path.clone(),
            abs_path: rec.abs_path.clone(),
            action,
            lines_added: added,
            lines_removed: removed,
            unified_diff,
            truncated,
            unavailable_reason: None,
        });
    }

    Ok(TxDiffPayload {
        tx_id: manifest.tx_id,
        timestamp_ms: manifest.timestamp_ms,
        source: manifest.source,
        rolled_back: manifest.rolled_back,
        files,
    })
}

pub fn detect_write_collisions(
    txs: &[WalManifest],
    now_ms_val: u64,
    window_ms: u64,
) -> Vec<WriteCollisionAlert> {
    use std::collections::{BTreeMap, BTreeSet};

    #[derive(Default)]
    struct WsActivity {
        latest_ts_ms: u64,
        recent_writers: BTreeSet<String>,
        recent_files: BTreeSet<String>,
        file_writers: BTreeMap<String, BTreeSet<String>>,
    }

    let shared_ws_window = window_ms.min(180_000);
    let mut by_ws: BTreeMap<String, WsActivity> = BTreeMap::new();

    for m in txs {
        if m.rolled_back || m.workspace_root.trim().is_empty() {
            continue;
        }
        let Some(writer) = m
            .writer_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let age_ms = now_ms_val.saturating_sub(m.timestamp_ms);
        if age_ms > window_ms {
            continue;
        }
        let entry = by_ws.entry(m.workspace_root.clone()).or_default();
        entry.latest_ts_ms = entry.latest_ts_ms.max(m.timestamp_ms);
        for f in &m.files {
            if f.rel_path.trim().is_empty() {
                continue;
            }
            entry
                .file_writers
                .entry(f.rel_path.clone())
                .or_default()
                .insert(writer.to_string());
            if age_ms <= shared_ws_window {
                entry.recent_files.insert(f.rel_path.clone());
            }
        }
        if age_ms <= shared_ws_window {
            entry.recent_writers.insert(writer.to_string());
        }
    }

    let mut alerts = Vec::new();
    for (ws, act) in by_ws {
        let mut raced_files = Vec::new();
        let mut race_writers = BTreeSet::new();
        for (rel, writers) in &act.file_writers {
            if writers.len() >= 2 {
                raced_files.push(rel.clone());
                for w in writers {
                    race_writers.insert(w.clone());
                }
            }
        }
        if !raced_files.is_empty() {
            alerts.push(WriteCollisionAlert {
                workspace_root: ws,
                kind: "same_file_race".into(),
                rel_paths: raced_files.into_iter().take(6).collect(),
                writer_count: race_writers.len(),
                latest_ts_ms: act.latest_ts_ms,
            });
        } else if act.recent_writers.len() >= 2 {
            alerts.push(WriteCollisionAlert {
                workspace_root: ws,
                kind: "shared_workspace_writers".into(),
                rel_paths: act.recent_files.into_iter().take(6).collect(),
                writer_count: act.recent_writers.len(),
                latest_ts_ms: act.latest_ts_ms,
            });
        }
    }
    alerts.sort_by_key(|b| std::cmp::Reverse(b.latest_ts_ms));
    alerts
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn inspect_diff_rejects_traversal_and_handles_legacy_fallback() {
        let wal = tempdir().unwrap();
        assert!(inspect_transaction_diff(wal.path(), "../etc/passwd", 100).is_err());
        assert!(inspect_transaction_diff(wal.path(), "not_a_tx", 100).is_err());

        // Create a legacy WAL entry (post_blob = None) with matching live file on disk.
        let ws = tempdir().unwrap();
        let live = ws.path().join("app.rs");
        fs::write(&live, "fn main() {\n    println!(\"v2\");\n}\n").unwrap();
        let after_etag = file_etag(&live).unwrap();

        let tx_id = "tx_1700000000000_abc123";
        let tx_dir = wal.path().join(tx_id);
        fs::create_dir_all(&tx_dir).unwrap();
        fs::write(tx_dir.join("pre_0.blob"), "fn main() {\n    println!(\"v1\");\n}\n").unwrap();
        let manifest = WalManifest {
            tx_id: tx_id.to_string(),
            timestamp_ms: 1700000000000,
            workspace_root: ws.path().display().to_string(),
            rolled_back: false,
            source: "hyper_edit".into(),
            writer_id: None,
            files: vec![WalFileRecord {
                abs_path: live.display().to_string(),
                rel_path: "app.rs".into(),
                existed_before: true,
                etag_before: Some("old".into()),
                etag_after: after_etag,
                backup_blob: Some("pre_0.blob".into()),
                post_blob: None,
                lines_added: 0,
                lines_removed: 0,
                mode_before: None,
            }],
        };
        fs::write(
            tx_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let diff = inspect_transaction_diff(wal.path(), tx_id, 100).unwrap();
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].lines_added, 1);
        assert_eq!(diff.files[0].lines_removed, 1);
        assert!(diff.files[0].unified_diff.contains("-    println!(\"v1\");"));
        assert!(diff.files[0].unified_diff.contains("+    println!(\"v2\");"));
    }

    #[test]
    fn detects_same_file_races_and_shared_workspace_writers() {
        let mk_tx = |id: &str, ts: u64, writer: Option<&str>, rel: &str, rolled_back: bool| WalManifest {
            tx_id: id.into(),
            timestamp_ms: ts,
            workspace_root: "/repo/demo".into(),
            rolled_back,
            source: "edit_file".into(),
            writer_id: writer.map(String::from),
            files: vec![WalFileRecord {
                abs_path: format!("/repo/demo/{}", rel),
                rel_path: rel.into(),
                existed_before: true,
                etag_before: Some("a".into()),
                etag_after: "b".into(),
                backup_blob: None,
                post_blob: None,
                lines_added: 1,
                lines_removed: 0,
                mode_before: None,
            }],
        };

        let clean = vec![
            mk_tx("tx1", 100_000, Some("conv-a"), "src/lib.rs", false),
            mk_tx("tx2", 110_000, Some("conv-a"), "src/main.rs", false),
        ];
        assert!(detect_write_collisions(&clean, 120_000, 300_000).is_empty());

        let shared = vec![
            mk_tx("tx1", 100_000, Some("conv-a"), "src/lib.rs", false),
            mk_tx("tx2", 120_000, Some("conv-b"), "src/main.rs", false),
        ];
        let alerts_shared = detect_write_collisions(&shared, 130_000, 300_000);
        assert_eq!(alerts_shared.len(), 1);
        assert_eq!(alerts_shared[0].kind, "shared_workspace_writers");
        assert_eq!(alerts_shared[0].writer_count, 2);

        let race = vec![
            mk_tx("tx1", 100_000, Some("conv-a"), "src/state.rs", false),
            mk_tx("tx2", 115_000, Some("conv-b"), "src/state.rs", false),
            mk_tx("tx3", 118_000, Some("conv-c"), "src/state.rs", true),
        ];
        let alerts_race = detect_write_collisions(&race, 125_000, 300_000);
        assert_eq!(alerts_race.len(), 1);
        assert_eq!(alerts_race[0].kind, "same_file_race");
        assert_eq!(alerts_race[0].writer_count, 2);
        assert_eq!(alerts_race[0].rel_paths, vec!["src/state.rs"]);
    }
}
