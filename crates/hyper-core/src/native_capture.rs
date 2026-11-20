use crate::jail::WorkspaceJail;
use crate::json_util::{json_arg_bool, json_arg_str, unquote_json_string};
use crate::wal::{
    blake3_etag, generate_tx_id, now_ms, prune_old_wal, with_wal_lock, write_and_sync,
    WalFileRecord, WalManifest,
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8192)].contains(&0)
}

fn is_internal_path(path: &Path) -> bool {
    path.components().any(|comp| {
        let seg = comp.as_os_str().to_string_lossy();
        matches!(seg.as_ref(), ".gemini" | ".git" | "node_modules")
    })
}

fn find_containing_workspace_root(abs_file: &Path, workspace_roots: &[PathBuf]) -> Option<PathBuf> {
    let ws = workspace_roots
        .iter()
        .filter_map(|raw| fs::canonicalize(raw).ok())
        .filter(|canon| abs_file.starts_with(canon))
        .max_by_key(|canon| canon.components().count())?;

    let mut cur = abs_file.parent();
    while let Some(dir) = cur {
        if !dir.starts_with(&ws) {
            break;
        }
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        if dir == ws {
            break;
        }
        cur = dir.parent();
    }
    Some(ws)
}

fn match_raw_or_unquoted<'a>(raw: &'a str, pred: impl Fn(&str) -> Option<Vec<u8>>) -> Option<Vec<u8>> {
    if let Some(res) = pred(raw) {
        return Some(res);
    }
    let unq = unquote_json_string(raw);
    if unq != raw {
        return pred(&unq);
    }
    None
}

fn reconstruct_unique_edit_preimage(
    after_text: &str,
    raw_target: &str,
    raw_repl: &str,
) -> Option<String> {
    let try_pair = |target: &str, repl: &str| -> Option<String> {
        if repl.is_empty() || target == repl || after_text.matches(repl).count() != 1 {
            return None;
        }
        let before = after_text.replacen(repl, target, 1);
        (before.replace(target, repl) == after_text).then_some(before)
    };

    if let Some(before) = try_pair(raw_target, raw_repl) {
        return Some(before);
    }
    let unq_t = unquote_json_string(raw_target);
    let unq_r = unquote_json_string(raw_repl);
    if unq_t != raw_target || unq_r != raw_repl {
        return try_pair(&unq_t, &unq_r);
    }
    None
}

