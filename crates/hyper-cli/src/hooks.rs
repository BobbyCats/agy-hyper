use anyhow::Result;
use hyper_core::config::HyperConfig;
use hyper_core::git::quick_head;
use hyper_core::runner::discover_presets;
use hyper_core::sessions::is_subagent_conversation;
use hyper_core::transcript::{
    inspect_transcript_tail, resolve_transcript_path, HealthLevel, TranscriptPulse,
};
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

fn extract_workspace_str(payload: &Value) -> &str {
    payload
        .get("workspacePaths")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(Value::as_str)
        .or_else(|| payload.get("cwd").and_then(Value::as_str))
        .unwrap_or("")
}

fn is_code_workspace(ws: &Path) -> bool {
    if !ws.is_dir() {
        return false;
    }
    const MARKERS: &[&str] = &[
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "Makefile",
        "CMakeLists.txt",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "requirements.txt",
        "Gemfile",
        "composer.json",
        "src",
        "crates",
        "packages",
        "apps",
    ];
    MARKERS.iter().any(|m| ws.join(m).exists())
}

fn build_session_start_payload(ws_str: &str, cfg: &HyperConfig, attach_watcher: bool) -> Value {
    let mut summary_parts = vec![format!(
        "[AGY Hyper v{} · profile={}]",
        hyper_core::version(),
        cfg.profile.as_str()
    )];

    let mut code_ws = false;
    if !ws_str.is_empty() {
        let ws = Path::new(ws_str);
        if ws.is_dir() {
            if attach_watcher {
                let _ = ensure_and_send(&DaemonRequest::WatchWorkspace {
                    workspace: ws_str.to_string(),
                });
            }
            code_ws = is_code_workspace(ws);
            if let Some((branch, _)) = quick_head(ws) {
                summary_parts.push(format!("branch={}", branch));
            }
            let presets: Vec<String> = discover_presets(ws)
                .into_iter()
                .take(8)
                .map(|p| p.id)
                .collect();
            if !presets.is_empty() {
                summary_parts.push(format!("presets=[{}]", presets.join(",")));
            }
        }
    }

    if code_ws {
        summary_parts.push(
            "Use hyper_pack (AST+ripgrep+git), hyper_search, hyper_edit (atomic WAL+ETag+Tree-sitter), and hyper_check (sync <=60s to preserve prompt cache) for workspace code tasks."
                .into(),
        );
    } else {
        summary_parts.push("Telemetry and context guard active.".into());
    }

    if cfg.enable_ui_pill {
        summary_parts.push("Cockpit: [Open Hyper Deck](sidecar://agy-hyper/hyper_deck/).".into());
    }

    let sys_text = summary_parts.join(" ");
    json!({
        "injectSteps": [{
            "systemMessage": {
                "systemMessage": sys_text
            }
        }]
    })
}

/// Prefix-stable `SessionStart` hook:
/// - Zero cold `git` subprocess forks (uses pure-Rust `gix::quick_head`)
/// - Discovers workspace check presets and attaches the daemon watcher
/// - Emits a schema-safe `systemMessage` step without volatile metadata fields
pub fn run_session_start() -> Result<()> {
    let payload = read_stdin_json();
    let cfg = HyperConfig::load();
    let ws_str = extract_workspace_str(&payload);
    let out = build_session_start_payload(ws_str, &cfg, true);
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct GuardState {
    last_warned_step: i64,
    #[serde(default)]
    last_loop_warned_step: i64,
    total_tokens: u64,
    #[serde(default)]
    last_health: Option<HealthLevel>,
}

fn build_loop_guard_payload(pulse: &TranscriptPulse) -> Value {
    let sig_note = pulse
        .repeat_error_signature
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!(" (`{}`)", s))
        .unwrap_or_default();
    let sys_msg = format!(
        "[AGY Hyper Loop Guard] Detected {} consecutive error steps{}. Stop repeating the same failing command or edit blindly: inspect the concrete error output, verify assumptions or file state once, and pivot strategy.",
        pulse.consecutive_error_streak, sig_note
    );
    json!({
        "injectSteps": [{
            "systemMessage": {
                "systemMessage": sys_msg
            }
        }]
    })
}

