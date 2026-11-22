use crate::ast::{check_syntax, find_symbol, SyntaxDiagnostic};
use crate::jail::WorkspaceJail;
use crate::wal::{
    atomic_restore_bytes, generate_tx_id, now_ms, prune_old_wal, with_wal_lock, write_and_sync,
    WalFileRecord, WalManifest, MAX_POST_BLOB_BYTES,
};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use similar::TextDiff;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub use crate::native_capture::record_native_post_edit;
pub use crate::wal::{
    blake3_etag, default_wal_root, file_etag, inspect_transaction_diff, list_transactions,
    rollback_transaction, RollbackResult, TxDiffPayload, TxFileDiff, WalFileRecord as WalRecord,
    WalManifest as TxWalManifest,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEditOp {
    pub path: String,
    #[serde(default)]
    pub old_text: Option<String>,
    #[serde(default)]
    pub new_text: Option<String>,
    /// Optional AST symbol targeting (e.g. "AuthService::verify" or "handle_request").
    #[serde(default)]
    pub symbol: Option<String>,
    /// Optional full-file create content when creating a brand-new file.
    #[serde(default)]
    pub create_content: Option<String>,
    /// Optional optimistic lock: BLAKE3 hex prefix from prior read/edit.
    #[serde(default)]
    pub expected_etag: Option<String>,
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEditOutcome {
    pub path: String,
    pub action: String,
    pub etag_before: Option<String>,
    pub etag_after: String,
    pub lines_added: usize,
    pub lines_removed: usize,
    pub diff_preview: String,
    pub syntax_errors: Vec<SyntaxDiagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxCommitResult {
    pub tx_id: String,
    pub timestamp_ms: u64,
    pub workspace_root: String,
    pub committed: bool,
    pub files: Vec<FileEditOutcome>,
    pub total_syntax_errors: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineEnding {
    Lf,
    Crlf,
}

fn detect_majority_eol(text: &str) -> LineEnding {
    let bytes = text.as_bytes();
    let (mut crlf, mut lf) = (0usize, 0usize);
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            if i > 0 && bytes[i - 1] == b'\r' {
                crlf += 1;
            } else {
                lf += 1;
            }
        }
    }
    if crlf > lf { LineEnding::Crlf } else { LineEnding::Lf }
}

fn normalize_to_lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

fn apply_eol(s: &str, eol: LineEnding) -> String {
    let lf = normalize_to_lf(s);
    match eol {
        LineEnding::Lf => lf,
        LineEnding::Crlf => lf.replace('\n', "\r\n"),
    }
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8192)].contains(&0)
}

struct PlannedWrite {
    abs_path: PathBuf,
    rel_path: String,
    existed_before: bool,
    original_bytes: Option<Vec<u8>>,
    original_mode: Option<u32>,
    etag_before: Option<String>,
    new_bytes: Vec<u8>,
    etag_after: String,
    diff_preview: String,
    lines_added: usize,
    lines_removed: usize,
    syntax_errors: Vec<SyntaxDiagnostic>,
}

/// Crash-safe, WAL-journaled, ETag-CAS multi-file atomic commit.
pub fn apply_transactional_edits(
    jail: &WorkspaceJail,
    ops: &[FileEditOp],
    wal_root: &Path,
    max_wal_entries: usize,
) -> Result<TxCommitResult> {
    if ops.is_empty() {
        return Err(anyhow!("no edits provided"));
    }
    if ops.len() > 64 {
        return Err(anyhow!("batch too large (max 64 ops per transaction)"));
    }
    with_wal_lock(wal_root, || {
        apply_transactional_edits_locked(jail, ops, wal_root, max_wal_entries)
    })
}

