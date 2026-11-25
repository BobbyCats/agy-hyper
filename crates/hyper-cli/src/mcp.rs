use anyhow::{anyhow, Result};
use hyper_core::audit::{build_audit_report, BurnAuditReport};
use hyper_core::config::{truncate_utf8_bytes, HyperConfig, Profile};
use hyper_core::context_pack::{build_context_pack, ContextPackRequest};
use hyper_core::git::inspect_repo;
use hyper_core::jail::WorkspaceJail;
use hyper_core::runner::{discover_presets, run_check, CheckRequest};
use hyper_core::search::{batch_search, glob_files, SearchQuery};
use hyper_core::transcript::HealthLevel;
use hyper_core::tx_edit::{apply_transactional_edits_with_writer, FileEditOp};
use hyper_core::usage::{
    default_brain_dirs, now_epoch_ms, UsageLedger, UsageWindow, UsageWindowReport,
};
use hyper_core::wal::{
    format_epoch_ms_rfc3339, inspect_transaction_diff, list_transactions, rollback_transaction,
    RollbackResult, WalManifest,
};
use hyper_daemon::client::{ensure_and_send, send_request, send_request_with_timeout};
use hyper_daemon::protocol::{
    CockpitSnapshot, DaemonRequest, DaemonResponse, DrillOutput, EngineStatus, HandleStored,
    PrewarmedCheck, SubagentSnapshot,
};
use hyper_daemon::server::socket_path;
use hyper_daemon::state::build_cockpit_snapshot;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const MCP_APP_MIME_TYPE: &str = "text/html;profile=mcp-app";
const MAX_MCP_APP_BYTES: usize = 10 * 1024 * 1024;
const MAX_STRUCTURED_DIFF_BYTES: usize = 256 * 1024;
const MAX_INLINE_LIST_DIFF_BYTES: usize = 64 * 1024;
const MAX_COCKPIT_TEXT_BYTES: usize = 2_000;
const TX_DIFF_RESOURCE_URI: &str = "ui://agy-hyper/tx-diff.html";
const COCKPIT_RESOURCE_URI: &str = "ui://agy-hyper/cockpit.html";

struct ToolCallOutput {
    text: String,
    structured_content: Option<Value>,
}

impl ToolCallOutput {
    fn text_only(text: String) -> Self {
        Self {
            text,
            structured_content: None,
        }
    }

    fn with_structured(text: String, structured: Value) -> Self {
        Self {
            text,
            structured_content: Some(structured),
        }
    }
}

#[derive(Debug)]
enum ResourceReadError {
    NotFound(String),
    Invalid(String),
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": {}
        },
        "serverInfo": { "name": "agy-hyper", "version": hyper_core::version() }
    })
}

fn resource_definitions() -> Value {
    json!([
        {
            "uri": TX_DIFF_RESOURCE_URI,
            "name": "AGY Hyper TX Diff Viewer",
            "description": "Interactive WAL transaction timeline, unified diff viewer, and rollback controls.",
            "mimeType": MCP_APP_MIME_TYPE,
            "_meta": { "ui": { "prefersBorder": false } }
        },
        {
            "uri": COCKPIT_RESOURCE_URI,
            "name": "AGY Hyper Cockpit",
            "description": "Live telemetry cockpit for context warmth, subagent watchdog, usage ledger, burn audit, and WAL history.",
            "mimeType": MCP_APP_MIME_TYPE,
            "_meta": { "ui": { "prefersBorder": false } }
        }
    ])
}

fn env_mcp_apps_dir() -> Option<PathBuf> {
    std::env::var("AGY_HYPER_MCP_APPS_DIR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

fn fallback_ui_html(title: &str, resource_uri: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width,initial-scale=1" />
<title>{title}</title>
<style>
  body {{
    margin: 0;
    padding: 16px;
    background: var(--color-background-primary, #111318);
    color: var(--color-text-primary, #e6e8ee);
    font-family: var(--font-sans, system-ui, -apple-system, sans-serif);
    font-size: 13px;
    line-height: 1.5;
  }}
  .card {{
    border: 1px solid var(--color-border-primary, #2b303b);
    border-radius: 8px;
    padding: 12px 14px;
    background: var(--color-background-secondary, #181b22);
  }}
  h1 {{ margin: 0 0 8px 0; font-size: 14px; font-weight: 600; }}
  pre {{
    margin: 8px 0 0 0;
    white-space: pre-wrap;
    word-break: break-word;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 12px;
    color: var(--color-text-secondary, #a8b0c0);
  }}
</style>
</head>
<body>
  <div class="card">
    <h1>{title}</h1>
    <div id="status">Built-in fallback view ({resource_uri})</div>
    <pre id="payload">Waiting for tool payload...</pre>
  </div>
  <script>
  (function () {{
    var payloadEl = document.getElementById('payload');
    var statusEl = document.getElementById('status');
    window.addEventListener('message', function (ev) {{
      var msg = ev.data;
      if (!msg || typeof msg !== 'object') return;
      if (msg.method === 'ui/notifications/tool-result' && msg.params) {{
        var data = msg.params.structuredContent || msg.params;
        statusEl.textContent = 'Loaded payload';
        payloadEl.textContent = JSON.stringify(data, null, 2);
      }}
    }});
    try {{
      window.parent.postMessage({{
        jsonrpc: '2.0',
        id: 1,
        method: 'ui/initialize',
        params: {{ appInfo: {{ name: 'agy-hyper-fallback', version: '0.6.0' }}, capabilities: {{}} }}
      }}, '*');
      window.parent.postMessage({{
        jsonrpc: '2.0',
        method: 'ui/notifications/initialized',
        params: {{}}
      }}, '*');
    }} catch (_) {{}}
  }})();
  </script>
</body>
</html>
"#
    )
}

fn validate_ui_resource_bytes(bytes: &[u8], source_label: &str) -> Result<String, ResourceReadError> {
    if bytes.is_empty() {
        return Err(ResourceReadError::Invalid(format!(
            "UI resource '{}' is empty (0 bytes)",
            source_label
        )));
    }
    if bytes.len() >= MAX_MCP_APP_BYTES {
        return Err(ResourceReadError::Invalid(format!(
            "UI resource '{}' exceeds 10 MiB limit ({} bytes)",
            source_label,
            bytes.len()
        )));
    }
    String::from_utf8(bytes.to_vec()).map_err(|e| {
        ResourceReadError::Invalid(format!(
            "UI resource '{}' is not valid UTF-8: {}",
            source_label, e
        ))
    })
}

fn read_ui_resource_with_context(
    uri: &str,
    env_apps_dir: Option<&Path>,
    env_plugin_root: Option<&Path>,
    exe_path: Option<&Path>,
    home: Option<&Path>,
) -> Result<Value, ResourceReadError> {
    let (filename, fallback_title) = match uri.trim() {
        TX_DIFF_RESOURCE_URI => ("tx-diff.html", "AGY Hyper TX Diff Viewer"),
        COCKPIT_RESOURCE_URI => ("cockpit.html", "AGY Hyper Cockpit"),
        other => {
            return Err(ResourceReadError::NotFound(format!(
                "unknown resource URI '{}'",
                other
            )));
        }
    };

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = env_apps_dir {
        candidates.push(dir.join(filename));
    }
    if let Some(root) = env_plugin_root {
        candidates.push(root.join("mcp_apps").join(filename));
    }
    if let Some(exe) = exe_path {
        if let Some(bin_dir) = exe.parent() {
            candidates.push(bin_dir.join("mcp_apps").join(filename));
            candidates.push(bin_dir.join("../mcp_apps").join(filename));
            candidates.push(bin_dir.join("../../plugin/mcp_apps").join(filename));
        }
    }
    if let Some(h) = home {
        candidates.push(
            h.join(".gemini")
                .join("config")
                .join("plugins")
                .join("agy-hyper")
                .join("mcp_apps")
                .join(filename),
        );
    }

    let mut html_opt: Option<String> = None;
    for candidate in candidates {
        match std::fs::read(&candidate) {
            Ok(bytes) => {
                let html = validate_ui_resource_bytes(&bytes, &candidate.display().to_string())?;
                html_opt = Some(html);
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(ResourceReadError::Invalid(format!(
                    "failed to read UI resource '{}': {}",
                    candidate.display(),
                    e
                )));
            }
        }
    }

    let html = match html_opt {
        Some(h) => h,
        None => {
            let fb = fallback_ui_html(fallback_title, uri);
            validate_ui_resource_bytes(fb.as_bytes(), uri)?
        }
    };

    Ok(json!({
        "contents": [
            {
                "uri": uri.trim(),
                "mimeType": MCP_APP_MIME_TYPE,
                "text": html,
                "_meta": { "ui": { "prefersBorder": false } }
            }
        ]
    }))
}

fn handle_resources_read(params: &Value) -> std::result::Result<Value, (i64, String)> {
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| (-32602, "missing 'uri' parameter".to_string()))?;

    let apps_dir = env_mcp_apps_dir();
    let plugin_root = env_plugin_root();
    let exe = std::env::current_exe().ok();
    let home = dirs::home_dir();

    read_ui_resource_with_context(
        uri,
        apps_dir.as_deref(),
        plugin_root.as_deref(),
        exe.as_deref(),
        home.as_deref(),
    )
    .map_err(|err| match err {
        ResourceReadError::NotFound(m) => (-32002, m),
        ResourceReadError::Invalid(m) => (-32603, m),
    })
}

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
                    "result": initialize_result()
                }),
            ),
            "ping" => write_json(&stdout, &json!({ "jsonrpc": "2.0", "id": req_id, "result": {} })),
            "resources/list" => write_json(
                &stdout,
                &json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": { "resources": resource_definitions() }
                }),
            ),
            "resources/read" => {
                let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
                let resp = match handle_resources_read(&params) {
                    Ok(res) => json!({ "jsonrpc": "2.0", "id": req_id, "result": res }),
                    Err((code, message)) => json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "error": { "code": code, "message": message }
                    }),
                };
                write_json(&stdout, &resp);
            }
            "tools/list" => {
                let cfg = HyperConfig::load();
                write_json(
                    &stdout,
                    &json!({ "jsonrpc": "2.0", "id": req_id, "result": { "tools": tool_definitions(&cfg) } }),
                )
            }
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
                        Ok(out) => {
                            let mut res = json!({
                                "content": [{ "type": "text", "text": out.text }],
                                "isError": false
                            });
                            if let Some(sc) = out.structured_content {
                                res["structuredContent"] = sc;
                            }
                            json!({
                                "jsonrpc": "2.0",
                                "id": req_id,
                                "result": res
                            })
                        }
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

