use anyhow::{anyhow, Result};
use hyper_core::config::{truncate_utf8_bytes, HyperConfig};
use hyper_core::context_pack::{build_context_pack, ContextPackRequest};
use hyper_core::git::inspect_repo;
use hyper_core::jail::WorkspaceJail;
use hyper_core::runner::{discover_presets, run_check, CheckRequest};
use hyper_core::search::{batch_search, glob_files, SearchQuery};
use hyper_core::tx_edit::{apply_transactional_edits, FileEditOp};
use hyper_core::wal::{list_transactions, rollback_transaction};
use hyper_daemon::client::ensure_and_send;
use hyper_daemon::protocol::{
    DaemonRequest, DaemonResponse, DrillOutput, HandleStored, PrewarmedCheck,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

pub fn run_mcp_stdio() -> Result<()> {
    let stdin = io::stdin();
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let active_cancels: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();

    let cancel_on_exit = active_cancels.clone();
    std::thread::spawn(move || {
        if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
            rt.block_on(async {
                let _ = tokio::signal::ctrl_c().await;
                if let Ok(map) = cancel_on_exit.lock() {
                    for flag in map.values() {
                        flag.store(true, Ordering::Relaxed);
                    }
                }
            });
        }
    });

    for line in stdin.lock().lines() {
        let Ok(raw) = line else { break };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                write_json(
                    &stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": { "code": -32700, "message": format!("Parse error: {}", e) }
                    }),
                );
                continue;
            }
        };

        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();

        if method == "notifications/initialized" {
            continue;
        }
        if method == "notifications/cancelled" {
            if let Some(req_id) = msg.get("params").and_then(|p| p.get("requestId")) {
                if let Ok(map) = active_cancels.lock() {
                    if let Some(flag) = map.get(&req_id.to_string()) {
                        flag.store(true, Ordering::Relaxed);
                    }
                }
            }
            continue;
        }

        let Some(req_id) = id else { continue };

        match method {
            "initialize" => write_json(
                &stdout,
                &json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "protocolVersion": MCP_PROTOCOL_VERSION,
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": { "name": "agy-hyper", "version": hyper_core::version() }
                    }
                }),
            ),
            "ping" => write_json(&stdout, &json!({ "jsonrpc": "2.0", "id": req_id, "result": {} })),
            "tools/list" => write_json(
                &stdout,
                &json!({ "jsonrpc": "2.0", "id": req_id, "result": { "tools": tool_definitions() } }),
            ),
            "tools/call" => {
                let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
                let stdout_c = stdout.clone();
                let cancels_c = active_cancels.clone();
                let key = req_id.to_string();
                let cancel_flag = Arc::new(AtomicBool::new(false));
                if let Ok(mut map) = cancels_c.lock() {
                    map.insert(key.clone(), cancel_flag.clone());
                }
                workers.retain(|h| !h.is_finished());

                workers.push(std::thread::spawn(move || {
                    let result = dispatch_tool(&params, cancel_flag);
                    if let Ok(mut map) = cancels_c.lock() {
                        map.remove(&key);
                    }
                    let resp = match result {
                        Ok(text) => json!({
                            "jsonrpc": "2.0",
                            "id": req_id,
                            "result": { "content": [{ "type": "text", "text": text }], "isError": false }
                        }),
                        Err(err) => json!({
                            "jsonrpc": "2.0",
                            "id": req_id,
                            "result": { "content": [{ "type": "text", "text": format!("ERROR: {}", err) }], "isError": true }
                        }),
                    };
                    write_json(&stdout_c, &resp);
                }));
            }
            _ => write_json(
                &stdout,
                &json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": { "code": -32601, "message": format!("Method not found: {}", method) }
                }),
            ),
        }
    }

    for h in workers {
        let _ = h.join();
    }
    Ok(())
}

fn write_json(stdout: &Arc<Mutex<io::Stdout>>, val: &Value) {
    if let Ok(mut out) = stdout.lock() {
        if let Ok(s) = serde_json::to_string(val) {
            let _ = writeln!(out, "{}", s);
            let _ = out.flush();
        }
    }
}