fn apply_transactional_edits_locked(
    jail: &WorkspaceJail,
    ops: &[FileEditOp],
    wal_root: &Path,
    max_wal_entries: usize,
) -> Result<TxCommitResult> {
    let mut ordered_paths: Vec<PathBuf> = Vec::new();
    let mut grouped: HashMap<PathBuf, Vec<&FileEditOp>> = HashMap::new();

    for op in ops {
        let abs = jail
            .resolve_write(&op.path)
            .with_context(|| format!("workspace jail rejected path '{}'", op.path))?;
        if !grouped.contains_key(&abs) {
            ordered_paths.push(abs.clone());
        }
        grouped.entry(abs).or_default().push(op);
    }

    let mut plans: Vec<PlannedWrite> = Vec::with_capacity(ordered_paths.len());

    for abs_path in &ordered_paths {
        let rel_path = jail.relative_display(abs_path);
        let file_ops = &grouped[abs_path];
        let existed_before = abs_path.exists();

        let (original_bytes, original_mode, etag_before, has_bom, eol, mut working_lf) =
            if existed_before {
                let meta = fs::metadata(abs_path).with_context(|| format!("stat {}", rel_path))?;
                if !meta.is_file() {
                    return Err(anyhow!("target '{}' is not a regular file", rel_path));
                }
                if meta.len() > 16 * 1024 * 1024 {
                    return Err(anyhow!("target '{}' exceeds 16 MiB safety limit", rel_path));
                }
                let raw = fs::read(abs_path).with_context(|| format!("read {}", rel_path))?;
                if looks_binary(&raw) {
                    return Err(anyhow!("refusing to edit binary file '{}'", rel_path));
                }
                let text = String::from_utf8(raw.clone()).map_err(|_| {
                    anyhow!(
                        "refusing to edit non-UTF-8 file '{}' (prevents silent mojibake corruption)",
                        rel_path
                    )
                })?;
                let etag = blake3_etag(&raw);
                let has_bom = text.starts_with('\u{feff}');
                let body = text.strip_prefix('\u{feff}').unwrap_or(&text);
                let eol = detect_majority_eol(body);
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::PermissionsExt;
                    Some(meta.permissions().mode())
                };
                #[cfg(not(unix))]
                let mode = None;
                (Some(raw), mode, Some(etag), has_bom, eol, normalize_to_lf(body))
            } else {
                (None, None, None, false, LineEnding::Lf, String::new())
            };

        for op in file_ops {
            if let Some(exp) = op.expected_etag.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                match &etag_before {
                    Some(actual) if actual.starts_with(exp) || exp.starts_with(actual.as_str()) => {}
                    Some(actual) => {
                        return Err(anyhow!(
                            "ETag CAS conflict on '{}': expected '{}', actual '{}' (file modified concurrently; re-read before editing)",
                            rel_path, exp, actual
                        ));
                    }
                    None => {
                        return Err(anyhow!(
                            "ETag CAS conflict on '{}': expected '{}', but file does not exist",
                            rel_path, exp
                        ));
                    }
                }
            }
        }

        let original_lf = working_lf.clone();

        for (i, op) in file_ops.iter().enumerate() {
            if let Some(content) = &op.create_content {
                if existed_before {
                    return Err(anyhow!(
                        "file '{}' already exists; use old_text/symbol to edit instead of create_content",
                        rel_path
                    ));
                }
                working_lf = normalize_to_lf(content);
                continue;
            }
            if !existed_before {
                return Err(anyhow!(
                    "file '{}' does not exist; pass create_content to create a new file",
                    rel_path
                ));
            }
            if let Some(sym_query) = &op.symbol {
                let replacement = op.new_text.as_deref().ok_or_else(|| {
                    anyhow!("edit #{} on '{}' with symbol requires new_text", i + 1, rel_path)
                })?;
                let sym = find_symbol(abs_path, &working_lf, sym_query)
                    .map_err(|e| anyhow!("symbol target failed on '{}': {}", rel_path, e))?;
                working_lf.replace_range(sym.start_byte..sym.end_byte, &normalize_to_lf(replacement));
                continue;
            }

            let old = op.old_text.as_deref().ok_or_else(|| {
                anyhow!(
                    "edit #{} on '{}' requires either old_text, symbol, or create_content",
                    i + 1, rel_path
                )
            })?;
            let new = op.new_text.as_deref().ok_or_else(|| {
                anyhow!("edit #{} on '{}' requires new_text", i + 1, rel_path)
            })?;
            let old_lf = normalize_to_lf(old);
            let new_lf = normalize_to_lf(new);
            if old_lf.is_empty() {
                return Err(anyhow!("edit #{} on '{}' has empty old_text", i + 1, rel_path));
            }
            let match_count = working_lf.matches(&old_lf).count();
            if match_count == 0 {
                return Err(anyhow!(
                    "old_text not found in '{}' (exact match failed; check indentation or re-read file)",
                    rel_path
                ));
            }
            if match_count > 1 && !op.replace_all {
                return Err(anyhow!(
                    "old_text matched {} times in '{}'; include more surrounding context or set replace_all=true",
                    match_count, rel_path
                ));
            }
            working_lf = if op.replace_all {
                working_lf.replace(&old_lf, &new_lf)
            } else {
                working_lf.replacen(&old_lf, &new_lf, 1)
            };
        }

        let mut final_text = apply_eol(&working_lf, eol);
        if has_bom {
            final_text.insert(0, '\u{feff}');
        }
        let new_bytes = final_text.as_bytes().to_vec();
        let etag_after = blake3_etag(&new_bytes);
        let (diff_preview, lines_added, lines_removed) =
            compact_diff_preview(&original_lf, &working_lf, 18);
        let syntax_errors = check_syntax(abs_path, &final_text);

        plans.push(PlannedWrite {
            abs_path: abs_path.clone(),
            rel_path,
            existed_before,
            original_bytes,
            original_mode,
            etag_before,
            new_bytes,
            etag_after,
            diff_preview,
            lines_added,
            lines_removed,
            syntax_errors,
        });
    }

    // Phase 1: Write and fsync WAL pre-image snapshot BEFORE mutating workspace files.
    let tx_id = generate_tx_id();
    let timestamp_ms = now_ms();
    let tx_dir = wal_root.join(&tx_id);
    fs::create_dir_all(&tx_dir).context("create WAL tx directory")?;

    let mut wal_records = Vec::with_capacity(plans.len());
    for (idx, plan) in plans.iter().enumerate() {
        let backup_blob = if let Some(orig) = &plan.original_bytes {
            let blob_name = format!("pre_{}.blob", idx);
            write_and_sync(&tx_dir.join(&blob_name), orig)?;
            Some(blob_name)
        } else {
            None
        };
        let post_blob = if plan.new_bytes.len() <= MAX_POST_BLOB_BYTES {
            let blob_name = format!("post_{}.blob", idx);
            write_and_sync(&tx_dir.join(&blob_name), &plan.new_bytes)?;
            Some(blob_name)
        } else {
            None
        };
        wal_records.push(WalFileRecord {
            abs_path: plan.abs_path.display().to_string(),
            rel_path: plan.rel_path.clone(),
            existed_before: plan.existed_before,
            etag_before: plan.etag_before.clone(),
            etag_after: plan.etag_after.clone(),
            backup_blob,
            post_blob,
            lines_added: plan.lines_added,
            lines_removed: plan.lines_removed,
            mode_before: plan.original_mode,
        });
    }

    let manifest = WalManifest {
        tx_id: tx_id.clone(),
        timestamp_ms,
        workspace_root: jail.root().display().to_string(),
        rolled_back: false,
        source: "hyper_edit".into(),
        files: wal_records,
    };
    write_and_sync(
        &tx_dir.join("manifest.json"),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;

    // Phase 2: Stage temp files in target dirs with sync_all(); abort cleanly on ENOSPC/EACCES.
    let mut staged_temps: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(plans.len());
    for (idx, plan) in plans.iter().enumerate() {
        let parent = plan.abs_path.parent().unwrap_or_else(|| Path::new("."));
        if !parent.exists() {
            if let Err(e) = fs::create_dir_all(parent) {
                for (t, _) in &staged_temps {
                    let _ = fs::remove_file(t);
                }
                let _ = fs::remove_dir_all(&tx_dir);
                return Err(anyhow!("failed to create parent dir for '{}': {}", plan.rel_path, e));
            }
        }
        let file_name = plan
            .abs_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file");
        let tmp_path = parent.join(format!(".agy-hyper.{}.{}.{}.tmp", file_name, tx_id, idx));

        if let Err(e) = write_and_sync(&tmp_path, &plan.new_bytes) {
            let _ = fs::remove_file(&tmp_path);
            for (t, _) in &staged_temps {
                let _ = fs::remove_file(t);
            }
            let _ = fs::remove_dir_all(&tx_dir);
            return Err(anyhow!(
                "atomic stage failed for '{}' ({}); ZERO files were modified or truncated",
                plan.rel_path, e
            ));
        }

        #[cfg(unix)]
        if let Some(mode) = plan.original_mode {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp_path, fs::Permissions::from_mode(mode));
        }
        staged_temps.push((tmp_path, plan.abs_path.clone()));
    }

    // Phase 3: Atomic rename commit with lossless recovery.
    let mut committed_indices: Vec<usize> = Vec::with_capacity(staged_temps.len());
    for (idx, (tmp_path, target_path)) in staged_temps.iter().enumerate() {
        if let Err(e) = fs::rename(tmp_path, target_path) {
            for (rem_tmp, _) in &staged_temps[idx..] {
                let _ = fs::remove_file(rem_tmp);
            }
            let mut all_restored = true;
            for &done_idx in committed_indices.iter().rev() {
                let p = &plans[done_idx];
                let ok = if let Some(orig) = &p.original_bytes {
                    atomic_restore_bytes(&p.abs_path, orig, p.original_mode).is_ok()
                } else {
                    fs::remove_file(&p.abs_path).is_ok()
                };
                if !ok {
                    all_restored = false;
                }
            }
            // Only remove WAL snapshot if every earlier file restored cleanly; otherwise preserve WAL for manual recovery.
            if all_restored {
                let _ = fs::remove_dir_all(&tx_dir);
            }
            return Err(anyhow!(
                "atomic rename failed on '{}' ({}); rolled back {} earlier file(s){}",
                plans[idx].rel_path,
                e,
                committed_indices.len(),
                if all_restored { "" } else { " (WAL preserved for recovery)" }
            ));
        }
        committed_indices.push(idx);
    }

    prune_old_wal(wal_root, max_wal_entries);

    let mut total_syntax_errors = 0usize;
    let outcomes: Vec<FileEditOutcome> = plans
        .into_iter()
        .map(|p| {
            total_syntax_errors += p.syntax_errors.len();
            FileEditOutcome {
                path: p.rel_path,
                action: if p.existed_before { "edited" } else { "created" }.into(),
                etag_before: p.etag_before,
                etag_after: p.etag_after,
                lines_added: p.lines_added,
                lines_removed: p.lines_removed,
                diff_preview: p.diff_preview,
                syntax_errors: p.syntax_errors,
            }
        })
        .collect();

    Ok(TxCommitResult {
        tx_id,
        timestamp_ms,
        workspace_root: jail.root().display().to_string(),
        committed: true,
        files: outcomes,
        total_syntax_errors,
    })
}