fn canonical_or_raw(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn is_installed_plugin_dir(
    candidate: &Path,
    exe_path: Option<&Path>,
    home: Option<&Path>,
    env_plugin_root: Option<&Path>,
) -> bool {
    let cand = canonical_or_raw(candidate);
    // Developing a Rust source checkout (e.g. agy-hyper repo itself) is never a deployed plugin dir.
    if cand.join("Cargo.toml").is_file() {
        return false;
    }

    if let Some(root) = env_plugin_root {
        let root_c = canonical_or_raw(root);
        if !root_c.join("Cargo.toml").is_file() && cand.starts_with(&root_c) {
            return true;
        }
    }

    if let Some(exe) = exe_path {
        let exe_c = canonical_or_raw(exe);
        if let Some(bin_dir) = exe_c.parent() {
            let is_bin = bin_dir
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("bin"))
                .unwrap_or(false);
            if is_bin {
                if let Some(plugin_root) = bin_dir.parent() {
                    if plugin_root.join("plugin.json").is_file()
                        && !plugin_root.join("Cargo.toml").is_file()
                        && cand.starts_with(plugin_root)
                    {
                        return true;
                    }
                }
            }
        }
    }

    if let Some(h) = home {
        let h_c = canonical_or_raw(h);
        for rel in [
            ".gemini/config/plugins",
            ".gemini/antigravity/plugins",
            ".gemini/antigravity-cli/plugins",
        ] {
            let plugins_root = canonical_or_raw(&h_c.join(rel));
            if cand.starts_with(&plugins_root) {
                return true;
            }
        }
    }

    false
}

fn unsafe_default_cwd_reason(
    cwd: &Path,
    exe_path: Option<&Path>,
    home: Option<&Path>,
    env_plugin_root: Option<&Path>,
) -> Option<&'static str> {
    if is_installed_plugin_dir(cwd, exe_path, home, env_plugin_root) {
        return Some("the installed plugin directory");
    }
    let cwd_c = canonical_or_raw(cwd);
    if cwd_c.parent().is_none() {
        return Some("the filesystem root");
    }
    if let Some(h) = home {
        if cwd_c == canonical_or_raw(h) {
            return Some("the user home directory");
        }
    }
    None
}

pub(crate) fn detect_remote_uri_prefix(raw: &str) -> Option<&'static str> {
    let lower = raw.trim().to_ascii_lowercase();
    if lower.starts_with("vscode-remote://") {
        Some("vscode-remote://")
    } else if lower.starts_with("ssh-remote://") {
        Some("ssh-remote://")
    } else {
        None
    }
}

pub(crate) fn current_ssh_session() -> Option<(&'static str, String)> {
    for key in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"] {
        if let Ok(val) = std::env::var(key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some((key, trimmed.to_string()));
            }
        }
    }
    None
}

fn resolve_mcp_workspace_with_ssh(
    explicit: Option<&str>,
    allow_outside_workspace: bool,
    cwd: Option<&Path>,
    exe_path: Option<&Path>,
    home: Option<&Path>,
    env_plugin_root: Option<&Path>,
    ssh_session: Option<(&str, &str)>,
) -> Result<String> {
    let trimmed = explicit.map(str::trim).filter(|s| !s.is_empty());
    if let Some(ws) = trimmed {
        if let Some(prefix) = detect_remote_uri_prefix(ws) {
            return Err(anyhow!(
                "remote workspace URI '{}' ({}) is not a local filesystem path; pass the host-native absolute path (e.g., \"/home/user/project\") on the machine running agy-hyper, or run agy-hyper directly on the remote SSH host",
                ws,
                prefix
            ));
        }
        if ws != "." {
            let p = Path::new(ws);
            if !allow_outside_workspace && is_installed_plugin_dir(p, exe_path, home, env_plugin_root) {
                return Err(anyhow!(
                    "refusing to target installed plugin directory '{}' as workspace; pass your project root path",
                    ws
                ));
            }
            if let Some((ssh_var, _)) = ssh_session {
                if !p.exists() {
                    return Err(anyhow!(
                        "SSH session detected ({}): workspace path '{}' does not exist on this host. Confirm whether agy-hyper is running locally or on the remote SSH host and pass an absolute path valid on this host.",
                        ssh_var,
                        ws
                    ));
                }
            }
            return Ok(ws.to_string());
        }
    }

    let Some(cwd_path) = cwd else {
        let ssh_note = ssh_session
            .map(|(k, _)| format!("; SSH session detected via {}", k))
            .unwrap_or_default();
        return Err(anyhow!(
            "workspace parameter is required (could not determine MCP process working directory{})",
            ssh_note
        ));
    };

    let cwd_lossy = cwd_path.to_string_lossy();
    if let Some(prefix) = detect_remote_uri_prefix(&cwd_lossy) {
        return Err(anyhow!(
            "remote workspace URI '{}' ({}) is not a local filesystem path; pass the host-native absolute 'workspace' path",
            cwd_lossy,
            prefix
        ));
    }

    if let Some(reason) = unsafe_default_cwd_reason(cwd_path, exe_path, home, env_plugin_root) {
        let ssh_suffix = ssh_session
            .map(|(k, _)| {
                format!(
                    " (SSH session detected via {}: pass an absolute path on the remote host)",
                    k
                )
            })
            .unwrap_or_default();
        return Err(anyhow!(
            "workspace parameter is required: MCP process cwd ('{}') points to {}, not your project workspace. Re-call this tool with an explicit absolute 'workspace' path (e.g., workspace: \"/path/to/project\").{}",
            cwd_path.display(),
            reason,
            ssh_suffix
        ));
    }

    if let Some((ssh_var, _)) = ssh_session {
        if !cwd_path.exists() {
            return Err(anyhow!(
                "SSH session detected ({}): MCP process cwd '{}' does not exist on this host; pass an explicit absolute 'workspace' path valid on this host.",
                ssh_var,
                cwd_path.display()
            ));
        }
    }

    Ok(cwd_path.display().to_string())
}

fn resolve_mcp_workspace(
    explicit: Option<&str>,
    allow_outside_workspace: bool,
    cwd: Option<&Path>,
    exe_path: Option<&Path>,
    home: Option<&Path>,
    env_plugin_root: Option<&Path>,
) -> Result<String> {
    let ssh = current_ssh_session();
    let ssh_ref = ssh.as_ref().map(|(k, v)| (*k, v.as_str()));
    resolve_mcp_workspace_with_ssh(
        explicit,
        allow_outside_workspace,
        cwd,
        exe_path,
        home,
        env_plugin_root,
        ssh_ref,
    )
}

fn env_plugin_root() -> Option<PathBuf> {
    for key in [
        "PLUGIN_ROOT",
        "AGY_PLUGIN_ROOT",
        "ANTIGRAVITY_PLUGIN_ROOT",
        "CLAUDE_PLUGIN_ROOT",
    ] {
        if let Ok(val) = std::env::var(key) {
            let t = val.trim();
            if !t.is_empty() {
                return Some(PathBuf::from(t));
            }
        }
    }
    None
}