fn build_guard_payload(
    pulse: &TranscriptPulse,
    cfg: &HyperConfig,
    is_subagent: bool,
    is_code_ws: bool,
) -> Value {
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

        let sys_msg = if is_subagent {
            format!(
                "[AGY Hyper Redline Guard · Subagent] Reason={} · context={}k/{}k tokens (step {}, cache {}%{}{}). Strictly: (1) stop broad file scans; (2) finish active edits and run concise sync verification via hyper_check if needed; (3) return a compact result via send_message to the parent agent immediately.",
                pulse.health_reason, total_k, red_k, pulse.max_step, cache_pct, model_tag, extra
            )
        } else if is_code_ws {
            format!(
                "[AGY Hyper Redline Guard] Reason={} · context={}k/{}k tokens (step {}, cache {}%{}{}). Strictly: (1) use hyper_pack/hyper_search or delegate multi-file scans to research subagent; (2) keep outputs concise and use sync hyper_check (<=60s) to avoid background wakeup cache resets; (3) persist key progress and remind the user to run `/compact` at the end of this turn.",
                pulse.health_reason, total_k, red_k, pulse.max_step, cache_pct, model_tag, extra
            )
        } else {
            format!(
                "[AGY Hyper Redline Guard] Reason={} · context={}k/{}k tokens (step {}, cache {}%{}{}). Strictly: (1) avoid redundant full-file reads and keep outputs concise; (2) persist key findings and remind the user to run `/compact` at the end of this turn.",
                pulse.health_reason, total_k, red_k, pulse.max_step, cache_pct, model_tag, extra
            )
        };

        return json!({
            "injectSteps": [{
                "systemMessage": {
                    "systemMessage": sys_msg
                }
            }]
        });
    }

    let mut hints = Vec::new();
    if pulse.health_reason == "proxy_reset" {
        hints.push(format!(
            "{} recent TCP proxy reset(s) detected — keep tool outputs compact",
            pulse.recent_resets
        ));
    }
    if pulse.health_reason == "stream_timeout" {
        hints.push(format!(
            "{} recent stream timeout(s) detected",
            pulse.recent_timeouts
        ));
    }
    if pulse.multimodal_cliff_risk {
        hints.push("multimodal image context >180k detected (avoid attaching redundant images)".into());
    }
    if pulse.cache_is_cold {
        let ttl_secs = if pulse.effective_cache_ttl_secs >= 30 {
            pulse.effective_cache_ttl_secs
        } else {
            cfg.cache_warm_window_secs
        };
        hints.push(format!(
            "prompt cache >{}s cold (batch next actions into 1 turn to re-warm)",
            ttl_secs
        ));
    }
    if pulse.consecutive_error_streak >= 2 {
        hints.push(format!(
            "{} consecutive error steps detected — stop retrying blindly and inspect root cause",
            pulse.consecutive_error_streak
        ));
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
    } else if is_subagent {
        format!(
            "[AGY Hyper Guard · Subagent] Context at {}k/{}k tokens (step {}, cache {}%{}). Keep reads targeted, batch edits, and prepare to report back via send_message before {}k.{}",
            total_k, red_k, pulse.max_step, cache_pct, model_tag, red_k, hint_suffix
        )
    } else if is_code_ws {
        format!(
            "[AGY Hyper Guard] Context at {}k/{}k tokens (step {}, cache {}%{}). Prefer hyper_pack/hyper_edit batching, sync hyper_check (<=60s), and research subagents for broad reads to stay under {}k.{}",
            total_k, red_k, pulse.max_step, cache_pct, model_tag, red_k, hint_suffix
        )
    } else {
        format!(
            "[AGY Hyper Guard] Context at {}k/{}k tokens (step {}, cache {}%{}). Keep tool outputs concise and batch reads to stay under {}k.{}",
            total_k, red_k, pulse.max_step, cache_pct, model_tag, red_k, hint_suffix
        )
    };

    json!({
        "injectSteps": [{
            "systemMessage": {
                "systemMessage": sys_msg
            }
        }]
    })
}