fn compact_diff_preview(before: &str, after: &str, max_lines: usize) -> (String, usize, usize) {
    let diff = TextDiff::from_lines(before, after);
    let (mut added, mut removed) = (0usize, 0usize);
    let mut lines: Vec<String> = Vec::new();
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => {
                added += 1;
                if lines.len() < max_lines {
                    lines.push(format!("+{}", change.value().trim_end_matches('\n')));
                }
            }
            similar::ChangeTag::Delete => {
                removed += 1;
                if lines.len() < max_lines {
                    lines.push(format!("-{}", change.value().trim_end_matches('\n')));
                }
            }
            similar::ChangeTag::Equal => {}
        }
    }
    if added + removed > max_lines {
        lines.push(format!("… ({} more changed lines)", (added + removed) - max_lines));
    }
    (lines.join("\n"), added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn atomic_edit_preserves_bom_crlf_checks_syntax_and_rolls_back() {
        let ws = tempdir().unwrap();
        let wal = tempdir().unwrap();
        let file = ws.path().join("demo.ts");
        let initial = "\u{feff}export function greet(name: string): string {\r\n  return `hi ${name}`;\r\n}\r\n";
        fs::write(&file, initial.as_bytes()).unwrap();

        let jail = WorkspaceJail::new(ws.path(), false, true).unwrap();
        let etag = file_etag(&file).unwrap();

        let tx = apply_transactional_edits(
            &jail,
            &[FileEditOp {
                path: "demo.ts".into(),
                old_text: Some("return `hi ${name}`;".into()),
                new_text: Some("return `hello ${name}!`;".into()),
                symbol: None,
                create_content: None,
                expected_etag: Some(etag),
                replace_all: false,
            }],
            wal.path(),
            50,
        )
        .unwrap();

        assert_eq!(tx.total_syntax_errors, 0);
        let updated = fs::read_to_string(&file).unwrap();
        assert!(updated.starts_with('\u{feff}'));
        assert!(updated.contains("\r\n"));
        assert!(updated.contains("hello ${name}!"));

        let rb = rollback_transaction(wal.path(), &tx.tx_id, false).unwrap();
        assert_eq!(rb.restored_files, vec!["demo.ts"]);
        assert_eq!(fs::read_to_string(&file).unwrap(), initial);
    }

    #[test]
    fn etag_mismatch_and_create_content_on_existing_file_abort_cleanly() {
        let ws = tempdir().unwrap();
        let wal = tempdir().unwrap();
        let file = ws.path().join("x.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        let jail = WorkspaceJail::new(ws.path(), false, true).unwrap();

        let err = apply_transactional_edits(
            &jail,
            &[FileEditOp {
                path: "x.rs".into(),
                old_text: Some("main".into()),
                new_text: Some("start".into()),
                symbol: None,
                create_content: None,
                expected_etag: Some("deadbeef1234".into()),
                replace_all: false,
            }],
            wal.path(),
            50,
        )
        .unwrap_err();
        assert!(err.to_string().contains("ETag CAS conflict"));

        // Regression: create_content on existing file must ALWAYS fail even when paired with old_text.
        let err2 = apply_transactional_edits(
            &jail,
            &[FileEditOp {
                path: "x.rs".into(),
                old_text: Some("main".into()),
                new_text: None,
                symbol: None,
                create_content: Some("wiped".into()),
                expected_etag: None,
                replace_all: false,
            }],
            wal.path(),
            50,
        )
        .unwrap_err();
        assert!(err2.to_string().contains("already exists"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "fn main() {}\n");
    }

    #[test]
    fn symbol_target_edit_replaces_exact_ast_span() {
        let ws = tempdir().unwrap();
        let wal = tempdir().unwrap();
        let file = ws.path().join("math.rs");
        fs::write(&file, "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n").unwrap();
        let jail = WorkspaceJail::new(ws.path(), false, true).unwrap();

        let tx = apply_transactional_edits(
            &jail,
            &[FileEditOp {
                path: "math.rs".into(),
                old_text: None,
                new_text: Some("pub fn add(a: i32, b: i32) -> i32 {\n    a.saturating_add(b)\n}".into()),
                symbol: Some("add".into()),
                create_content: None,
                expected_etag: None,
                replace_all: false,
            }],
            wal.path(),
            50,
        )
        .unwrap();
        assert_eq!(tx.total_syntax_errors, 0);
        let content = fs::read_to_string(&file).unwrap();
        assert!(content.contains("saturating_add"));
        assert!(content.contains("pub fn sub"));
    }

    #[cfg(unix)]
    #[test]
    fn staging_failure_never_truncates_live_target_file() {
        use std::os::unix::fs::PermissionsExt;
        let ws = tempdir().unwrap();
        let wal = tempdir().unwrap();
        let sub = ws.path().join("readonly_dir");
        fs::create_dir(&sub).unwrap();
        let file = sub.join("precious.txt");
        fs::write(&file, "PRECIOUS_DATA_DO_NOT_TRUNCATE\n").unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o555)).unwrap();

        let jail = WorkspaceJail::new(ws.path(), false, true).unwrap();
        let res = apply_transactional_edits(
            &jail,
            &[FileEditOp {
                path: "readonly_dir/precious.txt".into(),
                old_text: Some("PRECIOUS_DATA_DO_NOT_TRUNCATE".into()),
                new_text: Some("NEW".into()),
                symbol: None,
                create_content: None,
                expected_etag: None,
                replace_all: false,
            }],
            wal.path(),
            50,
        );
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(res.is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), "PRECIOUS_DATA_DO_NOT_TRUNCATE\n");
    }

    #[test]
    fn rejects_non_utf8_file_without_silent_corruption() {
        let ws = tempdir().unwrap();
        let wal = tempdir().unwrap();
        let file = ws.path().join("gbk.txt");
        let raw_gbk = vec![0xC4, 0xE3, 0xBA, 0xC3, b'\n'];
        fs::write(&file, &raw_gbk).unwrap();
        let jail = WorkspaceJail::new(ws.path(), false, true).unwrap();

        let err = apply_transactional_edits(
            &jail,
            &[FileEditOp {
                path: "gbk.txt".into(),
                old_text: Some("x".into()),
                new_text: Some("y".into()),
                symbol: None,
                create_content: None,
                expected_etag: None,
                replace_all: false,
            }],
            wal.path(),
            50,
        )
        .unwrap_err();
        assert!(err.to_string().contains("non-UTF-8"));
        assert_eq!(fs::read(&file).unwrap(), raw_gbk);
    }
}