/// Captures mathematically verified, byte-exact pre-images from Antigravity's built-in
/// `edit_file` and `write_file` tools inside `PostToolUse` (zero `PreToolUse` impact).
pub fn record_native_post_edit(
    wal_root: &Path,
    max_wal_entries: usize,
    workspace_roots: &[PathBuf],
    tool_name: &str,
    args: &Value,
    block_secret_files: bool,
) -> Result<Option<WalManifest>> {
    if !matches!(tool_name, "edit_file" | "write_file") {
        return Ok(None);
    }
    let Some(raw_path) = json_arg_str(args, &["Path", "path", "TargetFile", "file_path"]) else {
        return Ok(None);
    };
    let clean_path = unquote_json_string(raw_path);
    if clean_path.is_empty() {
        return Ok(None);
    }

    let Ok(abs_path) = fs::canonicalize(&clean_path) else {
        return Ok(None);
    };
    if is_internal_path(&abs_path) {
        return Ok(None);
    }

    let Some(ws_root) = find_containing_workspace_root(&abs_path, workspace_roots) else {
        return Ok(None);
    };
    let Ok(jail) = WorkspaceJail::new(&ws_root, false, block_secret_files) else {
        return Ok(None);
    };
    let Ok(verified_abs) = jail.resolve_existing(&abs_path) else {
        return Ok(None);
    };

    let meta = fs::metadata(&verified_abs)?;
    if !meta.is_file() || meta.len() > 16 * 1024 * 1024 {
        return Ok(None);
    }
    let after_bytes = fs::read(&verified_abs)?;
    if looks_binary(&after_bytes) {
        return Ok(None);
    }
    let etag_after = blake3_etag(&after_bytes);
    #[cfg(unix)]
    let mode_after = {
        use std::os::unix::fs::PermissionsExt;
        Some(meta.permissions().mode())
    };
    #[cfg(not(unix))]
    let mode_after = None;

    let (existed_before, pre_bytes): (bool, Option<Vec<u8>>) = match tool_name {
        "edit_file" => {
            let Ok(after_text) = String::from_utf8(after_bytes.clone()) else {
                return Ok(None);
            };
            let Some(raw_target) = json_arg_str(args, &["Target", "target", "old_text"]) else {
                return Ok(None);
            };
            let Some(raw_repl) = json_arg_str(args, &["Replacement", "replacement", "new_text"]) else {
                return Ok(None);
            };
            let Some(before_text) = reconstruct_unique_edit_preimage(&after_text, raw_target, raw_repl) else {
                return Ok(None);
            };
            (true, Some(before_text.into_bytes()))
        }
        "write_file" => {
            let overwrite = json_arg_bool(args, &["Overwrite", "overwrite"]);
            let append = json_arg_bool(args, &["Append", "append"]);
            if overwrite {
                return Ok(None);
            }
            let Some(raw_content) = json_arg_str(args, &["Content", "content"]) else {
                return Ok(None);
            };
            if !append {
                // Verify on-disk bytes actually match `Content` so a failed write_file on an
                // already-existing file can never be misrecorded as a brand-new creation.
                let exact_match = after_bytes == raw_content.as_bytes()
                    || after_bytes == unquote_json_string(raw_content).as_bytes();
                if !exact_match {
                    return Ok(None);
                }
                (false, None)
            } else {
                let Some(prefix) = match_raw_or_unquoted(raw_content, |c| {
                    (!c.is_empty()
                        && after_bytes.len() > c.len()
                        && after_bytes.ends_with(c.as_bytes()))
                    .then(|| after_bytes[..after_bytes.len() - c.len()].to_vec())
                }) else {
                    return Ok(None);
                };
                (true, Some(prefix))
            }
        }
        _ => return Ok(None),
    };

    with_wal_lock(wal_root, || {
        let tx_id = generate_tx_id();
        let timestamp_ms = now_ms();
        let tx_dir = wal_root.join(&tx_id);
        fs::create_dir_all(&tx_dir).context("create WAL tx directory")?;

        let (etag_before, backup_blob) = if let Some(orig) = &pre_bytes {
            let blob_name = "pre_0.blob".to_string();
            write_and_sync(&tx_dir.join(&blob_name), orig)?;
            (Some(blake3_etag(orig)), Some(blob_name))
        } else {
            (None, None)
        };

        let manifest = WalManifest {
            tx_id: tx_id.clone(),
            timestamp_ms,
            workspace_root: jail.root().display().to_string(),
            rolled_back: false,
            source: tool_name.to_string(),
            files: vec![WalFileRecord {
                abs_path: verified_abs.display().to_string(),
                rel_path: jail.relative_display(&verified_abs),
                existed_before,
                etag_before,
                etag_after,
                backup_blob,
                mode_before: mode_after,
            }],
        };
        write_and_sync(
            &tx_dir.join("manifest.json"),
            &serde_json::to_vec_pretty(&manifest)?,
        )?;
        prune_old_wal(wal_root, max_wal_entries);
        Ok(Some(manifest))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::rollback_transaction;
    use tempfile::tempdir;

    #[test]
    fn captures_unique_edit_and_rolls_back_byte_exact() {
        let ws = tempdir().unwrap();
        let wal = tempdir().unwrap();
        let file = ws.path().join("service.rs");
        let before = "pub fn retry_limit() -> usize {\n    3\n}\n";
        let after = "pub fn retry_limit() -> usize {\n    5\n}\n";
        fs::write(&file, after).unwrap();

        let args = serde_json::json!({
            "Path": file.display().to_string(),
            "Target": "    3\n}",
            "Replacement": "    5\n}"
        });
        let manifest = record_native_post_edit(
            wal.path(),
            50,
            &[ws.path().to_path_buf()],
            "edit_file",
            &args,
            true,
        )
        .unwrap()
        .expect("should capture unique edit_file");

        assert_eq!(manifest.source, "edit_file");
        let rb = rollback_transaction(wal.path(), &manifest.tx_id, false).unwrap();
        assert_eq!(rb.restored_files, vec!["service.rs"]);
        assert_eq!(fs::read_to_string(&file).unwrap(), before);
    }

    #[test]
    fn skips_ambiguous_duplicate_replacement_and_failed_write_mismatch() {
        let ws = tempdir().unwrap();
        let wal = tempdir().unwrap();
        let dup = ws.path().join("dup.rs");
        fs::write(&dup, "let a = 1;\nlet b = 1;\n").unwrap();
        assert!(record_native_post_edit(
            wal.path(),
            50,
            &[ws.path().to_path_buf()],
            "edit_file",
            &serde_json::json!({"Path": dup.display().to_string(), "Target": "0", "Replacement": "1"}),
            true,
        )
        .unwrap()
        .is_none());

        // Regression: failed write_file on pre-existing file (bytes != Content) must NEVER record existed_before=false!
        let existing = ws.path().join("precious.rs");
        fs::write(&existing, "ORIGINAL_CONTENT\n").unwrap();
        assert!(record_native_post_edit(
            wal.path(),
            50,
            &[ws.path().to_path_buf()],
            "write_file",
            &serde_json::json!({"Path": existing.display().to_string(), "Content": "DIFFERENT_CONTENT\n"}),
            true,
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn captures_write_create_append_and_handles_double_encoded_args() {
        let ws = tempdir().unwrap();
        let wal = tempdir().unwrap();

        let created = ws.path().join("notes.md");
        fs::write(&created, "# Hello\n").unwrap();
        let m1 = record_native_post_edit(
            wal.path(),
            50,
            &[ws.path().to_path_buf()],
            "write_file",
            &serde_json::json!({"Path": created.display().to_string(), "Content": "# Hello\n"}),
            true,
        )
        .unwrap()
        .expect("should record verified brand-new file");
        assert!(!m1.files[0].existed_before);

        fs::write(&created, "# Hello\nline2\n").unwrap();
        let m2 = record_native_post_edit(
            wal.path(),
            50,
            &[ws.path().to_path_buf()],
            "write_file",
            &serde_json::json!({"Path": created.display().to_string(), "Content": "line2\n", "Append": true}),
            true,
        )
        .unwrap()
        .expect("should record append");
        assert!(m2.files[0].existed_before);

        rollback_transaction(wal.path(), &m2.tx_id, false).unwrap();
        assert_eq!(fs::read_to_string(&created).unwrap(), "# Hello\n");
        rollback_transaction(wal.path(), &m1.tx_id, false).unwrap();
        assert!(!created.exists());

        // Double-encoded args
        let cfg = ws.path().join("cfg.rs");
        fs::write(&cfg, "const MODE: &str = \"hyper\";\n").unwrap();
        let m3 = record_native_post_edit(
            wal.path(),
            50,
            &[ws.path().to_path_buf()],
            "edit_file",
            &serde_json::json!({
                "Path": serde_json::to_string(&cfg.display().to_string()).unwrap(),
                "Target": serde_json::to_string("\"balanced\"").unwrap(),
                "Replacement": serde_json::to_string("\"hyper\"").unwrap()
            }),
            true,
        )
        .unwrap()
        .expect("should unwrap double-encoded args");
        rollback_transaction(wal.path(), &m3.tx_id, false).unwrap();
        assert_eq!(fs::read_to_string(&cfg).unwrap(), "const MODE: &str = \"balanced\";\n");
    }

    #[test]
    fn skips_secrets_overwrite_and_internal_gemini_dir() {
        let ws = tempdir().unwrap();
        let wal = tempdir().unwrap();

        let env_file = ws.path().join(".env");
        fs::write(&env_file, "TOKEN=new\n").unwrap();
        assert!(record_native_post_edit(
            wal.path(),
            50,
            &[ws.path().to_path_buf()],
            "edit_file",
            &serde_json::json!({"Path": env_file.display().to_string(), "Target": "old", "Replacement": "new"}),
            true,
        )
        .unwrap()
        .is_none());

        let doc = ws.path().join("doc.md");
        fs::write(&doc, "replaced\n").unwrap();
        assert!(record_native_post_edit(
            wal.path(),
            50,
            &[ws.path().to_path_buf()],
            "write_file",
            &serde_json::json!({"Path": doc.display().to_string(), "Content": "replaced\n", "Overwrite": true}),
            true,
        )
        .unwrap()
        .is_none());

        let gem_dir = ws.path().join(".gemini/antigravity/brain/abc");
        fs::create_dir_all(&gem_dir).unwrap();
        let plan = gem_dir.join("plan.md");
        fs::write(&plan, "# Plan\n").unwrap();
        assert!(record_native_post_edit(
            wal.path(),
            50,
            &[ws.path().to_path_buf()],
            "write_file",
            &serde_json::json!({"Path": plan.display().to_string(), "Content": "# Plan\n"}),
            true,
        )
        .unwrap()
        .is_none());
    }
}