fn dispatch_tool(params: &Value, cancel_flag: Arc<AtomicBool>) -> Result<String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing tool name"))?;
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let cfg = HyperConfig::load();
    let started = Instant::now();
    let max_bytes = cfg.max_output_bytes.clamp(2_000, 8_500);
    let workspace = args
        .get("workspace")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(String::from)
        .or_else(|| std::env::current_dir().ok().map(|p| p.display().to_string()))
        .unwrap_or_else(|| ".".into());

    let output = match name {
        "hyper_pack" => handle_pack(&workspace, args, &cfg, started)?,
        "hyper_search" => handle_search(&workspace, args, &cfg, started)?,
        "hyper_edit" => handle_edit(&workspace, args, &cfg, started)?,
        "hyper_check" => handle_check(&workspace, args, &cfg, started, cancel_flag)?,
        "hyper_tx" => handle_tx(&workspace, args, started)?,
        unknown => return Err(anyhow!("unknown tool '{}'", unknown)),
    };

    let (capped, was_cut) = truncate_utf8_bytes(&output, max_bytes);
    if was_cut {
        Ok(format!("{}\n… [capped at {}B inline]", capped, max_bytes))
    } else {
        Ok(capped.to_string())
    }
}

fn handle_pack(workspace: &str, args: Value, cfg: &HyperConfig, started: Instant) -> Result<String> {
    let jail = WorkspaceJail::new(workspace, cfg.allow_outside_workspace, cfg.block_secret_files)?;
    let req: ContextPackRequest = serde_json::from_value(args)?;
    let ws_str = jail.root().display().to_string();

    let (pack, cache_hit) = match ensure_and_send(&DaemonRequest::PackContext {
        workspace: ws_str.clone(),
        request: req.clone(),
    }) {
        Ok(DaemonResponse::Ok { data }) => {
            let hit = data.get("cache_hit").and_then(Value::as_bool).unwrap_or(false);
            serde_json::from_value::<hyper_core::context_pack::ContextPackResult>(
                data.get("pack").cloned().unwrap_or(Value::Null),
            )
            .map(|p| (p, hit))
            .unwrap_or_else(|_| (build_context_pack(&jail, &req, cfg.block_secret_files).unwrap(), false))
        }
        _ => (build_context_pack(&jail, &req, cfg.block_secret_files)?, false),
    };

    let turns_saved = if pack.files.len() >= 2 && req.include_git { 2 } else if !pack.files.is_empty() { 1 } else { 0 };
    record_telemetry(
        "hyper_pack",
        Some(ws_str),
        started.elapsed().as_millis() as u64,
        0,
        turns_saved,
        format!(
            "packed {} files ({}B{})",
            pack.files.len(),
            pack.used_bytes,
            if cache_hit { ", daemon-cache HIT" } else { "" }
        ),
        false,
    );
    Ok(pack.compact_markdown)
}

fn handle_search(workspace: &str, args: Value, cfg: &HyperConfig, started: Instant) -> Result<String> {
    let jail = WorkspaceJail::new(workspace, cfg.allow_outside_workspace, cfg.block_secret_files)?;
    let queries: Vec<SearchQuery> = args
        .get("queries")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let glob_pat = args.get("glob").and_then(Value::as_str);
    let glob_path = args.get("path").and_then(Value::as_str);
    let include_git = args.get("include_git").and_then(Value::as_bool).unwrap_or(false);

    let mut out = String::new();
    if !queries.is_empty() {
        for qr in &batch_search(&jail, &queries, cfg.block_secret_files) {
            out.push_str(&format!(
                "## search `{}` ({} hits, {} files{})\n",
                qr.pattern,
                qr.matches.len(),
                qr.total_files_scanned,
                if qr.truncated { ", truncated" } else { "" }
            ));
            if let Some(err) = &qr.error {
                out.push_str(&format!("  error: {}\n", err));
            }
            for m in &qr.matches {
                out.push_str(&format!("  {}:{}: {}\n", m.path, m.line, m.text));
            }
        }
    }
    if glob_pat.is_some() || (queries.is_empty() && !include_git) {
        let g = glob_files(&jail, glob_pat, glob_path, 120, cfg.block_secret_files)?;
        out.push_str(&format!(
            "## files ({} shown / {} matched{})\n",
            g.files.len(),
            g.total_matched,
            if g.truncated { ", truncated" } else { "" }
        ));
        for f in &g.files {
            out.push_str(&format!("  {}\n", f));
        }
    }
    if include_git {
        let git = inspect_repo(&jail, 10, 40, true);
        out.push_str(&format!(
            "## git ({} · {}@{})\n",
            git.engine,
            git.branch.as_deref().unwrap_or("nogit"),
            git.head_oid.as_deref().unwrap_or("-")
        ));
        if !git.dirty_files.is_empty() {
            out.push_str(&format!("  dirty: {}\n", git.dirty_files.join(", ")));
        }
        for c in git.recent_commits.iter().take(8) {
            out.push_str(&format!("  {} {} — {}\n", c.oid, c.author, c.subject));
        }
    }

    let turns_saved = (queries.len().saturating_sub(1) as u32) + u32::from(include_git && !queries.is_empty());
    record_telemetry(
        "hyper_search",
        Some(jail.root().display().to_string()),
        started.elapsed().as_millis() as u64,
        0,
        turns_saved,
        format!("{} queries, git={}", queries.len(), include_git),
        false,
    );
    Ok(out)
}

