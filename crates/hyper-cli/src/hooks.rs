use anyhow::Result;
use hyper_core::config::HyperConfig;
use hyper_core::git::quick_head;
use hyper_core::runner::discover_presets;
use hyper_core::transcript::{inspect_transcript_tail, resolve_transcript_path, HealthLevel};
use hyper_daemon::client::ensure_and_send;
use hyper_daemon::protocol::DaemonRequest;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

fn read_stdin_json() -> Value {
    let mut raw = String::new();
    if io::stdin().read_to_string(&mut raw).is_err() || raw.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str(&raw).unwrap_or_else(|_| json!({}))
}

/// Sub-5ms cache-friendly `SessionStart` hook:
/// - Zero cold `git` subprocess forks (uses pure-Rust `gix::quick_head`)
/// - Discovers workspace check presets
/// - Warm-attaches workspace watcher in Daemon
/// - Surfaces a sleek 1-line Hyper Deck sidecar pill for users
pub fn run_session_start() -> Result<()> {
    let payload = read_stdin_json();
    let cfg = HyperConfig::load();

    let ws_str = payload
        .get("workspacePaths")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(Value::as_str)
        .or_else(|| payload.get("cwd").and_then(Value::as_str))
        .unwrap_or("");

    let mut summary_parts = vec![format!(
        "[AGY Hyper v{} · profile={}]",
        hyper_core::version(),
        cfg.profile.as_str()
    )];

    if !ws_str.is_empty() {
        let ws = Path::new(ws_str);
        if ws.is_dir() {
            let _ = ensure_and_send(&DaemonRequest::WatchWorkspace {
                workspace: ws_str.to_string(),
            });
            if let Some((branch, oid)) = quick_head(ws) {
                summary_parts.push(format!("git={}@{}", branch, oid));
            }
            let presets: Vec<String> = discover_presets(ws)
                .into_iter()
                .take(6)
                .map(|p| p.id)
                .collect();
            if !presets.is_empty() {
                summary_parts.push(format!("presets=[{}]", presets.join(",")));
            }
        }
    }

    summary_parts.push(
        "Default to hyper_pack (AST+ripgrep+git), hyper_search, hyper_edit (default editor for workspace files: WAL+ETag+Tree-sitter), hyper_check (preset), hyper_tx (rollback/drill). Keep inline responses <=8.5KB."
            .into(),
    );

    let sys_text = summary_parts.join(" ");
    let step = if cfg.enable_ui_pill {
        json!({
            "systemMessage": {
                "systemMessage": sys_text,
                "metadata": {
                    "user_markdown": format!(
                        "⚡ **AGY Hyper** (`{}`) ready · [Open Hyper Deck](sidecar://agy-hyper/hyper_deck/)",
                        cfg.profile.as_str()
                    )
                }
            }
        })
    } else {
        json!({
            "systemMessage": {
                "systemMessage": sys_text
            }
        })
    };

    println!("{}", serde_json::to_string(&json!({ "injectSteps": [step] }))?);
    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct GuardState {
    last_warned_step: i64,
    total_tokens: u64,
    #[serde(default)]
    last_health: Option<HealthLevel>,
}

/// Sub-2ms native Rust `PreInvocation` context, cache-cliff & timeout guard.
/// Zero interpreter startup overhead + 60s Prompt Cache Warmth & model-aware guardlines.
pub fn run_pre_invocation() -> Result<()> {
    let payload = read_stdin_json();
    let cfg = HyperConfig::load();

    let conv_id = payload
        .get("conversationId")
        .and_then(Value::as_str)
        .unwrap_or("");
    let explicit_tpath = payload.get("transcriptPath").and_then(Value::as_str);

    let Some(tpath) = resolve_transcript_path(explicit_tpath, Some(conv_id)) else {
        println!("{{}}");
        return Ok(());
    };

    let Some(pulse) = inspect_transcript_tail(&tpath, &cfg) else {
        println!("{{}}");
        return Ok(());
    };

    if pulse.health == HealthLevel::Green {
        println!("{{}}");
        return Ok(());
    }

    // Throttle warnings to at most once every 10 steps unless escalating to Red or a stream timeout fired
    let state_dir = HyperConfig::data_dir().join("guard_state");
    let safe_id = hyper_core::config::sanitize_id(conv_id).unwrap_or("default");
    let state_file: PathBuf = state_dir.join(format!("{}.json", safe_id));
    let prev: GuardState = fs::read_to_string(&state_file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let step_i64 = pulse.max_step as i64;
    let escalated_to_red =
        pulse.health == HealthLevel::Red && prev.last_health != Some(HealthLevel::Red);
    if (step_i64 - prev.last_warned_step) < 10 && pulse.recent_timeouts == 0 && !escalated_to_red {
        println!("{{}}");
        return Ok(());
    }

    let _ = fs::create_dir_all(&state_dir);
    let _ = fs::write(
        &state_file,
        serde_json::to_vec(&GuardState {
            last_warned_step: step_i64,
            total_tokens: pulse.total_tokens,
            last_health: Some(pulse.health),
        })
        .unwrap_or_default(),
    );
    prune_guard_state(&state_dir, 50);

    let total_k = (pulse.total_tokens as f64 / 1000.0 * 10.0).round() / 10.0;
    let effective_red = if pulse.effective_red_tokens > 0 {
        pulse.effective_red_tokens
    } else {
        cfg.watermark_red_tokens
    };
    let red_k = (effective_red / 1000).max(1);
    let cache_pct = (pulse.cache_hit_ratio * 100.0).round() as u32;
    let model_tag = pulse
        .model
        .as_deref()
        .map(|m| format!(" · {}", m))
        .unwrap_or_default();

    if pulse.health == HealthLevel::Red {
        let mut tags = Vec::new();
        if pulse.recent_timeouts > 0 {
            tags.push(format!("{} stream timeout(s)", pulse.recent_timeouts));
        }
        if pulse.recent_resets > 0 {
            tags.push(format!("{} proxy reset(s)", pulse.recent_resets));
        }
        if pulse.multimodal_cliff_risk {
            tags.push("multimodal >180k cliff".into());
        }
        let extra = if tags.is_empty() {
            String::new()
        } else {
            format!(" · {}", tags.join(", "))
        };

        let user_md = format!(
            "> ⚡ **[AGY Hyper · Context Redline]** **`{}k / {}k` tokens · step `{}`** (cache `{}%`{}{}). Offload heavy reads to subagents and run **`/compact`** after this turn · [Open Hyper Deck](sidecar://agy-hyper/hyper_deck/)",
            total_k, red_k, pulse.max_step, cache_pct, model_tag, extra
        );
        let sys_msg = format!(
            "[AGY Hyper Redline Guard] Reason={} · context={}k/{}k tokens (step {}, cache {}%{}{}). Strictly: (1) use hyper_pack/hyper_search or delegate multi-file scans to research subagent; (2) keep outputs concise; (3) persist key progress and remind the user to run `/compact` at the end of this turn.",
            pulse.health_reason, total_k, red_k, pulse.max_step, cache_pct, model_tag, extra
        );

        println!(
            "{}",
            serde_json::to_string(&json!({
                "injectSteps": [{
                    "systemMessage": {
                        "systemMessage": sys_msg,
                        "metadata": { "user_markdown": user_md }
                    }
                }]
            }))?
        );
        return Ok(());
    }

    // Yellow: silent model-only hint with exact trigger reason (zero false context-size wording)
    let mut hints = Vec::new();
    if pulse.health_reason == "proxy_reset" {
        hints.push(format!("{} recent TCP proxy reset(s) detected — keep tool outputs compact", pulse.recent_resets));
    }
    if pulse.health_reason == "stream_timeout" {
        hints.push(format!("{} recent stream timeout(s) detected", pulse.recent_timeouts));
    }
    if pulse.multimodal_cliff_risk {
        hints.push("multimodal image context >180k detected (avoid attaching redundant images)".into());
    }
    if pulse.cache_is_cold {
        hints.push("prompt cache >60s cold (batch next actions into 1 turn to re-warm)".into());
    }
    let hint_suffix = if hints.is_empty() {
        String::new()
    } else {
        format!(" Notes: {}.", hints.join("; "))
    };
    let sys_msg = if pulse.health_reason == "proxy_reset" || pulse.health_reason == "stream_timeout" {
        format!(
            "[AGY Hyper Guard] Transient transport warning ({}) at {}k/{}k tokens (step {}, cache {}%{}).{}",
            pulse.health_reason, total_k, red_k, pulse.max_step, cache_pct, model_tag, hint_suffix
        )
    } else {
        format!(
            "[AGY Hyper Guard] Context at {}k/{}k tokens (step {}, cache {}%{}). Prefer hyper_pack/hyper_edit batching and research subagents for broad reads to stay under {}k.{}",
            total_k, red_k, pulse.max_step, cache_pct, model_tag, red_k, hint_suffix
        )
    };

    println!(
        "{}",
        serde_json::to_string(&json!({
            "injectSteps": [{
                "systemMessage": {
                    "systemMessage": sys_msg
                }
            }]
        }))?
    );
    Ok(())
}

/// Sub-2ms `PostToolUse` hook:
/// 1. Captures mathematically provable pre-images from built-in `edit_file` / `write_file`
///    into the crash-safe BLAKE3 WAL ledger (zero `PreToolUse` permission-gate interference).
/// 2. Triggers instant workspace revision bump in the Rust daemon on built-in & MCP writes/edits.
pub fn run_post_tool_use() -> Result<()> {
    let payload = read_stdin_json();
    let tool_call = payload.get("toolCall");
    let tool_name = tool_call
        .and_then(|t| t.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let tool_args = tool_call.and_then(|t| t.get("arguments").or_else(|| t.get("args")));

    let is_mcp_edit = tool_name == "call_mcp_tool"
        && tool_args
            .and_then(|a| a.get("ToolName").or_else(|| a.get("toolName")))
            .and_then(Value::as_str)
            .map(hyper_core::json_util::unquote_json_string)
            .map(|n| n == "hyper_edit" || n == "hyper_tx")
            .unwrap_or(false);

    let has_error = payload
        .get("error")
        .is_some_and(|v| match v {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::String(s) => !s.trim().is_empty(),
            _ => true,
        })
        || payload.get("isError").and_then(Value::as_bool).unwrap_or(false)
        || payload
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|s| s.eq_ignore_ascii_case("error") || s.eq_ignore_ascii_case("failed"))
        || payload
            .get("toolResponse")
            .and_then(|r| r.get("isError"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

    let mut workspace_roots: Vec<PathBuf> = payload
        .get("workspacePaths")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default();
    if let Some(cwd) = payload.get("cwd").and_then(Value::as_str) {
        if !cwd.trim().is_empty() {
            let p = PathBuf::from(cwd);
            if !workspace_roots.contains(&p) {
                workspace_roots.push(p);
            }
        }
    }

    // Auto-record verified native `edit_file` / `write_file` snapshots into WAL on success.
    if !has_error && matches!(tool_name, "edit_file" | "write_file") {
        if let Some(args) = tool_args {
            let cfg = HyperConfig::load();
            let t0 = std::time::Instant::now();
            if let Ok(Some(manifest)) = hyper_core::tx_edit::record_native_post_edit(
                &HyperConfig::wal_dir(),
                cfg.max_wal_entries,
                &workspace_roots,
                tool_name,
                args,
                cfg.block_secret_files,
            ) {
                let rel = manifest
                    .files
                    .first()
                    .map(|f| f.rel_path.clone())
                    .unwrap_or_else(|| manifest.tx_id.clone());
                let _ = hyper_daemon::client::send_request(&DaemonRequest::TelemetryEvent {
                    kind: "wal_capture".into(),
                    conversation_id: payload
                        .get("conversationId")
                        .and_then(Value::as_str)
                        .map(String::from),
                    workspace: Some(manifest.workspace_root.clone()),
                    duration_ms: t0.elapsed().as_millis() as u64,
                    saved_ms: 0,
                    turns_saved: 0,
                    detail: format!("{} ({})", rel, tool_name),
                    blocked: false,
                });
            }
        }
    }

    if matches!(
        tool_name,
        "edit_file" | "write_file" | "run_command" | "mcp_agy-hyper_hyper_edit"
    ) || is_mcp_edit
    {
        if let Some(ws) = workspace_roots.first() {
            let _ = hyper_daemon::client::send_request(&DaemonRequest::NotifyChange {
                workspace: ws.display().to_string(),
            });
        }
    }
    println!("{{}}");
    Ok(())
}

fn prune_guard_state(dir: &Path, max_files: usize) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, p))
        })
        .collect();
    if files.len() <= max_files {
        return;
    }
    files.sort_by_key(|(t, _)| *t);
    let excess = files.len() - max_files;
    for (_, old) in files.into_iter().take(excess) {
        let _ = fs::remove_file(old);
    }
}