fn dispatch_tool(params: &Value, cancel_flag: Arc<AtomicBool>) -> Result<ToolCallOutput> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing tool name"))?;
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let cfg = HyperConfig::load();
    let started = Instant::now();
    let max_bytes = cfg.max_output_bytes.clamp(2_000, 8_500);

    if !cfg.is_mcp_tool_enabled(name) {
        let hint = if name == "hyper_edit" {
            "Use Antigravity's built-in edit_file (native inline diff preserved; PostToolUse still records WAL snapshots) or enable enable_mcp_edit in Hyper Deck."
        } else {
            "Use Antigravity's built-in tools or switch profile / toggle MCP acceleration in Hyper Deck."
        };
        return Err(anyhow!(
            "tool '{}' is disabled under current AGY Hyper profile '{}' ({})",
            name,
            cfg.profile.as_str(),
            hint
        ));
    }

    let explicit_ws = args
        .get("workspace")
        .and_then(Value::as_str)
        .map(String::from);
    let mut output = match name {
        "hyper_tx" => handle_tx(
            explicit_ws.as_deref().filter(|s| !s.trim().is_empty()),
            args,
            started,
        )?,
        "hyper_cockpit" => handle_cockpit(args, &cfg)?,
        "hyper_app_tx_diff" => handle_app_tx_diff(&HyperConfig::wal_dir(), args)?,
        "hyper_app_tx_rollback" => handle_app_tx_rollback(
            &HyperConfig::wal_dir(),
            explicit_ws.as_deref().filter(|s| !s.trim().is_empty()),
            args,
            started,
        )?,
        "hyper_app_cockpit_data" => handle_app_cockpit_data(args, &cfg)?,
        "hyper_app_set_profile" => {
            handle_app_set_profile(&HyperConfig::config_path(), args, true)?
        }
        "hyper_pack" | "hyper_search" | "hyper_edit" | "hyper_check" => {
            let cwd = std::env::current_dir().ok();
            let exe = std::env::current_exe().ok();
            let home = dirs::home_dir();
            let env_root = env_plugin_root();
            let workspace = resolve_mcp_workspace(
                explicit_ws.as_deref(),
                cfg.allow_outside_workspace,
                cwd.as_deref(),
                exe.as_deref(),
                home.as_deref(),
                env_root.as_deref(),
            )?;
            let text = match name {
                "hyper_pack" => handle_pack(&workspace, args, &cfg, started)?,
                "hyper_search" => handle_search(&workspace, args, &cfg, started)?,
                "hyper_edit" => handle_edit(&workspace, args, &cfg, started)?,
                "hyper_check" => handle_check(&workspace, args, &cfg, started, cancel_flag)?,
                _ => unreachable!(),
            };
            ToolCallOutput::text_only(text)
        }
        unknown => return Err(anyhow!("unknown tool '{}'", unknown)),
    };

    let cap_limit = if name == "hyper_cockpit" {
        MAX_COCKPIT_TEXT_BYTES.min(max_bytes)
    } else {
        max_bytes
    };
    let (capped, was_cut) = truncate_utf8_bytes(&output.text, cap_limit);
    if was_cut {
        output.text = format!("{}\n… [capped at {}B inline]", capped, cap_limit);
    }
    Ok(output)
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
    let writer_tag = std::env::var("AGY_CONVERSATION_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("mcp-pid:{}", std::process::id()));
    let tx = match apply_transactional_edits_with_writer(
        &jail,
        &edits,
        &HyperConfig::wal_dir(),
        cfg.max_wal_entries,
        Some(&writer_tag),
    ) {
        Ok(t) => t,
        Err(e) => {
            let msg = e.to_string();
            let is_etag = msg.contains("ETag CAS conflict");
            let blocked = is_etag || msg.contains("jail") || msg.contains("secret");
            record_telemetry(
                if is_etag { "etag_conflict" } else { "hyper_edit" },
                Some(jail.root().display().to_string()),
                started.elapsed().as_millis() as u64,
                0,
                0,
                msg,
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
    if let Some(preset_id) = &req.preset {
        let _ = ensure_and_send(&DaemonRequest::RecordCheckTarget {
            workspace: jail.root().display().to_string(),
            preset: preset_id.clone(),
        });
    }
    if cfg.prewarm_tier2 {
        if let Some(preset_id) = &req.preset {
            let wait_t0 = Instant::now();
            if let Some(hit) =
                lookup_prewarm_singleflight(&jail.root().display().to_string(), preset_id)
            {
                let waited_ms = wait_t0.elapsed().as_millis() as u64;
                let saved_ms = hit.duration_ms.saturating_sub(waited_ms);
                record_telemetry(
                    "hyper_check_prewarm_hit",
                    Some(jail.root().display().to_string()),
                    started.elapsed().as_millis() as u64,
                    saved_ms,
                    0,
                    format!(
                        "prewarmed `{}` rev#{} exit={} ({}ms wait)",
                        hit.command_display, hit.workspace_rev, hit.exit_code, waited_ms
                    ),
                    false,
                );
                let mut out = format!(
                    "# check `{}` · exit={} · {}ms [prewarmed · rev #{}] · {}B\n",
                    hit.command_display,
                    hit.exit_code,
                    hit.duration_ms,
                    hit.workspace_rev,
                    hit.total_output_bytes
                );
                if let Some(hint) = &hit.toc_hint {
                    out.push_str(&format!("digest: {}\n", hint));
                }
                if let Some(h) = &hit.spill_handle {
                    out.push_str(&format!(
                        "handle: {} (call hyper_tx action=\"drill\" handle_id=\"{}\")\n",
                        h, h
                    ));
                }
                for l in &hit.summary_lines {
                    out.push_str(l);
                    out.push('\n');
                }
                return Ok(out);
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

    let abort_tag = res
        .abort_reason
        .as_deref()
        .map(|r| format!(" [{}]", r))
        .unwrap_or_default();
    record_telemetry(
        "hyper_check",
        Some(jail.root().display().to_string()),
        res.duration_ms,
        0,
        0,
        format!(
            "`{}` exit={} ({}ms){}",
            res.command_display, res.exit_code, res.duration_ms, abort_tag
        ),
        false,
    );

    let mut out = format!(
        "# check `{}` · exit={} · {}ms · {}B\n",
        res.command_display, res.exit_code, res.duration_ms, res.total_output_bytes
    );
    if res.fail_fast_triggered {
        out.push_str(
            "⚠ FAIL-FAST ABORT (fatal_hang: fatal stderr signature followed by silence; process tree killed cleanly)\n",
        );
    } else if res.idle_timed_out {
        out.push_str(
            "⚠ IDLE HANG ABORT (idle_hang: silent hang exceeded idle watchdog; process tree killed cleanly)\n",
        );
    } else if res.timed_out {
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

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

fn format_tx_summary(m: &WalManifest) -> String {
    let added: usize = m.files.iter().map(|f| f.lines_added).sum();
    let removed: usize = m.files.iter().map(|f| f.lines_removed).sum();
    if m.files.is_empty() {
        return "0 files".to_string();
    }
    if m.files.len() == 1 {
        return format!("{} (+{}/-{})", m.files[0].rel_path, added, removed);
    }
    let preview: Vec<&str> = m
        .files
        .iter()
        .take(2)
        .map(|f| f.rel_path.as_str())
        .collect();
    let more = if m.files.len() > 2 { ", …" } else { "" };
    format!(
        "{} files (+{}/-{}): {}{}",
        m.files.len(),
        added,
        removed,
        preview.join(", "),
        more
    )
}

fn tx_manifest_to_json(m: &WalManifest) -> Value {
    let files: Vec<Value> = m
        .files
        .iter()
        .map(|f| {
            json!({
                "path": f.rel_path,
                "abs_path": f.abs_path,
                "added": f.lines_added,
                "removed": f.lines_removed
            })
        })
        .collect();
    json!({
        "tx_id": m.tx_id,
        "created_at": format_epoch_ms_rfc3339(m.timestamp_ms),
        "source": m.source,
        "summary": format_tx_summary(m),
        "files": files,
        "rolled_back": m.rolled_back
    })
}

fn build_tx_diff_json(
    wal_dir: &Path,
    tx_id: &str,
    max_lines_per_file: usize,
    max_diff_bytes: usize,
) -> Result<Value> {
    let payload = inspect_transaction_diff(wal_dir, tx_id, max_lines_per_file)?;
    let mut combined = String::new();
    let mut files = Vec::with_capacity(payload.files.len());
    for (idx, f) in payload.files.iter().enumerate() {
        if idx > 0 && !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        if !f.unified_diff.is_empty() {
            combined.push_str(&f.unified_diff);
            combined.push('\n');
        } else if let Some(reason) = &f.unavailable_reason {
            combined.push_str(&format!(
                "--- a/{}\n+++ b/{}\n# {}\n",
                f.rel_path, f.rel_path, reason
            ));
        } else {
            combined.push_str(&format!(
                "--- a/{}\n+++ b/{}\n# (no line changes)\n",
                f.rel_path, f.rel_path
            ));
        }
        files.push(json!({
            "path": f.rel_path,
            "abs_path": f.abs_path,
            "added": f.lines_added,
            "removed": f.lines_removed
        }));
    }

    let cap = max_diff_bytes.min(MAX_STRUCTURED_DIFF_BYTES);
    let (capped, was_cut) = truncate_utf8_bytes(&combined, cap);
    let diff_text = if was_cut {
        format!("{}\n... [diff truncated at {}B]", capped, cap)
    } else {
        capped.trim_end_matches('\n').to_string()
    };

    Ok(json!({
        "tx_id": payload.tx_id,
        "diff_text": diff_text,
        "files": files
    }))
}

fn build_rollback_json(rb: &RollbackResult) -> Value {
    let ok = rb.skipped_or_conflicts.is_empty();
    let mut parts = Vec::new();
    if !rb.restored_files.is_empty() {
        parts.push(format!("restored {} file(s)", rb.restored_files.len()));
    }
    if !rb.removed_created_files.is_empty() {
        parts.push(format!(
            "removed {} created file(s)",
            rb.removed_created_files.len()
        ));
    }
    if !rb.skipped_or_conflicts.is_empty() {
        parts.push(format!(
            "skipped {} conflict(s): {}",
            rb.skipped_or_conflicts.len(),
            rb.skipped_or_conflicts.join("; ")
        ));
    }
    let message = if parts.is_empty() {
        format!("Transaction {} had no files to modify.", rb.tx_id)
    } else if ok {
        format!("Rolled back {} ({}).", rb.tx_id, parts.join(", "))
    } else {
        format!("Rollback incomplete for {} ({}).", rb.tx_id, parts.join(", "))
    };
    let mut restored_files = rb.restored_files.clone();
    restored_files.extend(rb.removed_created_files.iter().cloned());
    json!({
        "ok": ok,
        "tx_id": rb.tx_id,
        "message": message,
        "restored_files": restored_files,
        "removed_created_files": rb.removed_created_files,
        "skipped_or_conflicts": rb.skipped_or_conflicts
    })
}

fn infer_tx_workspace(explicit: Option<&str>, txs: &[WalManifest]) -> String {
    if let Some(ws) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return ws.to_string();
    }
    if let Some(ws) = txs
        .iter()
        .map(|t| t.workspace_root.trim())
        .find(|s| !s.is_empty())
    {
        return ws.to_string();
    }
    std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

fn handle_tx(workspace: Option<&str>, args: Value, started: Instant) -> Result<ToolCallOutput> {
    handle_tx_in(&HyperConfig::wal_dir(), workspace, args, started, true)
}

fn handle_tx_in(
    wal_dir: &Path,
    workspace: Option<&str>,
    args: Value,
    started: Instant,
    emit_telemetry: bool,
) -> Result<ToolCallOutput> {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
    match action {
        "list" => {
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(15) as usize;
            let txs = list_transactions(wal_dir, limit);
            let mut out = format!("# WAL transactions ({})\n", txs.len());
            for t in &txs {
                let files: Vec<&str> = t.files.iter().map(|f| f.rel_path.as_str()).collect();
                out.push_str(&format!(
                    "  {} [{}] {}\n",
                    t.tx_id,
                    if t.rolled_back { "rolled-back" } else { "committed" },
                    files.join(", ")
                ));
            }
            let ws_str = infer_tx_workspace(workspace, &txs);
            let tx_items: Vec<Value> = txs.iter().map(tx_manifest_to_json).collect();
            let selected_tx_id = args
                .get("tx_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .or_else(|| txs.first().map(|t| t.tx_id.clone()));

            let mut structured = json!({
                "action": "list",
                "workspace": ws_str,
                "transactions": tx_items
            });
            if let Some(sel_id) = &selected_tx_id {
                structured["selected_tx_id"] = json!(sel_id);
                if let Ok(diff_val) =
                    build_tx_diff_json(wal_dir, sel_id, 240, MAX_INLINE_LIST_DIFF_BYTES)
                {
                    structured["diff"] = diff_val;
                }
            }
            Ok(ToolCallOutput::with_structured(out, structured))
        }
        "diff" => {
            let tx_id = args
                .get("tx_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("diff requires 'tx_id'"))?;
            let max_lines = args
                .get("max_lines")
                .and_then(Value::as_u64)
                .unwrap_or(400) as usize;
            let diff_val = build_tx_diff_json(wal_dir, tx_id, max_lines, MAX_STRUCTURED_DIFF_BYTES)?;
            let txs = list_transactions(wal_dir, 15);
            let ws_str = infer_tx_workspace(workspace, &txs);
            let tx_items: Vec<Value> = txs.iter().map(tx_manifest_to_json).collect();

            let file_count = diff_val
                .get("files")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            let diff_text = diff_val
                .get("diff_text")
                .and_then(Value::as_str)
                .unwrap_or("");
            let text = format!("# WAL diff {} ({} file(s))\n{}", tx_id, file_count, diff_text);
            let structured = json!({
                "action": "diff",
                "workspace": ws_str,
                "transactions": tx_items,
                "selected_tx_id": tx_id,
                "diff": diff_val
            });
            Ok(ToolCallOutput::with_structured(text, structured))
        }
        "rollback" => {
            let tx_id = args
                .get("tx_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("rollback requires 'tx_id'"))?;
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
            let rb = rollback_transaction(wal_dir, tx_id, force)?;
            if emit_telemetry {
                record_telemetry(
                    "rollback",
                    workspace.map(String::from),
                    started.elapsed().as_millis() as u64,
                    0,
                    0,
                    format!("rollback {} ({} restored)", tx_id, rb.restored_files.len()),
                    false,
                );
            }
            let txs = list_transactions(wal_dir, 15);
            let ws_str = infer_tx_workspace(workspace, &txs);
            let tx_items: Vec<Value> = txs.iter().map(tx_manifest_to_json).collect();
            let structured = json!({
                "action": "rollback",
                "workspace": ws_str,
                "transactions": tx_items,
                "selected_tx_id": tx_id,
                "rollback": build_rollback_json(&rb)
            });
            Ok(ToolCallOutput::with_structured(
                serde_json::to_string_pretty(&rb)?,
                structured,
            ))
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
                        format!(
                            "{}-{} of {}",
                            d.offset_lines + 1,
                            d.offset_lines + d.returned_lines,
                            d.total_lines
                        )
                    };
                    let text = format!(
                        "# drill {} (`{}` · lines {})\n{}",
                        d.handle_id, d.label, range, d.content
                    );
                    let txs = list_transactions(wal_dir, 15);
                    let ws_str = infer_tx_workspace(workspace, &txs);
                    let tx_items: Vec<Value> = txs.iter().map(tx_manifest_to_json).collect();
                    let structured = json!({
                        "action": "drill",
                        "workspace": ws_str,
                        "transactions": tx_items,
                        "drill": {
                            "handle_id": d.handle_id,
                            "label": d.label,
                            "total_lines": d.total_lines,
                            "returned_lines": d.returned_lines,
                            "offset_lines": d.offset_lines,
                            "content": d.content
                        }
                    });
                    Ok(ToolCallOutput::with_structured(text, structured))
                }
                DaemonResponse::Err { message } => Err(anyhow!(message)),
            }
        }
        other => Err(anyhow!("unknown hyper_tx action '{}'", other)),
    }
}

fn handle_app_tx_diff(wal_dir: &Path, args: Value) -> Result<ToolCallOutput> {
    let tx_id = args
        .get("tx_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("hyper_app_tx_diff requires 'tx_id'"))?;
    let max_lines = args
        .get("max_lines")
        .and_then(Value::as_u64)
        .unwrap_or(400) as usize;
    let diff_val = build_tx_diff_json(wal_dir, tx_id, max_lines, MAX_STRUCTURED_DIFF_BYTES)?;
    let file_count = diff_val
        .get("files")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let text = format!("Loaded WAL diff for {} ({} file(s)).", tx_id, file_count);
    Ok(ToolCallOutput::with_structured(text, diff_val))
}

fn handle_app_tx_rollback(
    wal_dir: &Path,
    workspace: Option<&str>,
    args: Value,
    started: Instant,
) -> Result<ToolCallOutput> {
    handle_app_tx_rollback_in(wal_dir, workspace, args, started, true)
}

fn handle_app_tx_rollback_in(
    wal_dir: &Path,
    workspace: Option<&str>,
    args: Value,
    started: Instant,
    emit_telemetry: bool,
) -> Result<ToolCallOutput> {
    let tx_id = args
        .get("tx_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("hyper_app_tx_rollback requires 'tx_id'"))?;
    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
    let rb = rollback_transaction(wal_dir, tx_id, force)?;
    if emit_telemetry {
        record_telemetry(
            "rollback",
            workspace.map(String::from),
            started.elapsed().as_millis() as u64,
            0,
            0,
            format!("rollback {} ({} restored)", tx_id, rb.restored_files.len()),
            false,
        );
    }
    let structured = build_rollback_json(&rb);
    let msg = structured
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Rollback completed.")
        .to_string();
    Ok(ToolCallOutput::with_structured(msg, structured))
}

fn handle_app_set_profile(
    config_path: &Path,
    args: Value,
    notify_daemon: bool,
) -> Result<ToolCallOutput> {
    let raw = args
        .get("profile")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("hyper_app_set_profile requires 'profile'"))?;
    let profile = Profile::from_str_loose(raw).ok_or_else(|| {
        anyhow!(
            "invalid profile '{}'; expected observe|safe|balanced|hyper",
            raw
        )
    })?;
    let mut cfg = HyperConfig::load_from(config_path);
    cfg.apply_profile(profile);
    cfg.save_to(config_path)?;
    if notify_daemon {
        let _ = send_request(&DaemonRequest::SetProfile {
            profile: profile.as_str().to_string(),
        });
    }
    let name = profile.as_str();
    let structured = json!({
        "ok": true,
        "profile": name
    });
    Ok(ToolCallOutput::with_structured(
        format!("Profile switched to '{}'.", name),
        structured,
    ))
}

fn fallback_engine_status() -> EngineStatus {
    EngineStatus {
        pid: std::process::id(),
        version: hyper_core::version().into(),
        uptime_secs: 0,
        rss_mb: 0.0,
        active_workspaces: Vec::new(),
        handles_cached: 0,
        prewarm_cached: 0,
        ast_cache_entries: 0,
        ast_cache_hits: 0,
        ast_cache_misses: 0,
        pack_cache_hits: 0,
        pack_cache_misses: 0,
        tool_counts: HashMap::new(),
        total_calls: 0,
        total_saved_ms: 0,
        total_turns_saved: 0,
        blocked_security_events: 0,
        avg_tool_latency_ms: 0.0,
    }
}

fn format_subagent_rescue_prompt(s: &SubagentSnapshot, role: &str, state: &str) -> String {
    let short_id = &s.conversation_id[..s.conversation_id.len().min(8)];
    if s.error_loop || s.consecutive_error_streak >= 2 {
        let err_note = s
            .last_error_summary
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .map(|t| format!(" ({})", t.trim()))
            .unwrap_or_default();
        format!(
            "Subagent {} #{} hit {} consecutive errors{}. Verify what it already completed, kill the stuck subagent, and spawn a narrower fresh subagent for only the remaining work.",
            role, short_id, s.consecutive_error_streak, err_note
        )
    } else if s.stalled {
        format!(
            "Subagent {} #{} has been silent for {}s (likely hung). Inspect its latest progress, kill it if stuck, and continue only the remaining work in a fresh subagent.",
            role, short_id, s.idle_secs
        )
    } else {
        format!(
            "Inspect status of subagent {} #{} ({}, {} tokens) and collect its summary if ready.",
            role, short_id, state, s.total_tokens
        )
    }
}

fn build_cockpit_app_payload_with(
    args: &Value,
    cfg: &HyperConfig,
    wal_dir: &Path,
    ledger_path: &Path,
    try_daemon: bool,
) -> Value {
    let raw_window = args
        .get("usage_window")
        .and_then(Value::as_str)
        .unwrap_or("7d");
    let usage_window = UsageWindow::parse(raw_window);
    let audit_days = args
        .get("audit_days")
        .and_then(Value::as_u64)
        .unwrap_or(14)
        .clamp(1, 90) as u32;
    let explicit_ws = args
        .get("workspace")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let conv_id = args
        .get("conversation_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let allow_daemon = try_daemon && std::env::var_os("AGY_HYPER_BRAIN_DIR").is_none();

    let snapshot: CockpitSnapshot = if allow_daemon {
        match send_request_with_timeout(
            &DaemonRequest::Snapshot {
                conversation_id: conv_id.map(String::from),
                transcript_path: None,
            },
            1500,
        ) {
            Ok(DaemonResponse::Ok { data }) => serde_json::from_value(data).unwrap_or_else(|_| {
                build_cockpit_snapshot(
                    fallback_engine_status(),
                    cfg.clone(),
                    Vec::new(),
                    None,
                    None,
                    conv_id,
                    None,
                )
            }),
            _ => build_cockpit_snapshot(
                fallback_engine_status(),
                cfg.clone(),
                Vec::new(),
                None,
                None,
                conv_id,
                None,
            ),
        }
    } else {
        build_cockpit_snapshot(
            fallback_engine_status(),
            cfg.clone(),
            Vec::new(),
            None,
            None,
            conv_id,
            None,
        )
    };

    let usage_report: UsageWindowReport = if allow_daemon {
        match send_request_with_timeout(
            &DaemonRequest::UsageSummary {
                window: usage_window.as_str().to_string(),
                currency: None,
                tz_offset_mins: Some(0),
                force_sync: false,
            },
            1500,
        ) {
            Ok(DaemonResponse::Ok { data }) => serde_json::from_value(data).unwrap_or_else(|_| {
                let mut ledger = UsageLedger::load_from(ledger_path);
                let now_ms = now_epoch_ms();
                let outcome = ledger.sync_from_dirs(&default_brain_dirs(), now_ms);
                ledger.query_window(usage_window, cfg, None, 0, now_ms, outcome.scan_ms)
            }),
            _ => {
                let mut ledger = UsageLedger::load_from(ledger_path);
                let now_ms = now_epoch_ms();
                let outcome = ledger.sync_from_dirs(&default_brain_dirs(), now_ms);
                ledger.query_window(usage_window, cfg, None, 0, now_ms, outcome.scan_ms)
            }
        }
    } else {
        let mut ledger = UsageLedger::load_from(ledger_path);
        let now_ms = now_epoch_ms();
        let outcome = ledger.sync_from_dirs(&default_brain_dirs(), now_ms);
        ledger.query_window(usage_window, cfg, None, 0, now_ms, outcome.scan_ms)
    };

    let audit_report: BurnAuditReport = if allow_daemon {
        match send_request_with_timeout(
            &DaemonRequest::AuditReport {
                days: audit_days,
                max_sessions: 80,
                tz_offset_mins: 0,
            },
            2000,
        ) {
            Ok(DaemonResponse::Ok { data }) => serde_json::from_value(data)
                .unwrap_or_else(|_| build_audit_report(audit_days, 80, cfg, 0)),
            _ => build_audit_report(audit_days, 80, cfg, 0),
        }
    } else {
        build_audit_report(audit_days, 80, cfg, 0)
    };

    let txs = list_transactions(wal_dir, 15);
    let workspace = explicit_ws
        .map(String::from)
        .or_else(|| {
            snapshot
                .engine
                .active_workspaces
                .first()
                .map(|(w, _)| w.clone())
        })
        .or_else(|| {
            txs.iter()
                .map(|t| t.workspace_root.trim())
                .find(|s| !s.is_empty())
                .map(String::from)
        })
        .or_else(|| {
            let cwd = std::env::current_dir().ok()?;
            let exe = std::env::current_exe().ok();
            let home = dirs::home_dir();
            let env_root = env_plugin_root();
            if unsafe_default_cwd_reason(&cwd, exe.as_deref(), home.as_deref(), env_root.as_deref())
                .is_none()
            {
                Some(cwd.display().to_string())
            } else {
                None
            }
        })
        .unwrap_or_default();

    let pulse = snapshot.pulse.as_ref();
    let health_level = match pulse.map(|p| p.health).unwrap_or(HealthLevel::Green) {
        HealthLevel::Green => "green",
        HealthLevel::Yellow => "yellow",
        HealthLevel::Red => "red",
    };
    let context_tokens = pulse.map(|p| p.total_tokens).unwrap_or(0);
    let context_limit = pulse
        .map(|p| {
            if p.model_context_limit > 0 {
                p.model_context_limit
            } else if p.effective_red_tokens > 0 {
                p.effective_red_tokens
            } else {
                1_000_000
            }
        })
        .unwrap_or(1_000_000);
    let cache_ratio = pulse
        .filter(|p| p.total_tokens > 0)
        .map(|p| p.cache_hit_ratio)
        .or_else(|| snapshot.benchmark.as_ref().map(|b| b.cache_hit_ratio))
        .unwrap_or(0.0);
    let cache_ttl_secs = pulse
        .map(|p| p.effective_cache_ttl_secs)
        .unwrap_or(cfg.cache_warm_window_secs);
    let cache_ttl_source = pulse
        .map(|p| p.cache_ttl_source.clone())
        .unwrap_or_else(|| "default".to_string());
    let cache_age_secs = pulse
        .and_then(|p| p.seconds_since_last_planner)
        .unwrap_or(0);
    let active_model = pulse
        .and_then(|p| p.model.clone())
        .unwrap_or_default();

    let subagents: Vec<Value> = snapshot
        .session_tree
        .as_ref()
        .map(|tree| {
            tree.subagents
                .iter()
                .map(|s| {
                    let role = if !s.agent_name.trim().is_empty() {
                        s.agent_name.trim().to_string()
                    } else if !s.title.trim().is_empty() {
                        s.title.trim().to_string()
                    } else {
                        "subagent".to_string()
                    };
                    let state = if s.error_loop {
                        "error_loop"
                    } else if s.stalled {
                        "stalled"
                    } else if s.active {
                        "active"
                    } else {
                        "done"
                    };
                    let rescue_prompt = format_subagent_rescue_prompt(s, &role, state);
                    json!({
                        "id": s.conversation_id,
                        "role": role,
                        "state": state,
                        "stalled": s.stalled,
                        "crash_loop_count": s.consecutive_error_streak,
                        "context_tokens": s.total_tokens,
                        "rescue_prompt": rescue_prompt
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let fx_div = if usage_report.fx_rate > 0.0 {
        usage_report.fx_rate
    } else {
        1.0
    };
    let daily: Vec<Value> = usage_report
        .series
        .iter()
        .map(|pt| {
            let prompt_denom = pt.input_tokens.saturating_add(pt.cache_read_tokens);
            let hit_pct = if prompt_denom > 0 {
                round1((pt.cache_read_tokens as f64 / prompt_denom as f64) * 100.0)
            } else {
                0.0
            };
            json!({
                "date": pt.iso_key,
                "tokens": pt.total_tokens,
                "cache_hit_pct": hit_pct,
                "cost_usd": round4(pt.estimated_cost / fx_div)
            })
        })
        .collect();

    let top_findings: Vec<String> = audit_report
        .findings
        .iter()
        .take(5)
        .map(|f| {
            if f.detail.trim().is_empty() {
                f.title.clone()
            } else {
                format!("{}: {}", f.title, f.detail)
            }
        })
        .collect();

    let recent_txs: Vec<Value> = txs
        .iter()
        .take(15)
        .map(|t| {
            json!({
                "tx_id": t.tx_id,
                "created_at": format_epoch_ms_rfc3339(t.timestamp_ms),
                "source": t.source,
                "summary": format_tx_summary(t),
                "files_count": t.files.len(),
                "rolled_back": t.rolled_back
            })
        })
        .collect();

    json!({
        "version": hyper_core::version(),
        "profile": cfg.profile.as_str(),
        "workspace": workspace,
        "health": {
            "level": health_level,
            "context_tokens": context_tokens,
            "context_limit": context_limit,
            "cache_hit_pct": round1(cache_ratio * 100.0),
            "cache_ttl_secs": cache_ttl_secs,
            "cache_ttl_source": cache_ttl_source,
            "cache_age_secs": cache_age_secs,
            "active_model": active_model
        },
        "subagents": subagents,
        "usage": {
            "window": usage_report.window,
            "total_input": usage_report.total_input_tokens,
            "total_cache_read": usage_report.total_cache_read_tokens,
            "total_output": usage_report.total_output_tokens,
            "est_cost_usd": usage_report.estimated_cost_usd,
            "currency": usage_report.currency,
            "daily": daily
        },
        "audit": {
            "grade": audit_report.verdict,
            "cache_hit_pct": round1(audit_report.overall_cache_hit_ratio * 100.0),
            "first_edit_p50_tokens": audit_report.first_edit_p50_tokens.unwrap_or(0),
            "split_cache_drop_pct": audit_report.split_cache.headline_cache_drop_pct,
            "top_findings": top_findings
        },
        "recent_txs": recent_txs
    })
}

fn format_cockpit_text_summary(payload: &Value) -> String {
    let ver = payload["version"].as_str().unwrap_or(hyper_core::version());
    let profile = payload["profile"].as_str().unwrap_or("balanced");
    let health = &payload["health"];
    let level = health["level"].as_str().unwrap_or("green");
    let ctx_tok = health["context_tokens"].as_u64().unwrap_or(0);
    let ctx_lim = health["context_limit"].as_u64().unwrap_or(1_000_000);
    let cache_pct = health["cache_hit_pct"].as_f64().unwrap_or(0.0);
    let ttl_secs = health["cache_ttl_secs"].as_u64().unwrap_or(240);
    let model = health["active_model"].as_str().unwrap_or("");
    let sub_arr = payload["subagents"].as_array();
    let sub_count = sub_arr.map(Vec::len).unwrap_or(0);
    let unhealthy_subs = sub_arr
        .map(|arr| {
            arr.iter()
                .filter(|s| {
                    s["stalled"].as_bool().unwrap_or(false)
                        || s["crash_loop_count"].as_u64().unwrap_or(0) >= 2
                })
                .count()
        })
        .unwrap_or(0);
    let usage = &payload["usage"];
    let u_win = usage["window"].as_str().unwrap_or("7d");
    let est_usd = usage["est_cost_usd"].as_f64().unwrap_or(0.0);
    let audit = &payload["audit"];
    let grade = audit["grade"].as_str().unwrap_or("HEALTHY");
    let tx_count = payload["recent_txs"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0);

    let model_note = if model.is_empty() {
        "n/a".to_string()
    } else {
        model.to_string()
    };
    let text = format!(
        "# AGY Hyper Cockpit v{} (profile: {})\n\
         Health: {} · Context: {:.1}k/{:.0}k · Cache: {:.1}% (TTL {}s) · Model: {}\n\
         Subagents: {} ({} needing attention) · Usage ({}): ${:.2} USD · Audit: {} · WAL TXs: {}",
        ver,
        profile,
        level.to_uppercase(),
        ctx_tok as f64 / 1000.0,
        ctx_lim as f64 / 1000.0,
        cache_pct,
        ttl_secs,
        model_note,
        sub_count,
        unhealthy_subs,
        u_win,
        est_usd,
        grade,
        tx_count
    );
    let (capped, _) = truncate_utf8_bytes(&text, MAX_COCKPIT_TEXT_BYTES);
    capped.to_string()
}

fn handle_cockpit(args: Value, cfg: &HyperConfig) -> Result<ToolCallOutput> {
    let payload = build_cockpit_app_payload_with(
        &args,
        cfg,
        &HyperConfig::wal_dir(),
        &HyperConfig::ledger_path(),
        true,
    );
    let summary = format_cockpit_text_summary(&payload);
    Ok(ToolCallOutput::with_structured(summary, payload))
}

fn handle_app_cockpit_data(args: Value, cfg: &HyperConfig) -> Result<ToolCallOutput> {
    let payload = build_cockpit_app_payload_with(
        &args,
        cfg,
        &HyperConfig::wal_dir(),
        &HyperConfig::ledger_path(),
        true,
    );
    let summary = format_cockpit_text_summary(&payload);
    Ok(ToolCallOutput::with_structured(summary, payload))
}

fn lookup_prewarm_singleflight(workspace: &str, preset: &str) -> Option<PrewarmedCheck> {
    let req = DaemonRequest::LookupPrewarm {
        workspace: workspace.to_string(),
        preset: preset.to_string(),
    };
    let timeout = Duration::from_secs(26);

    #[cfg(unix)]
    let resp: DaemonResponse = {
        use std::os::unix::net::UnixStream;
        let sock = socket_path();
        let mut stream = UnixStream::connect(sock).ok()?;
        let _ = stream.set_read_timeout(Some(timeout));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
        let mut payload = serde_json::to_string(&req).ok()?;
        payload.push('\n');
        stream.write_all(payload.as_bytes()).ok()?;
        stream.flush().ok()?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        serde_json::from_str(line.trim()).ok()?
    };

    #[cfg(not(unix))]
    let resp: DaemonResponse = {
        use std::net::TcpStream;
        let _ = socket_path();
        let run_dir = HyperConfig::run_dir();
        let addr = std::fs::read_to_string(run_dir.join("agy-hyper-v1.addr")).ok()?;
        let token = std::fs::read_to_string(run_dir.join("agy-hyper-v1.token")).ok()?;
        let mut stream = TcpStream::connect(addr.trim()).ok()?;
        let _ = stream.set_read_timeout(Some(timeout));
        let payload = format!(
            "{}\n{}\n",
            token.trim(),
            serde_json::to_string(&req).ok()?
        );
        stream.write_all(payload.as_bytes()).ok()?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        serde_json::from_str(line.trim()).ok()?
    };

    match resp {
        DaemonResponse::Ok { data } if !data.is_null() => serde_json::from_value(data).ok(),
        _ => None,
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

fn tool_definitions(cfg: &HyperConfig) -> Value {
    let all = vec![
        json!({
            "name": "hyper_pack",
            "description": "Semantic context packer (Tree-sitter AST outlines + inline matched symbol slices + single-pass ripgrep + 1-hop import neighbors + read-only gix git signals) packed strictly <= 8.5KB inline.",
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Absolute project workspace root path (required when host spawns MCP outside the project directory)" },
                    "query": { "type": "string", "description": "Task keywords or symbol names to pack" },
                    "symbols": { "type": "array", "items": { "type": "string" }, "description": "Optional symbol names to inline full AST-sliced bodies for (eliminates follow-up read_file)." },
                    "focus_paths": { "type": "array", "items": { "type": "string" } },
                    "include_outlines": { "type": "boolean", "default": true },
                    "include_git": { "type": "boolean", "default": true },
                    "max_bytes": { "type": "integer", "default": 8200 }
                }
            }
        }),
        json!({
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
        }),
        json!({
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
        }),
        json!({
            "name": "hyper_check",
            "description": "Synchronous shell-free preset/argv verification runner (<=60s in-loop to preserve prompt cache) with smart idle/fatal-hang watchdog, head+tail ring buffer, error digest, process-tree cleanup, and LRU spill handles.",
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string" },
                    "list_presets": { "type": "boolean" },
                    "preset": { "type": "string" },
                    "argv": { "type": "array", "items": { "type": "string" } },
                    "timeout_secs": { "type": "integer", "default": 60 },
                    "idle_timeout_secs": { "type": "integer", "description": "Optional silence watchdog seconds (default: off for compile/static presets, 45s for tests/scripts)" },
                    "fail_fast": { "type": "boolean", "description": "Abort hung runs on fatal stderr + 4s silence (default: false for compile/static checks, true for tests/scripts)" },
                    "cwd": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "hyper_tx",
            "description": "Inspect WAL edit transactions, view unified diffs, atomically rollback a tx_id, or 1ms drill into spilled check handles.",
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false },
            "_meta": {
                "ui": {
                    "resourceUri": TX_DIFF_RESOURCE_URI
                }
            },
            "inputSchema": {
                "type": "object",
                "required": ["action"],
                "properties": {
                    "workspace": { "type": "string" },
                    "action": { "type": "string", "enum": ["list", "diff", "rollback", "drill"] },
                    "tx_id": { "type": "string" },
                    "force": { "type": "boolean" },
                    "handle_id": { "type": "string" },
                    "offset_lines": { "type": "integer" },
                    "limit_lines": { "type": "integer" },
                    "grep": { "type": "string" },
                    "limit": { "type": "integer" },
                    "max_lines": { "type": "integer" }
                }
            }
        }),
        json!({
            "name": "hyper_cockpit",
            "description": "Open the AGY Hyper Cockpit telemetry view (context warmth, subagent watchdog, usage ledger, burn audit, and recent WAL transactions). All arguments are optional.",
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
            "_meta": {
                "ui": {
                    "resourceUri": COCKPIT_RESOURCE_URI
                },
                "com.google.antigravity/ui": {
                    "entrypoints": [
                        { "type": "aux_pane", "title": "AGY Hyper Cockpit" },
                        { "type": "global", "title": "AGY Hyper" }
                    ]
                }
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "usage_window": { "type": "string", "enum": ["24h", "7d", "30d", "90d"], "default": "7d" },
                    "audit_days": { "type": "integer", "default": 14 },
                    "workspace": { "type": "string" },
                    "conversation_id": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "hyper_app_tx_diff",
            "description": "App-only: load unified diff and file breakdown for a WAL transaction.",
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
            "_meta": {
                "ui": {
                    "visibility": ["app"]
                }
            },
            "inputSchema": {
                "type": "object",
                "required": ["tx_id"],
                "properties": {
                    "tx_id": { "type": "string" },
                    "max_lines": { "type": "integer", "default": 400 }
                }
            }
        }),
        json!({
            "name": "hyper_app_tx_rollback",
            "description": "App-only: safely rollback a WAL transaction by tx_id.",
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false },
            "_meta": {
                "ui": {
                    "visibility": ["app"]
                }
            },
            "inputSchema": {
                "type": "object",
                "required": ["tx_id"],
                "properties": {
                    "tx_id": { "type": "string" },
                    "force": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "hyper_app_cockpit_data",
            "description": "App-only: fetch live AGY Hyper Cockpit telemetry, subagent watchdog, usage ledger, burn audit, and WAL history.",
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
            "_meta": {
                "ui": {
                    "visibility": ["app"]
                }
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "usage_window": { "type": "string", "enum": ["24h", "7d", "30d", "90d"], "default": "7d" },
                    "audit_days": { "type": "integer", "default": 14 },
                    "workspace": { "type": "string" },
                    "conversation_id": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "hyper_app_set_profile",
            "description": "App-only: switch active AGY Hyper profile (observe|safe|balanced|hyper).",
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
            "_meta": {
                "ui": {
                    "visibility": ["app"]
                }
            },
            "inputSchema": {
                "type": "object",
                "required": ["profile"],
                "properties": {
                    "profile": { "type": "string", "enum": ["observe", "safe", "balanced", "hyper"] }
                }
            }
        }),
    ];
    let filtered: Vec<Value> = all
        .into_iter()
        .filter(|def| {
            def.get("name")
                .and_then(Value::as_str)
                .map(|n| cfg.is_mcp_tool_enabled(n))
                .unwrap_or(true)
        })
        .collect();
    Value::Array(filtered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_workspace_guard_blocks_plugin_root_home_and_fs_root() {
        let tmp = std::env::temp_dir().join(format!("agy-hyper-mcp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let home = tmp.join("home");
        let plugin_root = home.join(".gemini/config/plugins/agy-hyper");
        let plugin_bin = plugin_root.join("bin");
        let source_repo = tmp.join("src/agy-hyper");
        let user_project = tmp.join("work/my-app");
        std::fs::create_dir_all(&plugin_bin).unwrap();
        std::fs::create_dir_all(&source_repo).unwrap();
        std::fs::create_dir_all(&user_project).unwrap();
        std::fs::write(plugin_root.join("plugin.json"), r#"{"name":"agy-hyper"}"#).unwrap();
        std::fs::write(plugin_bin.join("agy-hyper"), b"").unwrap();
        std::fs::write(source_repo.join("Cargo.toml"), "[workspace]\n").unwrap();
        let exe = plugin_bin.join("agy-hyper");

        // 1. Omitted workspace when cwd == installed PLUGIN_ROOT -> fail fast
        let err = resolve_mcp_workspace(None, false, Some(&plugin_root), Some(&exe), Some(&home), None)
            .unwrap_err();
        assert!(err.to_string().contains("installed plugin directory"), "got: {}", err);

        // 2. Dot workspace "." when cwd == installed PLUGIN_ROOT -> fail fast
        let err = resolve_mcp_workspace(Some("."), false, Some(&plugin_root), Some(&exe), Some(&home), None)
            .unwrap_err();
        assert!(err.to_string().contains("installed plugin directory"), "got: {}", err);

        // 3. Explicit workspace targeting installed PLUGIN_ROOT -> blocked
        let err = resolve_mcp_workspace(
            Some(plugin_root.to_str().unwrap()),
            false,
            Some(&user_project),
            Some(&exe),
            Some(&home),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("refusing to target installed plugin directory"), "got: {}", err);

        // 4. Omitted workspace when cwd == $HOME -> fail fast
        let err = resolve_mcp_workspace(None, false, Some(&home), Some(&exe), Some(&home), None)
            .unwrap_err();
        assert!(err.to_string().contains("user home directory"), "got: {}", err);

        // 5. Omitted workspace when cwd == real project directory -> allowed (CLI compatibility)
        let ok = resolve_mcp_workspace(None, false, Some(&user_project), Some(&exe), Some(&home), None)
            .unwrap();
        assert_eq!(PathBuf::from(ok), user_project);

        // 6. Explicit workspace targeting the agy-hyper source repo (has Cargo.toml) -> allowed
        let ok = resolve_mcp_workspace(
            Some(source_repo.to_str().unwrap()),
            false,
            Some(&plugin_root),
            Some(&exe),
            Some(&home),
            None,
        )
        .unwrap();
        assert_eq!(PathBuf::from(ok), source_repo);

        // 7. Filesystem root as omitted cwd -> fail fast
        #[cfg(unix)]
        {
            let err = resolve_mcp_workspace(None, false, Some(Path::new("/")), Some(&exe), Some(&home), None)
                .unwrap_err();
            assert!(err.to_string().contains("filesystem root"), "got: {}", err);
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn hyper_pack_schema_exposes_symbols_array_property() {
        let cfg = HyperConfig::default();
        let defs = tool_definitions(&cfg);
        let tools = defs.as_array().unwrap();
        let pack = tools
            .iter()
            .find(|t| t.get("name").and_then(Value::as_str) == Some("hyper_pack"))
            .expect("hyper_pack definition present");
        let symbols_prop = &pack["inputSchema"]["properties"]["symbols"];
        assert_eq!(symbols_prop["type"], "array");
        assert_eq!(symbols_prop["items"]["type"], "string");
        assert!(symbols_prop["description"]
            .as_str()
            .unwrap()
            .contains("AST-sliced bodies"));
    }

    #[test]
    fn initialize_declares_resources_and_tools_capabilities() {
        let init = initialize_result();
        assert_eq!(init["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert!(init["capabilities"]["tools"].is_object());
        assert_eq!(init["capabilities"]["resources"], json!({}));
        assert_eq!(init["serverInfo"]["name"], "agy-hyper");
    }

    #[test]
    fn resources_list_and_read_follow_resolution_chain_and_size_guards() {
        let listed = resource_definitions();
        let arr = listed.as_array().expect("resources array");
        assert_eq!(arr.len(), 2);
        for expected_uri in [TX_DIFF_RESOURCE_URI, COCKPIT_RESOURCE_URI] {
            let item = arr
                .iter()
                .find(|r| r["uri"] == expected_uri)
                .expect("expected UI resource");
            assert_eq!(item["mimeType"], MCP_APP_MIME_TYPE);
            assert_eq!(item["_meta"]["ui"]["prefersBorder"], false);
        }

        let tmp = std::env::temp_dir().join(format!(
            "agy-hyper-mcp-res-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let env_dir = tmp.join("override_apps");
        let plugin_root = tmp.join("plugin_root");
        let exe_dir = tmp.join("bin_parent/bin");
        let sibling_apps = tmp.join("bin_parent/mcp_apps");
        let empty_home = tmp.join("empty_home");
        std::fs::create_dir_all(&env_dir).unwrap();
        std::fs::create_dir_all(plugin_root.join("mcp_apps")).unwrap();
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(&sibling_apps).unwrap();
        std::fs::create_dir_all(&empty_home).unwrap();
        let fake_exe = exe_dir.join("agy-hyper");
        std::fs::write(&fake_exe, b"").unwrap();

        // 1. Built-in fallback when no files exist
        let fb = read_ui_resource_with_context(
            TX_DIFF_RESOURCE_URI,
            None,
            None,
            Some(&fake_exe),
            Some(&empty_home),
        )
        .unwrap();
        let fb_item = &fb["contents"][0];
        assert_eq!(fb_item["uri"], TX_DIFF_RESOURCE_URI);
        assert_eq!(fb_item["mimeType"], MCP_APP_MIME_TYPE);
        assert_eq!(fb_item["_meta"]["ui"]["prefersBorder"], false);
        assert!(fb_item["text"].as_str().unwrap().contains("<!DOCTYPE html>"));

        // 2. Sibling ../mcp_apps relative to binary
        std::fs::write(sibling_apps.join("tx-diff.html"), "<!DOCTYPE html><div>sibling</div>")
            .unwrap();
        let sib = read_ui_resource_with_context(
            TX_DIFF_RESOURCE_URI,
            None,
            None,
            Some(&fake_exe),
            Some(&empty_home),
        )
        .unwrap();
        assert!(sib["contents"][0]["text"].as_str().unwrap().contains("sibling"));

        // 3. PLUGIN_ROOT overrides sibling
        std::fs::write(
            plugin_root.join("mcp_apps/tx-diff.html"),
            "<!DOCTYPE html><div>plugin-root</div>",
        )
        .unwrap();
        let pr = read_ui_resource_with_context(
            TX_DIFF_RESOURCE_URI,
            None,
            Some(&plugin_root),
            Some(&fake_exe),
            Some(&empty_home),
        )
        .unwrap();
        assert!(pr["contents"][0]["text"].as_str().unwrap().contains("plugin-root"));

        // 4. AGY_HYPER_MCP_APPS_DIR overrides PLUGIN_ROOT
        std::fs::write(env_dir.join("tx-diff.html"), "<!DOCTYPE html><div>env-override</div>")
            .unwrap();
        let ov = read_ui_resource_with_context(
            TX_DIFF_RESOURCE_URI,
            Some(&env_dir),
            Some(&plugin_root),
            Some(&fake_exe),
            Some(&empty_home),
        )
        .unwrap();
        assert!(ov["contents"][0]["text"].as_str().unwrap().contains("env-override"));

        // 5. Empty file rejected
        std::fs::write(env_dir.join("cockpit.html"), b"").unwrap();
        let empty_err = read_ui_resource_with_context(
            COCKPIT_RESOURCE_URI,
            Some(&env_dir),
            None,
            Some(&fake_exe),
            Some(&empty_home),
        )
        .unwrap_err();
        assert!(matches!(empty_err, ResourceReadError::Invalid(_)));

        // 6. >= 10 MiB payload rejected
        let oversized = vec![b'a'; MAX_MCP_APP_BYTES];
        let size_err = validate_ui_resource_bytes(&oversized, "oversized.html").unwrap_err();
        assert!(matches!(size_err, ResourceReadError::Invalid(_)));

        // 7. Unknown URI returns NotFound
        let nf = read_ui_resource_with_context(
            "ui://agy-hyper/missing.html",
            None,
            None,
            Some(&fake_exe),
            Some(&empty_home),
        )
        .unwrap_err();
        assert!(matches!(nf, ResourceReadError::NotFound(_)));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn tools_list_exposes_mcp_apps_meta_entrypoints_and_app_only_visibility() {
        let mut observe_cfg = HyperConfig::default();
        observe_cfg.apply_profile(Profile::Observe);
        let defs = tool_definitions(&observe_cfg);
        let tools = defs.as_array().unwrap();

        // Model code-accel tools hidden in Observe, but UI/cockpit/app-only tools stay enabled
        assert!(tools.iter().all(|t| t["name"] != "hyper_pack"));
        assert!(tools.iter().all(|t| t["name"] != "hyper_edit"));

        let tx = tools
            .iter()
            .find(|t| t["name"] == "hyper_tx")
            .expect("hyper_tx present in observe");
        assert_eq!(tx["_meta"]["ui"]["resourceUri"], TX_DIFF_RESOURCE_URI);

        let cockpit = tools
            .iter()
            .find(|t| t["name"] == "hyper_cockpit")
            .expect("hyper_cockpit present in observe");
        assert_eq!(cockpit["_meta"]["ui"]["resourceUri"], COCKPIT_RESOURCE_URI);
        let entrypoints = cockpit["_meta"]["com.google.antigravity/ui"]["entrypoints"]
            .as_array()
            .expect("entrypoints array");
        assert_eq!(entrypoints.len(), 2);
        assert_eq!(entrypoints[0]["type"], "aux_pane");
        assert_eq!(entrypoints[0]["title"], "AGY Hyper Cockpit");
        assert_eq!(entrypoints[1]["type"], "global");
        assert_eq!(entrypoints[1]["title"], "AGY Hyper");
        assert!(cockpit["inputSchema"].get("required").is_none());

        for app_tool in [
            "hyper_app_tx_diff",
            "hyper_app_tx_rollback",
            "hyper_app_cockpit_data",
            "hyper_app_set_profile",
        ] {
            let def = tools
                .iter()
                .find(|t| t["name"] == app_tool)
                .unwrap_or_else(|| panic!("missing app-only tool {}", app_tool));
            assert_eq!(def["_meta"]["ui"]["visibility"], json!(["app"]));
        }
    }

    #[test]
    fn hyper_tx_and_app_tx_tools_emit_contract_structured_content_and_rollback() {
        let tmp = std::env::temp_dir().join(format!(
            "agy-hyper-mcp-tx-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let ws = tmp.join("repo");
        let wal = tmp.join("wal");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        let file = ws.join("src/lib.rs");
        std::fs::write(&file, "pub fn greeting() -> &'static str {\n    \"hello\"\n}\n").unwrap();

        let jail = WorkspaceJail::new(ws.to_str().unwrap(), false, true).unwrap();
        let tx_res = apply_transactional_edits_with_writer(
            &jail,
            &[FileEditOp {
                path: "src/lib.rs".into(),
                old_text: Some("\"hello\"".into()),
                new_text: Some("\"world\"".into()),
                symbol: None,
                create_content: None,
                expected_etag: None,
                replace_all: false,
            }],
            &wal,
            50,
            Some("test-writer"),
        )
        .unwrap();

        // 1. hyper_tx action=list
        let list_out = handle_tx_in(
            &wal,
            Some(ws.to_str().unwrap()),
            json!({ "action": "list", "limit": 10 }),
            Instant::now(),
            false,
        )
        .unwrap();
        assert!(list_out.text.contains("# WAL transactions (1)"));
        assert!(list_out.text.contains(&tx_res.tx_id));
        let sc = list_out.structured_content.expect("structuredContent present");
        assert_eq!(sc["action"], "list");
        assert_eq!(sc["selected_tx_id"], tx_res.tx_id);
        let txs = sc["transactions"].as_array().unwrap();
        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0]["tx_id"], tx_res.tx_id);
        assert_eq!(txs[0]["source"], "hyper_edit");
        assert_eq!(txs[0]["rolled_back"], false);
        assert!(txs[0]["created_at"].as_str().unwrap().contains('T'));
        assert_eq!(txs[0]["files"][0]["path"], "src/lib.rs");
        assert!(!txs[0]["files"][0]["abs_path"].as_str().unwrap().is_empty());
        assert_eq!(txs[0]["files"][0]["added"], 1);
        assert_eq!(txs[0]["files"][0]["removed"], 1);
        assert!(sc["diff"]["diff_text"].as_str().unwrap().contains("-\"hello\"")
            || sc["diff"]["diff_text"].as_str().unwrap().contains("\"world\""));

        // 2. hyper_app_tx_diff
        let diff_out = handle_app_tx_diff(&wal, json!({ "tx_id": tx_res.tx_id })).unwrap();
        let diff_sc = diff_out.structured_content.expect("diff structuredContent");
        assert_eq!(diff_sc["tx_id"], tx_res.tx_id);
        assert!(diff_sc["diff_text"].as_str().unwrap().contains("+\"world\"")
            || diff_sc["diff_text"].as_str().unwrap().contains("+    \"world\""));
        assert_eq!(diff_sc["files"][0]["path"], "src/lib.rs");
        assert_eq!(diff_sc["files"][0]["added"], 1);
        assert_eq!(diff_sc["files"][0]["removed"], 1);

        // 3. hyper_app_tx_rollback
        let rb_out = handle_app_tx_rollback_in(
            &wal,
            Some(ws.to_str().unwrap()),
            json!({ "tx_id": tx_res.tx_id }),
            Instant::now(),
            false,
        )
        .unwrap();
        let rb_sc = rb_out.structured_content.expect("rollback structuredContent");
        assert_eq!(rb_sc["ok"], true);
        assert_eq!(rb_sc["tx_id"], tx_res.tx_id);
        assert_eq!(rb_sc["restored_files"], json!(["src/lib.rs"]));
        let restored_body = std::fs::read_to_string(&file).unwrap();
        assert!(restored_body.contains("\"hello\""));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn hyper_cockpit_empty_call_and_app_set_profile_match_contract() {
        let tmp = std::env::temp_dir().join(format!(
            "agy-hyper-mcp-cockpit-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let wal = tmp.join("wal");
        let ledger = tmp.join("usage_ledger.json");
        let cfg_path = tmp.join("config.json");

        // Test hyper_app_set_profile
        let set_out = handle_app_set_profile(&cfg_path, json!({ "profile": "safe" }), false).unwrap();
        let set_sc = set_out.structured_content.unwrap();
        assert_eq!(set_sc["ok"], true);
        assert_eq!(set_sc["profile"], "safe");
        let loaded = HyperConfig::load_from(&cfg_path);
        assert_eq!(loaded.profile, Profile::Safe);
        assert!(handle_app_set_profile(&cfg_path, json!({ "profile": "invalid" }), false).is_err());

        // Test hyper_cockpit / hyper_app_cockpit_data with empty `{}` args (no daemon required)
        let payload = build_cockpit_app_payload_with(&json!({}), &loaded, &wal, &ledger, false);
        assert_eq!(payload["version"], hyper_core::version());
        assert_eq!(payload["profile"], "safe");
        assert!(payload["workspace"].is_string());

        let health = &payload["health"];
        assert!(matches!(
            health["level"].as_str(),
            Some("green" | "yellow" | "red")
        ));
        assert!(health["context_tokens"].is_u64());
        assert!(health["context_limit"].as_u64().unwrap_or(0) > 0);
        assert!(health["cache_hit_pct"].is_f64());
        assert!(health["cache_ttl_secs"].is_u64());
        assert!(health["cache_ttl_source"].is_string());
        assert!(health["cache_age_secs"].is_u64());
        assert!(health["active_model"].is_string());

        assert!(payload["subagents"].is_array());
        assert_eq!(payload["usage"]["window"], "7d");
        assert!(payload["usage"]["total_input"].is_u64());
        assert!(payload["usage"]["total_cache_read"].is_u64());
        assert!(payload["usage"]["total_output"].is_u64());
        assert!(payload["usage"]["est_cost_usd"].is_f64());
        assert!(payload["usage"]["currency"].is_string());
        assert!(payload["usage"]["daily"].is_array());

        assert!(payload["audit"]["grade"].is_string());
        assert!(payload["audit"]["cache_hit_pct"].is_f64());
        assert!(payload["audit"]["first_edit_p50_tokens"].is_u64());
        assert!(payload["audit"]["split_cache_drop_pct"].is_f64());
        assert!(payload["audit"]["top_findings"].is_array());

        assert!(payload["recent_txs"].is_array());

        let text = format_cockpit_text_summary(&payload);
        assert!(text.len() <= MAX_COCKPIT_TEXT_BYTES);
        assert!(text.contains("AGY Hyper Cockpit"));
        assert!(text.contains("profile: safe"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn ssh_remote_workspace_guard_rejects_remote_uris_and_missing_remote_paths() {
        let tmp = std::env::temp_dir().join(format!(
            "agy-hyper-mcp-ssh-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let existing_proj = tmp.join("remote-repo");
        let fake_home = tmp.join("home");
        std::fs::create_dir_all(&existing_proj).unwrap();
        std::fs::create_dir_all(&fake_home).unwrap();

        // 1. Remote URI prefixes rejected cleanly
        let err1 = resolve_mcp_workspace_with_ssh(
            Some("vscode-remote://ssh-remote+devbox/home/dev/app"),
            false,
            Some(&existing_proj),
            None,
            Some(&fake_home),
            None,
            None,
        )
        .unwrap_err();
        assert!(err1.to_string().contains("vscode-remote://"), "got: {}", err1);
        assert!(err1.to_string().contains("not a local filesystem path"), "got: {}", err1);

        let err2 = resolve_mcp_workspace_with_ssh(
            Some("ssh-remote://devbox/home/dev/app"),
            false,
            Some(&existing_proj),
            None,
            Some(&fake_home),
            None,
            None,
        )
        .unwrap_err();
        assert!(err2.to_string().contains("ssh-remote://"), "got: {}", err2);

        // 2. Active SSH session + missing explicit path gives actionable host-mismatch error
        let missing = tmp.join("does-not-exist-on-host");
        let err3 = resolve_mcp_workspace_with_ssh(
            Some(missing.to_str().unwrap()),
            false,
            Some(&existing_proj),
            None,
            Some(&fake_home),
            None,
            Some(("SSH_CONNECTION", "10.0.0.1 54321 10.0.0.2 22")),
        )
        .unwrap_err();
        assert!(err3.to_string().contains("SSH session detected (SSH_CONNECTION)"), "got: {}", err3);
        assert!(err3.to_string().contains("does not exist on this host"), "got: {}", err3);

        // 3. Active SSH session + $HOME cwd includes SSH note
        let err4 = resolve_mcp_workspace_with_ssh(
            None,
            false,
            Some(&fake_home),
            None,
            Some(&fake_home),
            None,
            Some(("SSH_TTY", "/dev/pts/1")),
        )
        .unwrap_err();
        assert!(err4.to_string().contains("SSH_TTY"), "got: {}", err4);

        // 4. Active SSH session + valid existing project path works normally
        let ok = resolve_mcp_workspace_with_ssh(
            Some(existing_proj.to_str().unwrap()),
            false,
            Some(&fake_home),
            None,
            Some(&fake_home),
            None,
            Some(("SSH_CLIENT", "10.0.0.1 54321 22")),
        )
        .unwrap();
        assert_eq!(PathBuf::from(ok), existing_proj);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
