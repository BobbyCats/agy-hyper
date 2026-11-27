use anyhow::Result;
use hyper_core::config::HyperConfig;
use hyper_core::git::quick_head;
use hyper_core::runner::discover_presets;
use hyper_core::sessions::{discover_active_child_sessions, lookup_session_role};
use hyper_core::transcript::{
    inspect_transcript_tail, resolve_transcript_path, HealthLevel, TranscriptPulse,
};
use hyper_core::update::{
    load_update_state, now_epoch_ms_u64, spawn_detached_update_check, UpdateState,
    SESSION_START_UPDATE_TTL_MS,
};
use hyper_daemon::client::ensure_and_send;
use hyper_daemon::protocol::DaemonRequest;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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

pub(crate) fn is_home_or_root_workspace(ws: &Path, home_dir: Option<&Path>) -> bool {
    let canon = fs::canonicalize(ws).unwrap_or_else(|_| ws.to_path_buf());
    if canon.parent().is_none() {
        return true;
    }
    if let Some(h) = home_dir {
        let home_canon = fs::canonicalize(h).unwrap_or_else(|_| h.to_path_buf());
        if canon == home_canon {
            return true;
        }
    }
    false
}

fn is_code_workspace_with_home(ws: &Path, home_dir: Option<&Path>) -> bool {
    if !ws.is_dir() || is_home_or_root_workspace(ws, home_dir) {
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

fn is_code_workspace(ws: &Path) -> bool {
    is_code_workspace_with_home(ws, dirs::home_dir().as_deref())
}

#[cfg_attr(not(test), allow(dead_code))]
fn build_session_start_payload(ws_str: &str, cfg: &HyperConfig, attach_watcher: bool) -> Value {
    let home = dirs::home_dir();
    build_session_start_payload_with_context(ws_str, cfg, attach_watcher, home.as_deref(), None)
}

fn build_session_start_payload_with_context(
    ws_str: &str,
    cfg: &HyperConfig,
    attach_watcher: bool,
    home_dir: Option<&Path>,
    update_state: Option<&UpdateState>,
) -> Value {
    let mut summary_parts = vec![format!(
        "[AGY Hyper v{} · profile={}]",
        hyper_core::version(),
        cfg.profile.as_str()
    )];

    let mut code_ws = false;
    let mut is_broad_ws = false;
    if !ws_str.is_empty() {
        let ws = Path::new(ws_str);
        if ws.is_dir() {
            is_broad_ws = is_home_or_root_workspace(ws, home_dir);
            if !is_broad_ws {
                if attach_watcher {
                    let _ = ensure_and_send(&DaemonRequest::WatchWorkspace {
                        workspace: ws_str.to_string(),
                    });
                }
                code_ws = is_code_workspace_with_home(ws, home_dir);
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
    }

    if is_broad_ws {
        summary_parts.push(
            "Workspace is $HOME/root: open a specific project directory to enable AGY Hyper code acceleration."
                .into(),
        );
        summary_parts.push("Telemetry and context guard active.".into());
    } else if code_ws {
        if !cfg.any_mcp_accel_enabled() {
            summary_parts.push(
                "Zero-intrusion cockpit mode: native tools & inline diff untouched; live telemetry, cache radar, and passive WAL capture active."
                    .into(),
            );
        } else {
            let mut active = Vec::new();
            if cfg.enable_mcp_read {
                active.push("hyper_pack (AST+ripgrep+git)");
                active.push("hyper_search");
            }
            if cfg.enable_mcp_edit {
                active.push("hyper_edit (atomic WAL+ETag+Tree-sitter)");
            }
            if cfg.enable_mcp_check {
                active.push("hyper_check (sync <=60s to preserve prompt cache)");
            }
            let tail = if cfg.enable_mcp_edit {
                ""
            } else {
                " (native edit_file preserved for inline diff)"
            };
            summary_parts.push(format!(
                "Use {} for workspace code tasks{}.",
                active.join(", "),
                tail
            ));
        }
    } else {
        summary_parts.push("Telemetry and context guard active.".into());
    }

    if cfg.enable_ui_pill {
        summary_parts.push("Cockpit: [Open Hyper Deck](sidecar://agy-hyper/hyper_deck/).".into());
    }

    let current_ver = hyper_core::version();
    if cfg.enable_update_check {
        if let Some(st) = update_state.filter(|s| s.is_actionable(current_ver)) {
            if let Some(latest) = st.latest_version.as_deref() {
                summary_parts.push(format!(
                    "Update v{} available (run 'agy-hyper update' or update in Hyper Deck).",
                    latest.trim_start_matches(['v', 'V'])
                ));
            }
        }
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

fn spawn_background_update_check_detached() {
    if let Ok(exe) = std::env::current_exe() {
        spawn_detached_update_check(&exe);
    }
}

/// Prefix-stable `SessionStart` hook:
/// - Zero cold `git` subprocess forks (uses pure-Rust `gix::quick_head`)
/// - Zero synchronous network calls (`update_state.json` read in `<0.1ms`; stale checks spawn detached)
/// - Discovers workspace check presets and attaches the daemon watcher
/// - Emits a schema-safe `systemMessage` step without volatile metadata fields
pub fn run_session_start() -> Result<()> {
    let payload = read_stdin_json();
    let cfg = HyperConfig::load();
    let ws_str = extract_workspace_str(&payload);
    let current_ver = hyper_core::version();
    let update_state = load_update_state(current_ver);
    let home = dirs::home_dir();

    let out = build_session_start_payload_with_context(
        ws_str,
        &cfg,
        true,
        home.as_deref(),
        Some(&update_state),
    );
    println!("{}", serde_json::to_string(&out)?);

    if cfg.enable_update_check && std::env::var_os("AGY_HYPER_NO_UPDATE_CHECK").is_none() {
        let now_ms = now_epoch_ms_u64();
        if update_state.checked_at_ms == 0
            || now_ms.saturating_sub(update_state.checked_at_ms) > SESSION_START_UPDATE_TTL_MS
        {
            spawn_background_update_check_detached();
        }
    }
    Ok(())
}

const CHILD_STALLED_IDLE_SECS: u64 = 90;
const CHILD_ERROR_LOOP_ALERT_STREAK: u32 = 3;

#[derive(Debug, Default, Serialize, Deserialize)]
struct GuardState {
    last_warned_step: i64,
    #[serde(default)]
    last_loop_warned_step: i64,
    total_tokens: u64,
    #[serde(default)]
    last_health: Option<HealthLevel>,
    #[serde(default)]
    last_child_alert_step: i64,
    #[serde(default)]
    last_child_alert_sig: String,
    #[serde(default)]
    last_collision_sig: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChildSubagentAlert {
    conversation_id: String,
    agent_name: String,
    title: String,
    consecutive_error_streak: u32,
    last_error_summary: Option<String>,
    idle_secs: u64,
    stalled: bool,
}

fn is_non_code_subagent(agent_name: &str) -> bool {
    let lower = agent_name.trim().to_ascii_lowercase();
    lower == "browser" || lower == "image-generator"
}

fn collect_active_child_alerts(parent_conv_id: &str, cfg: &HyperConfig) -> Vec<ChildSubagentAlert> {
    let children = discover_active_child_sessions(parent_conv_id);
    if children.is_empty() {
        return Vec::new();
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let mut alerts = Vec::new();
    for child in children {
        let idle_secs = now_ms.saturating_sub(child.mtime_ms) / 1000;
        let pulse = inspect_transcript_tail(&child.transcript_path, cfg);
        let streak = pulse
            .as_ref()
            .map(|p| p.consecutive_error_streak)
            .unwrap_or(0);
        let last_err = pulse.and_then(|p| p.repeat_error_signature);
        let is_loop = streak >= CHILD_ERROR_LOOP_ALERT_STREAK;
        let is_stalled = !is_loop && idle_secs >= CHILD_STALLED_IDLE_SECS;
        if is_loop || is_stalled {
            alerts.push(ChildSubagentAlert {
                conversation_id: child.conversation_id,
                agent_name: child.agent_name,
                title: child.title,
                consecutive_error_streak: streak,
                last_error_summary: last_err,
                idle_secs,
                stalled: is_stalled,
            });
        }
    }
    alerts
}

fn child_alerts_signature(alerts: &[ChildSubagentAlert]) -> String {
    alerts
        .iter()
        .take(3)
        .map(|a| {
            let short_id = &a.conversation_id[..a.conversation_id.len().min(8)];
            if a.consecutive_error_streak >= CHILD_ERROR_LOOP_ALERT_STREAK {
                format!("{}:loop{}", short_id, a.consecutive_error_streak)
            } else {
                format!("{}:stalled", short_id)
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn format_child_watchdog_message(alerts: &[ChildSubagentAlert]) -> Option<String> {
    if alerts.is_empty() {
        return None;
    }
    let parts: Vec<String> = alerts
        .iter()
        .take(3)
        .map(|a| {
            let role = if a.agent_name.trim().is_empty() {
                "subagent"
            } else {
                a.agent_name.trim()
            };
            let short_id = &a.conversation_id[..a.conversation_id.len().min(8)];
            let label = if a.title.trim().is_empty() || a.title.contains(short_id) {
                format!("{} #{}", role, short_id)
            } else {
                format!("{} #{} (\"{}\")", role, short_id, a.title.trim())
            };
            if a.consecutive_error_streak >= CHILD_ERROR_LOOP_ALERT_STREAK {
                let err_detail = a
                    .last_error_summary
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| format!(": {}", s))
                    .unwrap_or_default();
                format!(
                    "{} [{} consecutive errors{}]",
                    label, a.consecutive_error_streak, err_detail
                )
            } else {
                format!("{} [silent {}s]", label, a.idle_secs)
            }
        })
        .collect();
    Some(format!(
        "[AGY Hyper Subagent Watchdog] Active child subagent(s) appear unhealthy: {}. Inspect state via manage_subagents or the child transcript; if stuck in a retry loop, consider killing and replacing with a narrower fresh subagent. Surface [Open Hyper Deck](sidecar://agy-hyper/hyper_deck/) if the user wants live subagent status or one-click rescue prompts.",
        parts.join("; ")
    ))
}

fn build_loop_guard_message(pulse: &TranscriptPulse, is_subagent: bool, agent_name: &str) -> String {
    let sig_note = pulse
        .repeat_error_signature
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!(" (`{}`)", s))
        .unwrap_or_default();
    if is_subagent && is_non_code_subagent(agent_name) {
        format!(
            "[AGY Hyper Loop Guard · Subagent] Detected {} consecutive error steps{}. Stop repeating the failing action blindly: capture the blocker concisely and report back via send_message so the parent agent can split or retry cleanly.",
            pulse.consecutive_error_streak, sig_note
        )
    } else if is_subagent {
        format!(
            "[AGY Hyper Loop Guard · Subagent] Detected {} consecutive error steps{}. Stop repeating the same failing step blindly: inspect the concrete error once, pivot strategy, or report the blocker back via send_message.",
            pulse.consecutive_error_streak, sig_note
        )
    } else {
        format!(
            "[AGY Hyper Loop Guard] Detected {} consecutive error steps{}. Stop repeating the same failing command or edit blindly: inspect the concrete error output, verify assumptions or file state once, and pivot strategy.",
            pulse.consecutive_error_streak, sig_note
        )
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn build_loop_guard_payload(pulse: &TranscriptPulse) -> Value {
    json!({
        "injectSteps": [{
            "systemMessage": {
                "systemMessage": build_loop_guard_message(pulse, false, "")
            }
        }]
    })
}

fn build_guard_message(
    pulse: &TranscriptPulse,
    cfg: &HyperConfig,
    is_subagent: bool,
    agent_name: &str,
    is_code_ws: bool,
) -> String {
    if pulse.health_reason == "error_loop" && pulse.total_tokens < pulse.effective_yellow_tokens {
        return build_loop_guard_message(pulse, is_subagent, agent_name);
    }

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
        if pulse.consecutive_error_streak >= 2 {
            let sig = pulse
                .repeat_error_signature
                .as_deref()
                .map(|s| format!(" ({})", s))
                .unwrap_or_default();
            tags.push(format!(
                "{} consecutive errors{}",
                pulse.consecutive_error_streak, sig
            ));
        }
        if pulse.recent_timeouts > 0 {
            tags.push(format!("{} stream timeout(s)", pulse.recent_timeouts));
        }
        if pulse.recent_resets > 0 {
            tags.push(format!("{} transport reset(s)", pulse.recent_resets));
        }
        if pulse.multimodal_cliff_risk {
            tags.push("multimodal >180k cliff".into());
        }
        let extra = if tags.is_empty() {
            String::new()
        } else {
            format!(" · {}", tags.join(", "))
        };

        return if is_subagent && is_non_code_subagent(agent_name) {
            format!(
                "[AGY Hyper Redline Guard · Subagent] Reason={} · context={}k/{}k tokens (step {}, cache {}%{}{}). Strictly: (1) avoid redundant full-page captures or retry loops; (2) finish the current atomic step cleanly; (3) return a compact result via send_message to the parent agent immediately so remaining work can be split.",
                pulse.health_reason, total_k, red_k, pulse.max_step, cache_pct, model_tag, extra
            )
        } else if is_subagent {
            let verify_tip = if cfg.enable_mcp_check {
                "finish active edits and run concise sync verification via hyper_check if needed"
            } else {
                "finish active edits and run concise verification if needed"
            };
            format!(
                "[AGY Hyper Redline Guard · Subagent] Reason={} · context={}k/{}k tokens (step {}, cache {}%{}{}). Strictly: (1) stop broad file scans; (2) {}; (3) return a compact result via send_message to the parent agent immediately.",
                pulse.health_reason, total_k, red_k, pulse.max_step, cache_pct, model_tag, extra, verify_tip
            )
        } else if is_code_ws && cfg.any_mcp_accel_enabled() {
            let read_tip = if cfg.enable_mcp_read {
                "use hyper_pack/hyper_search or delegate multi-file scans to research subagent"
            } else {
                "use targeted ranged reads or delegate multi-file scans to research subagent"
            };
            let check_tip = if cfg.enable_mcp_check {
                "keep outputs concise and use sync hyper_check (<=60s) to avoid background wakeup cache resets"
            } else {
                "keep outputs concise and avoid long background waits that cool prompt cache"
            };
            format!(
                "[AGY Hyper Redline Guard] Reason={} · context={}k/{}k tokens (step {}, cache {}%{}{}). Strictly: (1) {}; (2) {}; (3) persist key progress and remind the user to run `/compact` at the end of this turn.",
                pulse.health_reason, total_k, red_k, pulse.max_step, cache_pct, model_tag, extra, read_tip, check_tip
            )
        } else {
            format!(
                "[AGY Hyper Redline Guard] Reason={} · context={}k/{}k tokens (step {}, cache {}%{}{}). Strictly: (1) avoid redundant full-file reads and keep outputs concise; (2) persist key findings and remind the user to run `/compact` at the end of this turn.",
                pulse.health_reason, total_k, red_k, pulse.max_step, cache_pct, model_tag, extra
            )
        };
    }

    let mut hints = Vec::new();
    if pulse.health_reason == "proxy_reset" {
        hints.push(format!(
            "{} recent transport reset(s) detected — keep tool outputs compact",
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
    if is_subagent && pulse.checkpoint_count >= 2 {
        hints.push(format!(
            "{} auto-checkpoints in this subagent — prefer wrapping up via send_message rather than stretching one subagent across long loops",
            pulse.checkpoint_count
        ));
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

    if pulse.health_reason == "proxy_reset" || pulse.health_reason == "stream_timeout" {
        format!(
            "[AGY Hyper Guard] Transient transport warning ({}) at {}k/{}k tokens (step {}, cache {}%{}).{}",
            pulse.health_reason, total_k, red_k, pulse.max_step, cache_pct, model_tag, hint_suffix
        )
    } else if is_subagent && is_non_code_subagent(agent_name) {
        format!(
            "[AGY Hyper Guard · Subagent] Context at {}k/{}k tokens (step {}, cache {}%{}). Keep actions batched, avoid redundant screenshots, and prepare to report back via send_message before {}k.{}",
            total_k, red_k, pulse.max_step, cache_pct, model_tag, red_k, hint_suffix
        )
    } else if is_subagent {
        format!(
            "[AGY Hyper Guard · Subagent] Context at {}k/{}k tokens (step {}, cache {}%{}). Keep reads targeted, batch edits, and prepare to report back via send_message before {}k.{}",
            total_k, red_k, pulse.max_step, cache_pct, model_tag, red_k, hint_suffix
        )
    } else if is_code_ws && cfg.any_mcp_accel_enabled() {
        let mut tips = Vec::new();
        if cfg.enable_mcp_read && cfg.enable_mcp_edit {
            tips.push("hyper_pack/hyper_edit batching");
        } else if cfg.enable_mcp_read {
            tips.push("hyper_pack targeted reads");
        } else if cfg.enable_mcp_edit {
            tips.push("hyper_edit batching");
        }
        if cfg.enable_mcp_check {
            tips.push("sync hyper_check (<=60s)");
        }
        tips.push("research subagents for broad reads");
        format!(
            "[AGY Hyper Guard] Context at {}k/{}k tokens (step {}, cache {}%{}). Prefer {} to stay under {}k.{}",
            total_k, red_k, pulse.max_step, cache_pct, model_tag, tips.join(", "), red_k, hint_suffix
        )
    } else {
        format!(
            "[AGY Hyper Guard] Context at {}k/{}k tokens (step {}, cache {}%{}). Keep tool outputs concise and batch reads to stay under {}k.{}",
            total_k, red_k, pulse.max_step, cache_pct, model_tag, red_k, hint_suffix
        )
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn build_guard_payload(
    pulse: &TranscriptPulse,
    cfg: &HyperConfig,
    is_subagent: bool,
    is_code_ws: bool,
) -> Value {
    json!({
        "injectSteps": [{
            "systemMessage": {
                "systemMessage": build_guard_message(pulse, cfg, is_subagent, "", is_code_ws)
            }
        }]
    })
}

/// Sub-2ms native Rust `PreInvocation` context, cache-cliff, timeout & subagent watchdog guard.
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

    let (is_subagent, agent_name) = lookup_session_role(conv_id);
    let child_alerts = if !is_subagent && !conv_id.is_empty() {
        collect_active_child_alerts(conv_id, &cfg)
    } else {
        Vec::new()
    };

    let ws_raw = extract_workspace_str(&payload);
    let collision_alert = if !is_subagent && !ws_raw.is_empty() {
        let canon_ws = fs::canonicalize(ws_raw)
            .unwrap_or_else(|_| PathBuf::from(ws_raw))
            .display()
            .to_string();
        let recent_txs = hyper_core::wal::list_transactions(&HyperConfig::wal_dir(), 25);
        hyper_core::wal::detect_write_collisions(&recent_txs, now_epoch_ms_u64(), 300_000)
            .into_iter()
            .find(|c| c.workspace_root == canon_ws && c.kind == "same_file_race")
    } else {
        None
    };

    let has_loop_streak = pulse.consecutive_error_streak >= 2;
    let self_alert_eligible = pulse.health != HealthLevel::Green || has_loop_streak;
    if !self_alert_eligible && child_alerts.is_empty() && collision_alert.is_none() {
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
    let mut next_state = GuardState {
        last_warned_step: prev.last_warned_step,
        last_loop_warned_step: prev.last_loop_warned_step,
        total_tokens: pulse.total_tokens,
        last_health: prev.last_health,
        last_child_alert_step: prev.last_child_alert_step,
        last_child_alert_sig: prev.last_child_alert_sig.clone(),
        last_collision_sig: prev.last_collision_sig.clone(),
    };

    let mut messages: Vec<String> = Vec::new();

    if self_alert_eligible {
        let is_pure_loop =
            pulse.health_reason == "error_loop" && pulse.total_tokens < pulse.effective_yellow_tokens;
        if is_pure_loop || (pulse.health == HealthLevel::Green && has_loop_streak) {
            if (step_i64 - prev.last_loop_warned_step) >= 3
                || (pulse.health == HealthLevel::Red && prev.last_health != Some(HealthLevel::Red))
            {
                next_state.last_loop_warned_step = step_i64;
                next_state.last_health = Some(pulse.health);
                messages.push(build_loop_guard_message(&pulse, is_subagent, &agent_name));
            }
        } else {
            let escalated_to_red =
                pulse.health == HealthLevel::Red && prev.last_health != Some(HealthLevel::Red);
            let min_step_gap = if pulse.recent_timeouts > 0 || has_loop_streak {
                4
            } else {
                10
            };
            if (step_i64 - prev.last_warned_step) >= min_step_gap || escalated_to_red {
                next_state.last_warned_step = step_i64;
                if has_loop_streak {
                    next_state.last_loop_warned_step = step_i64;
                }
                next_state.last_health = Some(pulse.health);
                let ws_str = extract_workspace_str(&payload);
                let code_ws = if ws_str.is_empty() {
                    false
                } else {
                    is_code_workspace(Path::new(ws_str))
                };
                messages.push(build_guard_message(
                    &pulse,
                    &cfg,
                    is_subagent,
                    &agent_name,
                    code_ws,
                ));
            }
        }
    }

    if !child_alerts.is_empty() {
        let sig = child_alerts_signature(&child_alerts);
        if sig != prev.last_child_alert_sig || (step_i64 - prev.last_child_alert_step) >= 4 {
            if let Some(msg) = format_child_watchdog_message(&child_alerts) {
                next_state.last_child_alert_step = step_i64;
                next_state.last_child_alert_sig = sig;
                messages.push(msg);
            }
        }
    }

    if let Some(col) = collision_alert {
        let col_sig = format!("{}:{}:{}", col.kind, col.latest_ts_ms, col.rel_paths.join(","));
        if col_sig != prev.last_collision_sig {
            next_state.last_collision_sig = col_sig;
            messages.push(format!(
                "[AGY Hyper Write-Race Guard] {} distinct writer sessions modified shared workspace file(s) [{}] within 5m. Re-read affected files before editing, pass `expected_etag` in `hyper_edit`, and prefer `workspace=\"share\"` (or `\"branch\"`) when spawning parallel `/swarm` or `run_workflow` writers. Instant WAL rollback is available in [Open Hyper Deck](sidecar://agy-hyper/hyper_deck/).",
                col.writer_count,
                col.rel_paths.join(", ")
            ));
        }
    }

    if messages.is_empty() {
        println!("{{}}");
        return Ok(());
    }

    let _ = fs::create_dir_all(&state_dir);
    let _ = fs::write(&state_file, serde_json::to_vec(&next_state).unwrap_or_default());
    prune_guard_state(&state_dir, 50);

    let inject_steps: Vec<Value> = messages
        .into_iter()
        .map(|m| json!({ "systemMessage": { "systemMessage": m } }))
        .collect();
    let out = json!({ "injectSteps": inject_steps });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

fn should_notify_post_tool_use(tool_name: &str, has_error: bool) -> bool {
    !has_error && matches!(tool_name, "edit_file" | "write_file")
}

/// Sub-2ms `PostToolUse` hook:
/// 1. Captures mathematically provable pre-images from built-in `edit_file` / `write_file`
///    into the crash-safe BLAKE3 WAL ledger (zero `PreToolUse` permission-gate interference).
/// 2. Triggers instant workspace revision bump in the Rust daemon strictly on verified native
///    `edit_file` / `write_file` (`hyper_edit` already notifies in `mcp.rs`, and `run_command`
///    never thrashes `workspace_rev` / `prewarm_tier2`).
pub fn run_post_tool_use() -> Result<()> {
    let payload = read_stdin_json();
    let tool_call = payload.get("toolCall");
    let tool_name = tool_call
        .and_then(|t| t.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let tool_args = tool_call.and_then(|t| t.get("arguments").or_else(|| t.get("args")));

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

    if !should_notify_post_tool_use(tool_name, has_error) {
        println!("{{}}");
        return Ok(());
    }

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
    let mut notify_ws: Option<String> = None;
    if let Some(args) = tool_args {
        let cfg = HyperConfig::load();
        let t0 = std::time::Instant::now();
        let writer_conv = payload.get("conversationId").and_then(Value::as_str);
        if let Ok(Some(manifest)) = hyper_core::tx_edit::record_native_post_edit_with_writer(
            &HyperConfig::wal_dir(),
            cfg.max_wal_entries,
            &workspace_roots,
            tool_name,
            args,
            cfg.block_secret_files,
            writer_conv,
        ) {
            notify_ws = Some(manifest.workspace_root.clone());
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
                workspace: Some(manifest.workspace_root),
                duration_ms: t0.elapsed().as_millis() as u64,
                saved_ms: 0,
                turns_saved: 0,
                detail: format!("{} ({})", rel, tool_name),
                blocked: false,
            });
        }
    }

    if let Some(ws_str) = notify_ws.or_else(|| workspace_roots.first().map(|w| w.display().to_string())) {
        let _ = hyper_daemon::client::send_request(&DaemonRequest::NotifyChange {
            workspace: ws_str,
        });
    }
    println!("{{}}");
    Ok(())
}

/// Pure, deterministic gate for `Stop` lifecycle hook (`<0.1ms`).
/// - Blocks recursion via `AGY_HYPER_NAMER_WORKER == "1"` or `"antigravity-cli"` / `"antigravity-ide"` paths.
/// - Drops non-normal terminations (`terminationReason != "NO_TOOL_CALL"`).
/// - Intentionally ALLOWS `fullyIdle == false` (e.g. parked `/swarm` or `browser` subagents) and
///   `continuationActive == true` (`/goal` loops) when `terminationReason == "NO_TOOL_CALL"`!
pub(crate) fn extract_eligible_stop_job(
    payload: &Value,
    namer_worker_env: Option<&str>,
) -> Option<(String, Option<String>, Vec<String>)> {
    if namer_worker_env.map(str::trim) == Some("1") {
        return None;
    }

    for path_key in ["transcriptPath", "artifactDirectoryPath", "appDataDir"] {
        if let Some(p) = payload.get(path_key).and_then(Value::as_str) {
            let lower = p.to_ascii_lowercase();
            if lower.contains("antigravity-cli") || lower.contains("antigravity-ide") {
                return None;
            }
        }
    }

    if payload.get("terminationReason").and_then(Value::as_str) != Some("NO_TOOL_CALL") {
        return None;
    }

    let raw_cid = payload.get("conversationId").and_then(Value::as_str)?;
    let cid = hyper_core::config::sanitize_id(raw_cid)?.to_string();

    let transcript_path = payload
        .get("transcriptPath")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);

    let mut workspaces: Vec<String> = payload
        .get("workspacePaths")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    if let Some(cwd) = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !workspaces.iter().any(|w| w == cwd) {
            workspaces.push(cwd.to_string());
        }
    }

    Some((cid, transcript_path, workspaces))
}

/// Sub-2ms `Stop` hook: validates eligibility, fire-and-forget enqueues `SessionStop` to `hyper-daemon`,
/// and unconditionally prints `{}` on stdout without blocking the agent turn loop.
pub fn run_stop() -> Result<()> {
    let payload = read_stdin_json();
    let worker_env = std::env::var("AGY_HYPER_NAMER_WORKER").ok();
    if let Some((conversation_id, transcript_path, workspace_paths)) =
        extract_eligible_stop_job(&payload, worker_env.as_deref())
    {
        let cfg = HyperConfig::load();
        if cfg.session_naming.enabled {
            let req = DaemonRequest::SessionStop {
                conversation_id,
                transcript_path,
                workspace_paths,
            };
            if hyper_daemon::client::send_request_with_timeout(&req, 25).is_err() {
                let _ = hyper_daemon::client::spawn_daemon_detached();
            }
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
            checkpoint_count: 0,
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

    #[test]
    fn observe_and_safe_profiles_never_nag_disabled_mcp_tools() {
        let ws = std::env::temp_dir().join(format!(
            "agy-hyper-observe-hook-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&ws);
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("Cargo.toml"), "[package]\n").unwrap();

        let mut observe_cfg = HyperConfig::default();
        observe_cfg.apply_profile(hyper_core::config::Profile::Observe);
        let start_obs = build_session_start_payload(&ws.display().to_string(), &observe_cfg, false);
        let msg_obs = start_obs["injectSteps"][0]["systemMessage"]["systemMessage"]
            .as_str()
            .unwrap();
        assert!(msg_obs.contains("Zero-intrusion cockpit mode"));
        assert!(!msg_obs.contains("hyper_edit"));
        assert!(!msg_obs.contains("hyper_pack"));

        let mut safe_cfg = HyperConfig::default();
        safe_cfg.apply_profile(hyper_core::config::Profile::Safe);
        let start_safe = build_session_start_payload(&ws.display().to_string(), &safe_cfg, false);
        let msg_safe = start_safe["injectSteps"][0]["systemMessage"]["systemMessage"]
            .as_str()
            .unwrap();
        assert!(msg_safe.contains("hyper_pack"));
        assert!(!msg_safe.contains("hyper_edit"));
        assert!(msg_safe.contains("native edit_file preserved"));

        let yellow = sample_pulse(HealthLevel::Yellow, "token_yellow", 195_000);
        let guard_obs = build_guard_payload(&yellow, &observe_cfg, false, true);
        let guard_obs_msg = guard_obs["injectSteps"][0]["systemMessage"]["systemMessage"]
            .as_str()
            .unwrap();
        assert!(!guard_obs_msg.contains("hyper_edit"));
        assert!(!guard_obs_msg.contains("hyper_pack"));

        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn subagent_watchdog_and_browser_role_guard_behave_cleanly() {
        let cfg = HyperConfig::default();

        // Non-code browser subagent must never be told to run `hyper_check` or stop file scans.
        let mut red_browser = sample_pulse(HealthLevel::Red, "token_red", 290_000);
        red_browser.checkpoint_count = 3;
        let msg_browser = build_guard_message(&red_browser, &cfg, true, "browser", true);
        assert!(msg_browser.contains("send_message"));
        assert!(!msg_browser.contains("hyper_check"));
        assert!(!msg_browser.contains("broad file scans"));

        // Yellow browser subagent with >=2 auto-checkpoints gets gentle split hint.
        let mut y_browser = sample_pulse(HealthLevel::Yellow, "multimodal_cliff", 185_000);
        y_browser.checkpoint_count = 2;
        y_browser.multimodal_cliff_risk = true;
        let msg_y = build_guard_message(&y_browser, &cfg, true, "browser", true);
        assert!(msg_y.contains("2 auto-checkpoints"));
        assert!(msg_y.contains("redundant screenshots"));

        // Parent watchdog formats both active crash loops and silent stalls cleanly.
        let alerts = vec![
            ChildSubagentAlert {
                conversation_id: "aaaaaaaa-1111-2222-3333-444444444444".into(),
                agent_name: "browser".into(),
                title: "UI Runner".into(),
                consecutive_error_streak: 6,
                last_error_summary: Some("API error (attempt 6): EOF".into()),
                idle_secs: 15,
                stalled: false,
            },
            ChildSubagentAlert {
                conversation_id: "bbbbbbbb-1111-2222-3333-444444444444".into(),
                agent_name: "research".into(),
                title: "Docs Scan".into(),
                consecutive_error_streak: 0,
                last_error_summary: None,
                idle_secs: 145,
                stalled: true,
            },
        ];
        let watchdog = format_child_watchdog_message(&alerts).unwrap();
        assert!(watchdog.contains("[AGY Hyper Subagent Watchdog]"));
        assert!(watchdog.contains("browser #aaaaaaaa"));
        assert!(watchdog.contains("6 consecutive errors: API error (attempt 6): EOF"));
        assert!(watchdog.contains("research #bbbbbbbb"));
        assert!(watchdog.contains("silent 145s"));
        assert_eq!(child_alerts_signature(&alerts), "aaaaaaaa:loop6,bbbbbbbb:stalled");
    }

    #[test]
    fn home_and_root_workspace_guards_reject_stray_markers_and_emit_hint() {
        let tmp = std::env::temp_dir().join(format!(
            "agy-hyper-home-guard-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        let fake_home = tmp.join("fake-home");
        fs::create_dir_all(fake_home.join("src")).unwrap();
        fs::write(fake_home.join("package.json"), "{}").unwrap();
        fs::write(fake_home.join("Cargo.toml"), "[package]\n").unwrap();

        assert!(!is_code_workspace_with_home(&fake_home, Some(&fake_home)));
        #[cfg(unix)]
        assert!(!is_code_workspace_with_home(Path::new("/"), Some(&fake_home)));

        let sub_proj = fake_home.join("projects/app");
        fs::create_dir_all(&sub_proj).unwrap();
        fs::write(sub_proj.join("Cargo.toml"), "[package]\n").unwrap();
        assert!(is_code_workspace_with_home(&sub_proj, Some(&fake_home)));

        let cfg = HyperConfig::default();
        let home_payload = build_session_start_payload_with_context(
            &fake_home.display().to_string(),
            &cfg,
            false,
            Some(&fake_home),
            None,
        );
        let msg = home_payload["injectSteps"][0]["systemMessage"]["systemMessage"]
            .as_str()
            .unwrap();
        assert!(msg.contains(
            "Workspace is $HOME/root: open a specific project directory to enable AGY Hyper code acceleration."
        ));
        assert!(!msg.contains("hyper_pack"));

        let actionable_update = UpdateState {
            checked_at_ms: 1_700_000_000_000,
            current_version: hyper_core::version().into(),
            latest_version: Some("99.0.0".into()),
            update_available: true,
            release_url: Some("https://github.com/BobbyCats/agy-hyper/releases/tag/v99.0.0".into()),
            published_at: None,
            dismissed_version: None,
            last_error: None,
        };
        let with_update = build_session_start_payload_with_context(
            &sub_proj.display().to_string(),
            &cfg,
            false,
            Some(&fake_home),
            Some(&actionable_update),
        );
        let msg_upd = with_update["injectSteps"][0]["systemMessage"]["systemMessage"]
            .as_str()
            .unwrap();
        assert!(msg_upd.contains(
            "Update v99.0.0 available (run 'agy-hyper update' or update in Hyper Deck)."
        ));

        let mut dismissed_update = actionable_update;
        dismissed_update.dismissed_version = Some("99.0.0".into());
        let with_dismissed = build_session_start_payload_with_context(
            &sub_proj.display().to_string(),
            &cfg,
            false,
            Some(&fake_home),
            Some(&dismissed_update),
        );
        let msg_dis = with_dismissed["injectSteps"][0]["systemMessage"]["systemMessage"]
            .as_str()
            .unwrap();
        assert!(!msg_dis.contains("Update v99.0.0 available"));

        assert!(should_notify_post_tool_use("edit_file", false));
        assert!(should_notify_post_tool_use("write_file", false));
        assert!(!should_notify_post_tool_use("edit_file", true));
        assert!(!should_notify_post_tool_use("run_command", false));
        assert!(!should_notify_post_tool_use("call_mcp_tool", false));
        assert!(!should_notify_post_tool_use("mcp_agy-hyper_hyper_edit", false));

        // Stop hook gate: recursion guards, fullyIdle, terminationReason, and /goal continuationActive
        let base_stop = json!({
            "hookEventName": "Stop",
            "conversationId": "11111111-2222-3333-4444-555555555555",
            "transcriptPath": "/tmp/.gemini/antigravity/brain/11111111/transcript.jsonl",
            "artifactDirectoryPath": "/tmp/.gemini/antigravity/brain/11111111",
            "fullyIdle": true,
            "continuationActive": true,
            "terminationReason": "NO_TOOL_CALL",
            "workspacePaths": ["/work/app"]
        });
        // Allows continuationActive == true when terminationReason == NO_TOOL_CALL
        assert!(extract_eligible_stop_job(&base_stop, None).is_some());
        // Blocks AGY_HYPER_NAMER_WORKER == "1"
        assert!(extract_eligible_stop_job(&base_stop, Some("1")).is_none());
        // Blocks antigravity-cli / antigravity-ide recursion paths
        let cli_stop = json!({
            "conversationId": "11111111-2222-3333-4444-555555555555",
            "transcriptPath": "/tmp/.gemini/antigravity-cli/brain/11111111/transcript.jsonl",
            "fullyIdle": true,
            "terminationReason": "NO_TOOL_CALL"
        });
        assert!(extract_eligible_stop_job(&cli_stop, None).is_none());
        let ide_stop = json!({
            "conversationId": "11111111-2222-3333-4444-555555555555",
            "artifactDirectoryPath": "/tmp/.gemini/antigravity-ide/brain/11111111",
            "fullyIdle": true,
            "terminationReason": "NO_TOOL_CALL"
        });
        assert!(extract_eligible_stop_job(&ide_stop, None).is_none());
        // Allows fullyIdle == false (e.g. parked /swarm or browser subagent) when NO_TOOL_CALL, blocks non-NO_TOOL_CALL reasons
        let busy_stop = json!({
            "conversationId": "11111111-2222-3333-4444-555555555555",
            "fullyIdle": false,
            "terminationReason": "NO_TOOL_CALL"
        });
        assert!(extract_eligible_stop_job(&busy_stop, None).is_some());
        let canceled_stop = json!({
            "conversationId": "11111111-2222-3333-4444-555555555555",
            "fullyIdle": true,
            "terminationReason": "USER_CANCELED"
        });
        assert!(extract_eligible_stop_job(&canceled_stop, None).is_none());

        let _ = fs::remove_dir_all(&tmp);
    }
}