fn handle_edit(workspace: &str, args: Value, cfg: &HyperConfig, started: Instant) -> Result<String> {
    let jail = match WorkspaceJail::new(workspace, cfg.allow_outside_workspace, cfg.block_secret_files) {
        Ok(j) => j,
        Err(e) => {
            record_telemetry("hyper_edit", Some(workspace.to_string()), 1, 0, 0, e.to_string(), true);
            return Err(e.into());
        }
    };
    let edits: Vec<FileEditOp> = serde_json::from_value(
        args.get("edits").cloned().ok_or_else(|| anyhow!("missing 'edits' array"))?,
    )?;
    let tx = match apply_transactional_edits(&jail, &edits, &HyperConfig::wal_dir(), cfg.max_wal_entries) {
        Ok(t) => t,
        Err(e) => {
            let blocked = e.to_string().contains("jail") || e.to_string().contains("secret");
            record_telemetry(
                "hyper_edit",
                Some(jail.root().display().to_string()),
                started.elapsed().as_millis() as u64,
                0,
                0,
                e.to_string(),
                blocked,
            );
            return Err(e);
        }
    };

    let _ = ensure_and_send(&DaemonRequest::NotifyChange {
        workspace: jail.root().display().to_string(),
    });

    let elapsed = started.elapsed().as_millis() as u64;
    let turns_saved = tx.files.len().saturating_sub(1) as u32;
    record_telemetry(
        "hyper_edit",
        Some(jail.root().display().to_string()),
        elapsed,
        0,
        turns_saved,
        format!("{} ({} files, {} syntax err)", tx.tx_id, tx.files.len(), tx.total_syntax_errors),
        false,
    );

    let mut out = format!(
        "# committed {} ({} file(s), {} syntax error(s), {}ms)\n",
        tx.tx_id, tx.files.len(), tx.total_syntax_errors, elapsed
    );
    for f in &tx.files {
        out.push_str(&format!(
            "## {} [{} · +{} -{} · etag:{}]\n",
            f.path, f.action, f.lines_added, f.lines_removed, f.etag_after
        ));
        if !f.diff_preview.is_empty() {
            out.push_str(&f.diff_preview);
            out.push('\n');
        }
        for se in &f.syntax_errors {
            out.push_str(&format!("  ⚠ SYNTAX L{}:{} {} `{}`\n", se.line, se.col, se.kind, se.snippet));
        }
    }
    Ok(out)
}