/// Sub-2ms native Rust `PreInvocation` context, cache-cliff & timeout guard.
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

    let has_loop_streak = pulse.consecutive_error_streak >= 2;
    if pulse.health == HealthLevel::Green && !has_loop_streak {
        println!("{{}}");
        return Ok(());
    }

    let state_dir = HyperConfig::data_dir().join("guard_state");
    let safe_id = hyper_core::config::sanitize_id(conv_id).unwrap_or("default");
    let state_file: PathBuf = state_dir.join(format!("{}.json", safe_id));
    let prev: GuardState = fs::read_to_string(&state_file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let step_i64 = pulse.max_step as i64;

    if pulse.health == HealthLevel::Green && has_loop_streak {
        if (step_i64 - prev.last_loop_warned_step) < 3 {
            println!("{{}}");
            return Ok(());
        }
        let _ = fs::create_dir_all(&state_dir);
        let _ = fs::write(
            &state_file,
            serde_json::to_vec(&GuardState {
                last_warned_step: prev.last_warned_step,
                last_loop_warned_step: step_i64,
                total_tokens: pulse.total_tokens,
                last_health: prev.last_health,
            })
            .unwrap_or_default(),
        );
        prune_guard_state(&state_dir, 50);
        let out = build_loop_guard_payload(&pulse);
        println!("{}", serde_json::to_string(&out)?);
        return Ok(());
    }

    let escalated_to_red =
        pulse.health == HealthLevel::Red && prev.last_health != Some(HealthLevel::Red);
    let min_step_gap = if pulse.recent_timeouts > 0 { 4 } else { 10 };
    if (step_i64 - prev.last_warned_step) < min_step_gap && !escalated_to_red {
        println!("{{}}");
        return Ok(());
    }

    let _ = fs::create_dir_all(&state_dir);
    let _ = fs::write(
        &state_file,
        serde_json::to_vec(&GuardState {
            last_warned_step: step_i64,
            last_loop_warned_step: if has_loop_streak {
                step_i64
            } else {
                prev.last_loop_warned_step
            },
            total_tokens: pulse.total_tokens,
            last_health: Some(pulse.health),
        })
        .unwrap_or_default(),
    );
    prune_guard_state(&state_dir, 50);

    let ws_str = extract_workspace_str(&payload);
    let code_ws = if ws_str.is_empty() {
        false
    } else {
        is_code_workspace(Path::new(ws_str))
    };
    let is_subagent = is_subagent_conversation(conv_id);
    let out = build_guard_payload(&pulse, &cfg, is_subagent, code_ws);
    println!("{}", serde_json::to_string(&out)?);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pulse(health: HealthLevel, reason: &str, tokens: u64) -> TranscriptPulse {
        TranscriptPulse {
            conversation_id: "conv-1".into(),
            transcript_path: "/tmp/transcript.jsonl".into(),
            model: Some("gemini-4-neon-xhigh".into()),
            model_context_limit: 1_000_000,
            pricing_cliff_tokens: 200_000,
            effective_yellow_tokens: 180_000,
            effective_red_tokens: 280_000,
            max_step: 120,
            steps_since_compact: 120,
            total_tokens: tokens,
            input_tokens: 10_000,
            cache_read_tokens: tokens.saturating_sub(10_000),
            output_tokens: 500,
            cache_hit_ratio: 0.92,
            recent_timeouts: 0,
            recent_resets: 0,
            media_items_recent: 0,
            last_planner_epoch_ms: None,
            seconds_since_last_planner: Some(15),
            cache_warm_remaining_secs: 225,
            effective_cache_ttl_secs: 240,
            cache_ttl_source: "default".into(),
            cache_is_cold: false,
            multimodal_cliff_risk: false,
            recently_compacted: false,
            model_switched_from: None,
            consecutive_error_streak: 0,
            repeat_error_signature: None,
            health,
            health_reason: reason.into(),
        }
    }

    #[test]
    fn hook_payloads_omit_metadata_and_adapt_to_subagent_and_workspace() {
        let cfg = HyperConfig::default();
        let ss = build_session_start_payload("", &cfg, false);
        let ss_step = &ss["injectSteps"][0]["systemMessage"];
        assert!(ss_step.get("metadata").is_none());
        assert!(ss_step["systemMessage"].as_str().unwrap().contains("AGY Hyper"));

        let red = sample_pulse(HealthLevel::Red, "token_red", 295_000);
        let red_main = build_guard_payload(&red, &cfg, false, true);
        let red_main_step = &red_main["injectSteps"][0]["systemMessage"];
        assert!(red_main_step.get("metadata").is_none());
        assert!(red_main_step["systemMessage"].as_str().unwrap().contains("/compact"));

        let red_sub = build_guard_payload(&red, &cfg, true, true);
        let red_sub_txt = red_sub["injectSteps"][0]["systemMessage"]["systemMessage"]
            .as_str()
            .unwrap();
        assert!(red_sub_txt.contains("send_message"));
        assert!(!red_sub_txt.contains("/compact"));

        let yellow = sample_pulse(HealthLevel::Yellow, "token_yellow", 195_000);
        let non_code = build_guard_payload(&yellow, &cfg, false, false);
        let non_code_txt = non_code["injectSteps"][0]["systemMessage"]["systemMessage"]
            .as_str()
            .unwrap();
        assert!(!non_code_txt.contains("hyper_pack"));

        let notes_dir = std::env::temp_dir().join(format!(
            "agy-hyper-hooks-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&notes_dir);
        fs::create_dir_all(notes_dir.join(".git")).unwrap();
        assert!(!is_code_workspace(&notes_dir));
        fs::write(notes_dir.join("Cargo.toml"), "[package]\n").unwrap();
        assert!(is_code_workspace(&notes_dir));
        let _ = fs::remove_dir_all(&notes_dir);
    }

    #[test]
    fn loop_guard_and_empirical_ttl_surface_in_pre_invocation_payload() {
        let cfg = HyperConfig::default();
        let mut p = sample_pulse(HealthLevel::Green, "healthy", 40_000);
        p.consecutive_error_streak = 3;
        p.repeat_error_signature = Some("cargo test failed: missing import".into());
        let loop_payload = build_loop_guard_payload(&p);
        let loop_msg = loop_payload["injectSteps"][0]["systemMessage"]["systemMessage"]
            .as_str()
            .unwrap();
        assert!(loop_msg.contains("[AGY Hyper Loop Guard]"));
        assert!(loop_msg.contains("3 consecutive error steps"));
        assert!(loop_msg.contains("cargo test failed"));

        let mut yellow = sample_pulse(HealthLevel::Yellow, "token_yellow", 195_000);
        yellow.cache_is_cold = true;
        yellow.effective_cache_ttl_secs = 180;
        yellow.consecutive_error_streak = 2;
        let yellow_payload = build_guard_payload(&yellow, &cfg, false, true);
        let yellow_msg = yellow_payload["injectSteps"][0]["systemMessage"]["systemMessage"]
            .as_str()
            .unwrap();
        assert!(yellow_msg.contains(">180s cold"));
        assert!(yellow_msg.contains("2 consecutive error steps"));
    }
}