fn handle_check(
    workspace: &str,
    args: Value,
    cfg: &HyperConfig,
    started: Instant,
    cancel_flag: Arc<AtomicBool>,
) -> Result<String> {
    let jail = WorkspaceJail::new(workspace, cfg.allow_outside_workspace, cfg.block_secret_files)?;
    if args.get("list_presets").and_then(Value::as_bool).unwrap_or(false) {
        let presets = discover_presets(jail.root());
        let mut out = format!("# discovered presets ({})\n", presets.len());
        for p in presets {
            out.push_str(&format!(
                "  {} => `{} {}` ({}{})\n",
                p.id,
                p.program,
                p.args.join(" "),
                p.source,
                if p.read_only_safe { ", read-only" } else { "" }
            ));
        }
        return Ok(out);
    }

    let req: CheckRequest = serde_json::from_value(args)?;
    if cfg.prewarm_tier2 {
        if let Some(preset_id) = &req.preset {
            if let Ok(DaemonResponse::Ok { data }) = ensure_and_send(&DaemonRequest::LookupPrewarm {
                workspace: jail.root().display().to_string(),
                preset: preset_id.clone(),
            }) {
                if !data.is_null() {
                    if let Ok(hit) = serde_json::from_value::<PrewarmedCheck>(data) {
                        record_telemetry(
                            "hyper_check_prewarm_hit",
                            Some(jail.root().display().to_string()),
                            started.elapsed().as_millis() as u64,
                            hit.duration_ms,
                            0,
                            format!("prewarmed `{}` rev#{} exit={} (0ms wait)", hit.command_display, hit.workspace_rev, hit.exit_code),
                            false,
                        );
                        let mut out = format!(
                            "# check `{}` · exit={} · {}ms [prewarmed · rev #{}] · {}B\n",
                            hit.command_display, hit.exit_code, hit.duration_ms, hit.workspace_rev, hit.total_output_bytes
                        );
                        if let Some(hint) = &hit.toc_hint {
                            out.push_str(&format!("digest: {}\n", hint));
                        }
                        if let Some(h) = &hit.spill_handle {
                            out.push_str(&format!("handle: {} (call hyper_tx action=\"drill\" handle_id=\"{}\")\n", h, h));
                        }
                        for l in &hit.summary_lines {
                            out.push_str(l);
                            out.push('\n');
                        }
                        return Ok(out);
                    }
                }
            }
        }
    }

    let mut res = match run_check(&jail, cfg, &req, Some(cancel_flag)) {
        Ok(r) => r,
        Err(e) => {
            record_telemetry(
                "hyper_check",
                Some(jail.root().display().to_string()),
                started.elapsed().as_millis() as u64,
                0,
                0,
                e.to_string(),
                true,
            );
            return Err(e);
        }
    };

    if res.digest.truncated && !res.full_captured_log.is_empty() {
        if let Ok(DaemonResponse::Ok { data }) = ensure_and_send(&DaemonRequest::StoreHandle {
            label: res.command_display.clone(),
            content: res.full_captured_log.clone(),
        }) {
            if let Ok(stored) = serde_json::from_value::<HandleStored>(data) {
                res.spill_handle = Some(stored.handle_id);
            }
        }
    }

    record_telemetry(
        "hyper_check",
        Some(jail.root().display().to_string()),
        res.duration_ms,
        0,
        0,
        format!("`{}` exit={} ({}ms)", res.command_display, res.exit_code, res.duration_ms),
        false,
    );

    let mut out = format!(
        "# check `{}` · exit={} · {}ms · {}B\n",
        res.command_display, res.exit_code, res.duration_ms, res.total_output_bytes
    );
    if res.timed_out {
        out.push_str("⚠ TIMED OUT (process tree killed cleanly)\n");
    }
    if res.cancelled {
        out.push_str("⚠ CANCELLED by client (process tree killed cleanly)\n");
    }
    if let Some(hint) = &res.digest.toc_hint {
        out.push_str(&format!("digest: {}\n", hint));
    }
    if let Some(h) = &res.spill_handle {
        out.push_str(&format!(
            "handle: {} (call hyper_tx action=\"drill\" handle_id=\"{}\" to inspect raw lines)\n",
            h, h
        ));
    }
    for l in &res.digest.summary_lines {
        out.push_str(l);
        out.push('\n');
    }
    Ok(out)
}

fn handle_tx(workspace: &str, args: Value, started: Instant) -> Result<String> {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
    match action {
        "list" => {
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(15) as usize;
            let txs = list_transactions(&HyperConfig::wal_dir(), limit);
            let mut out = format!("# WAL transactions ({})\n", txs.len());
            for t in txs {
                let files: Vec<&str> = t.files.iter().map(|f| f.rel_path.as_str()).collect();
                out.push_str(&format!(
                    "  {} [{}] {}\n",
                    t.tx_id,
                    if t.rolled_back { "rolled-back" } else { "committed" },
                    files.join(", ")
                ));
            }
            Ok(out)
        }
        "rollback" => {
            let tx_id = args
                .get("tx_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("rollback requires 'tx_id'"))?;
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
            let rb = rollback_transaction(&HyperConfig::wal_dir(), tx_id, force)?;
            record_telemetry(
                "rollback",
                Some(workspace.to_string()),
                started.elapsed().as_millis() as u64,
                0,
                0,
                format!("rollback {} ({} restored)", tx_id, rb.restored_files.len()),
                false,
            );
            Ok(serde_json::to_string_pretty(&rb)?)
        }
        "drill" => {
            let handle_id = args
                .get("handle_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("drill requires 'handle_id'"))?;
            let offset_lines = args.get("offset_lines").and_then(Value::as_u64).unwrap_or(0) as usize;
            let limit_lines = args.get("limit_lines").and_then(Value::as_u64).unwrap_or(80) as usize;
            let grep = args.get("grep").and_then(Value::as_str).map(String::from);
            match ensure_and_send(&DaemonRequest::DrillHandle {
                handle_id: handle_id.to_string(),
                offset_lines,
                limit_lines,
                grep,
            })? {
                DaemonResponse::Ok { data } => {
                    let d: DrillOutput = serde_json::from_value(data)?;
                    let range = if d.returned_lines == 0 {
                        format!("0 of {}", d.total_lines)
                    } else {
                        format!("{}-{} of {}", d.offset_lines + 1, d.offset_lines + d.returned_lines, d.total_lines)
                    };
                    Ok(format!("# drill {} (`{}` · lines {})\n{}", d.handle_id, d.label, range, d.content))
                }
                DaemonResponse::Err { message } => Err(anyhow!(message)),
            }
        }
        other => Err(anyhow!("unknown hyper_tx action '{}'", other)),
    }
}

fn record_telemetry(
    kind: &str,
    workspace: Option<String>,
    duration_ms: u64,
    saved_ms: u64,
    turns_saved: u32,
    detail: String,
    blocked: bool,
) {
    let _ = ensure_and_send(&DaemonRequest::TelemetryEvent {
        kind: kind.into(),
        conversation_id: None,
        workspace,
        duration_ms,
        saved_ms,
        turns_saved,
        detail,
        blocked,
    });
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "hyper_pack",
            "description": "Semantic context packer (Tree-sitter AST outlines + inline matched symbol slices + single-pass ripgrep + 1-hop import neighbors + read-only gix git signals) packed strictly <= 8.5KB inline.",
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Workspace root directory path" },
                    "query": { "type": "string", "description": "Task keywords or symbol names to pack" },
                    "focus_paths": { "type": "array", "items": { "type": "string" } },
                    "include_outlines": { "type": "boolean", "default": true },
                    "include_git": { "type": "boolean", "default": true },
                    "max_bytes": { "type": "integer", "default": 8200 }
                }
            }
        },
        {
            "name": "hyper_search",
            "description": "Batched pure-Rust ripgrep search, glob file listing, and physically read-only gix git inspection.",
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string" },
                    "queries": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["pattern"],
                            "properties": {
                                "pattern": { "type": "string" },
                                "path": { "type": "string" },
                                "glob": { "type": "string" },
                                "fixed_strings": { "type": "boolean" },
                                "case_sensitive": { "type": "boolean" },
                                "max_matches": { "type": "integer" }
                            }
                        }
                    },
                    "glob": { "type": "string" },
                    "path": { "type": "string" },
                    "include_git": { "type": "boolean" }
                }
            }
        },
        {
            "name": "hyper_edit",
            "description": "Crash-safe WAL-journaled atomic multi-file editor with BLAKE3 ETag CAS locking, exact or Tree-sitter symbol replacement, zero ENOSPC truncation, and instant inline syntax verification.",
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false },
            "inputSchema": {
                "type": "object",
                "required": ["edits"],
                "properties": {
                    "workspace": { "type": "string" },
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["path"],
                            "properties": {
                                "path": { "type": "string" },
                                "old_text": { "type": "string" },
                                "new_text": { "type": "string" },
                                "symbol": { "type": "string" },
                                "create_content": { "type": "string" },
                                "expected_etag": { "type": "string" },
                                "replace_all": { "type": "boolean" }
                            }
                        }
                    }
                }
            }
        },
        {
            "name": "hyper_check",
            "description": "Safe shell-free preset/argv verification runner with head+tail ring buffer, error digest, cancellation process-tree cleanup, and LRU spill handles.",
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string" },
                    "list_presets": { "type": "boolean" },
                    "preset": { "type": "string" },
                    "argv": { "type": "array", "items": { "type": "string" } },
                    "timeout_secs": { "type": "integer", "default": 60 },
                    "cwd": { "type": "string" }
                }
            }
        },
        {
            "name": "hyper_tx",
            "description": "Inspect WAL edit transactions, atomically rollback a tx_id, or 1ms drill into spilled check handles.",
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false },
            "inputSchema": {
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": { "type": "string", "enum": ["list", "rollback", "drill"] },
                    "tx_id": { "type": "string" },
                    "force": { "type": "boolean" },
                    "handle_id": { "type": "string" },
                    "offset_lines": { "type": "integer" },
                    "limit_lines": { "type": "integer" },
                    "grep": { "type": "string" },
                    "limit": { "type": "integer" }
                }
            }
        }
    ])
}
