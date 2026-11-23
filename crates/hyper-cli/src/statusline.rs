use anyhow::{anyhow, Result};
use hyper_core::config::HyperConfig;
use hyper_core::transcript::{inspect_transcript_tail, resolve_transcript_path, HealthLevel};
use hyper_daemon::client::send_request_with_timeout;
use hyper_daemon::protocol::{DaemonRequest, DaemonResponse, StatuslinePulseResponse};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct CliModelInfo {
    pub id: Option<String>,
    pub display_name: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct CliWorkspaceInfo {
    pub current_dir: Option<String>,
    pub project_dir: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct CliCurrentUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct CliContextWindow {
    pub total_input_tokens: Option<u64>,
    pub total_output_tokens: Option<u64>,
    pub context_window_size: Option<u64>,
    pub used_percentage: Option<f64>,
    pub remaining_percentage: Option<f64>,
    pub current_usage: Option<CliCurrentUsage>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct CliQuotaBucket {
    pub remaining_fraction: Option<f64>,
    pub reset_in_seconds: Option<u64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct CliCostInfo {
    pub total_usd: Option<f64>,
    pub subagent_usd: Option<f64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct CliStatusInput {
    pub cwd: Option<String>,
    pub session_id: Option<String>,
    pub conversation_id: Option<String>,
    pub conversation_title: Option<String>,
    pub transcript_path: Option<String>,
    pub model: Option<CliModelInfo>,
    pub workspace: Option<CliWorkspaceInfo>,
    pub context_window: Option<CliContextWindow>,
    pub exceeds_200k_tokens: Option<bool>,
    pub quota: BTreeMap<String, CliQuotaBucket>,
    pub agent_state: Option<String>,
    pub subagents: Option<Value>,
    pub cost: Option<CliCostInfo>,
}

#[derive(Debug, Serialize)]
struct StatuslineJsonOutput {
    model: String,
    project: String,
    agent_state: Option<String>,
    context_tokens: u64,
    context_limit: u64,
    context_used_pct: f64,
    cache_hit_ratio: f64,
    cache_warm_remaining_secs: u64,
    effective_cache_ttl_secs: u64,
    cache_ttl_source: String,
    cache_is_cold: bool,
    median_turn_ms: u64,
    median_llm_ms: u64,
    effective_tps: f64,
    hyper_adoption_ratio: f64,
    quota_remaining_pct: Option<f64>,
    cost_usd: Option<f64>,
    consecutive_error_streak: u32,
    unhealthy_subagents: usize,
    health: String,
}

fn read_stdin_nonblocking() -> String {
    if io::stdin().is_terminal() {
        return String::new();
    }
    let mut raw = String::new();
    let _ = io::stdin().read_to_string(&mut raw);
    raw
}

fn fetch_daemon_or_tail_pulse(input: &CliStatusInput) -> Option<StatuslinePulseResponse> {
    let conv_id = input
        .conversation_id
        .clone()
        .or_else(|| input.session_id.clone());
    let model_hint = input
        .model
        .as_ref()
        .and_then(|m| m.id.clone().or_else(|| m.display_name.clone()));

    // Non-spawning fast UDS query (35ms cap) so Antigravity CLI's 300ms refresh never blocks.
    if let Ok(DaemonResponse::Ok { data }) = send_request_with_timeout(
        &DaemonRequest::StatuslinePulse {
            conversation_id: conv_id.clone(),
            transcript_path: input.transcript_path.clone(),
            model: model_hint.clone(),
        },
        35,
    ) {
        if !data.is_null() {
            if let Ok(pulse) = serde_json::from_value::<StatuslinePulseResponse>(data) {
                return Some(pulse);
            }
        }
    }

    // Ultra-fast <1ms local tail fallback when daemon is stopped.
    let cfg = HyperConfig::load();
    let tpath = resolve_transcript_path(input.transcript_path.as_deref(), conv_id.as_deref())?;
    let tp = inspect_transcript_tail(&tpath, &cfg)?;
    let unhealthy_subagents =
        hyper_daemon::state::count_unhealthy_active_children(&tp.conversation_id, &cfg);
    Some(StatuslinePulseResponse {
        model: tp.model.or(model_hint),
        effective_cache_ttl_secs: tp.effective_cache_ttl_secs,
        cache_ttl_source: tp.cache_ttl_source,
        cache_warm_remaining_secs: tp.cache_warm_remaining_secs,
        cache_is_cold: tp.cache_is_cold,
        total_tokens: tp.total_tokens,
        effective_red_tokens: tp.effective_red_tokens,
        cache_hit_ratio: tp.cache_hit_ratio,
        health: tp.health,
        consecutive_error_streak: tp.consecutive_error_streak,
        unhealthy_subagents,
        median_turn_ms: 0,
        median_llm_ms: 0,
        effective_tps: 0.0,
        hyper_adoption_ratio: 0.0,
        total_turns: 0,
    })
}

fn short_model_name(input: &CliStatusInput, pulse: Option<&StatuslinePulseResponse>) -> String {
    let raw = input
        .model
        .as_ref()
        .and_then(|m| m.display_name.as_deref().or(m.id.as_deref()))
        .or_else(|| pulse.and_then(|p| p.model.as_deref()))
        .unwrap_or("Antigravity");
    let cleaned = raw
        .trim()
        .trim_start_matches("models/")
        .replace("gemini-4-", "G4-")
        .replace("claude-", "Claude ");
    let mut s: String = cleaned.chars().take(24).collect();
    if let Some(eff) = input
        .model
        .as_ref()
        .and_then(|m| m.effort.as_deref())
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        if !s.to_ascii_lowercase().contains(&eff.to_ascii_lowercase()) {
            s.push_str(&format!(" ({})", eff));
        }
    }
    s
}

fn project_label(input: &CliStatusInput) -> String {
    let dir = input
        .workspace
        .as_ref()
        .and_then(|w| w.project_dir.as_deref().or(w.current_dir.as_deref()))
        .or(input.cwd.as_deref())
        .unwrap_or("");
    Path::new(dir)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("workspace")
        .to_string()
}

fn fmt_tokens_short(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", (n as f64 / 1000.0).round() as u64)
    } else {
        format!("{}", n)
    }
}

fn fmt_secs_compact(secs: u64) -> String {
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}s", secs)
    }
}

fn sanitize_plain_title(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        // Strip any accidental CSI escape sequence.
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                let _ = chars.next();
                for esc in chars.by_ref() {
                    if ('@'..='~').contains(&esc) {
                        break;
                    }
                }
            }
            continue;
        }
        if c.is_control() {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn resolve_context_metrics(
    input: &CliStatusInput,
    pulse: Option<&StatuslinePulseResponse>,
) -> (u64, u64, f64) {
    let cli_cur = input
        .context_window
        .as_ref()
        .and_then(|cw| cw.current_usage.as_ref())
        .map(|cu| {
            cu.input_tokens.unwrap_or(0)
                + cu.cache_read_input_tokens.unwrap_or(0)
                + cu.cache_creation_input_tokens.unwrap_or(0)
        })
        .unwrap_or(0);

    let tokens = if cli_cur > 0 {
        cli_cur
    } else if let Some(p) = pulse.filter(|p| p.total_tokens > 0) {
        p.total_tokens
    } else {
        input
            .context_window
            .as_ref()
            .and_then(|cw| cw.total_input_tokens)
            .unwrap_or(0)
    };

    let limit = input
        .context_window
        .as_ref()
        .and_then(|cw| cw.context_window_size)
        .filter(|&v| v > 0)
        .or_else(|| pulse.map(|p| p.effective_red_tokens).filter(|&v| v > 0))
        .unwrap_or(280_000);

    let pct = input
        .context_window
        .as_ref()
        .and_then(|cw| cw.used_percentage)
        .unwrap_or_else(|| {
            if limit > 0 {
                (tokens as f64 / limit as f64) * 100.0
            } else {
                0.0
            }
        });

    (tokens, limit, pct.clamp(0.0, 100.0))
}

fn resolve_cache_hit(input: &CliStatusInput, pulse: Option<&StatuslinePulseResponse>) -> f64 {
    if let Some(cu) = input
        .context_window
        .as_ref()
        .and_then(|cw| cw.current_usage.as_ref())
    {
        let cr = cu.cache_read_input_tokens.unwrap_or(0);
        let denom = cr + cu.input_tokens.unwrap_or(0) + cu.cache_creation_input_tokens.unwrap_or(0);
        if denom > 0 {
            return cr as f64 / denom as f64;
        }
    }
    pulse.map(|p| p.cache_hit_ratio).unwrap_or(0.0)
}

fn lowest_quota_pct(input: &CliStatusInput) -> Option<f64> {
    input
        .quota
        .values()
        .filter_map(|q| q.remaining_fraction)
        .map(|f| (f * 100.0).clamp(0.0, 100.0))
        .reduce(f64::min)
}

fn active_subagent_count(input: &CliStatusInput) -> usize {
    match &input.subagents {
        Some(Value::Array(arr)) => arr.len(),
        Some(Value::Object(map)) => map
            .get("active")
            .and_then(Value::as_u64)
            .or_else(|| map.get("running").and_then(Value::as_u64))
            .unwrap_or(0) as usize,
        _ => 0,
    }
}

fn colorize(text: &str, code: &str, use_color: bool) -> String {
    if use_color {
        format!("\x1b[{}m{}\x1b[0m", code, text)
    } else {
        text.to_string()
    }
}

pub fn render_statusline(
    input: &CliStatusInput,
    pulse: Option<&StatuslinePulseResponse>,
    format: &str,
    title: bool,
    use_color: bool,
) -> String {
    let model = short_model_name(input, pulse);
    let project = project_label(input);
    let (ctx_tokens, ctx_limit, ctx_pct) = resolve_context_metrics(input, pulse);
    let cache_hit = resolve_cache_hit(input, pulse);
    let quota_pct = lowest_quota_pct(input);
    let cost_usd = input
        .cost
        .as_ref()
        .and_then(|c| c.total_usd)
        .filter(|&v| v > 0.0001);
    let subs = active_subagent_count(input);

    let eff_ttl = pulse.map(|p| p.effective_cache_ttl_secs).unwrap_or(240);
    let ttl_src = pulse
        .map(|p| p.cache_ttl_source.as_str())
        .unwrap_or("default");
    let warm_rem = pulse.map(|p| p.cache_warm_remaining_secs).unwrap_or(eff_ttl);
    let is_cold = pulse.map(|p| p.cache_is_cold).unwrap_or(false);
    let err_streak = pulse.map(|p| p.consecutive_error_streak).unwrap_or(0);
    let unhealthy_subs = pulse.map(|p| p.unhealthy_subagents).unwrap_or(0);
    let health = pulse.map(|p| p.health).unwrap_or(HealthLevel::Green);

    // Antigravity CLI `title` requires plain single-line text without ANSI escape codes.
    if title {
        let mut parts = Vec::new();
        if let Some(st) = input
            .agent_state
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "idle")
        {
            parts.push(format!("⚡ {}", st));
        } else {
            parts.push("⚡".to_string());
        }
        parts.push(project);
        if ctx_tokens > 0 {
            parts.push(fmt_tokens_short(ctx_tokens));
        }
        if pulse.is_some() {
            if is_cold {
                parts.push("COLD".to_string());
            } else {
                parts.push(format!("warm {}", fmt_secs_compact(warm_rem)));
            }
        }
        if err_streak >= 2 {
            parts.push(format!("loopx{}", err_streak));
        }
        if unhealthy_subs > 0 {
            parts.push(format!("⚠{} sub", unhealthy_subs));
        }
        return sanitize_plain_title(&parts.join(" · "));
    }

    if format.eq_ignore_ascii_case("json") {
        let payload = StatuslineJsonOutput {
            model,
            project,
            agent_state: input.agent_state.clone(),
            context_tokens: ctx_tokens,
            context_limit: ctx_limit,
            context_used_pct: (ctx_pct * 10.0).round() / 10.0,
            cache_hit_ratio: (cache_hit * 1000.0).round() / 1000.0,
            cache_warm_remaining_secs: warm_rem,
            effective_cache_ttl_secs: eff_ttl,
            cache_ttl_source: ttl_src.to_string(),
            cache_is_cold: is_cold,
            median_turn_ms: pulse.map(|p| p.median_turn_ms).unwrap_or(0),
            median_llm_ms: pulse.map(|p| p.median_llm_ms).unwrap_or(0),
            effective_tps: pulse.map(|p| p.effective_tps).unwrap_or(0.0),
            hyper_adoption_ratio: pulse.map(|p| p.hyper_adoption_ratio).unwrap_or(0.0),
            quota_remaining_pct: quota_pct.map(|v| (v * 10.0).round() / 10.0),
            cost_usd,
            consecutive_error_streak: err_streak,
            unhealthy_subagents: unhealthy_subs,
            health: format!("{:?}", health).to_ascii_lowercase(),
        };
        return serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    }

    let ctx_color = if health == HealthLevel::Red || ctx_pct >= 85.0 {
        "31"
    } else if health == HealthLevel::Yellow || ctx_pct >= 65.0 || input.exceeds_200k_tokens.unwrap_or(false) {
        "33"
    } else {
        "32"
    };
    let ctx_str = colorize(
        &format!(
            "ctx {}/{} ({:.0}%)",
            fmt_tokens_short(ctx_tokens),
            fmt_tokens_short(ctx_limit),
            ctx_pct
        ),
        ctx_color,
        use_color,
    );

    let cache_str = if pulse.is_some() {
        let ttl_tag = if ttl_src == "empirical" { "~" } else { "" };
        if is_cold {
            colorize(
                &format!(
                    "cache {:.0}% COLD (>{}{})",
                    cache_hit * 100.0,
                    ttl_tag,
                    fmt_secs_compact(eff_ttl)
                ),
                "31",
                use_color,
            )
        } else {
            let warm_color = if warm_rem <= 45 { "33" } else { "36" };
            colorize(
                &format!(
                    "cache {:.0}% warm {} ({}{})",
                    cache_hit * 100.0,
                    fmt_secs_compact(warm_rem),
                    ttl_tag,
                    fmt_secs_compact(eff_ttl)
                ),
                warm_color,
                use_color,
            )
        }
    } else {
        colorize(&format!("cache {:.0}%", cache_hit * 100.0), "36", use_color)
    };

    let sep = colorize(" · ", "90", use_color);
    let head = colorize(&format!("⚡ {}", model), "1;36", use_color);

    if format.eq_ignore_ascii_case("rich") {
        let filled = ((ctx_pct / 10.0).round() as usize).min(10);
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(10 - filled));
        let mut line1 = vec![
            head,
            colorize(&project, "37", use_color),
            colorize(&format!("[{}]", bar), ctx_color, use_color),
            ctx_str,
            cache_str,
        ];
        if subs > 0 {
            line1.push(colorize(&format!("{} sub", subs), "35", use_color));
        }
        if err_streak >= 2 {
            line1.push(colorize(&format!("⚠ loop×{}", err_streak), "1;31", use_color));
        }
        if unhealthy_subs > 0 {
            line1.push(colorize(&format!("⚠ {} sub", unhealthy_subs), "1;31", use_color));
        }

        let mut line2 = Vec::new();
        if let Some(p) = pulse.filter(|p| p.median_turn_ms > 0) {
            line2.push(format!(
                "P50 {:.1}s (LLM {:.1}s)",
                p.median_turn_ms as f64 / 1000.0,
                p.median_llm_ms as f64 / 1000.0
            ));
        }
        if let Some(p) = pulse.filter(|p| p.effective_tps > 0.1) {
            line2.push(format!("{:.1} tok/s", p.effective_tps));
        }
        if let Some(p) = pulse.filter(|p| p.total_turns > 0) {
            line2.push(format!("hyper {:.0}%", p.hyper_adoption_ratio * 100.0));
        }
        if let Some(q) = quota_pct {
            let qc = if q < 15.0 { "31" } else if q < 35.0 { "33" } else { "32" };
            line2.push(colorize(&format!("quota {:.0}%", q), qc, use_color));
        }
        if let Some(usd) = cost_usd {
            line2.push(format!("${:.2}", usd));
        }

        if line2.is_empty() {
            return line1.join(&sep);
        }
        return format!("{}\n  {}", line1.join(&sep), line2.join(&sep));
    }

    // Default `compact` single-line layout.
    let mut segs = vec![head, ctx_str, cache_str];
    if let Some(p) = pulse.filter(|p| p.median_turn_ms > 0) {
        segs.push(format!("P50 {:.1}s", p.median_turn_ms as f64 / 1000.0));
    }
    if let Some(p) = pulse.filter(|p| p.effective_tps > 0.1) {
        segs.push(format!("{:.0} t/s", p.effective_tps));
    }
    if let Some(p) = pulse.filter(|p| p.total_turns > 0) {
        segs.push(format!("hyper {:.0}%", p.hyper_adoption_ratio * 100.0));
    }
    if let Some(q) = quota_pct {
        let qc = if q < 15.0 { "31" } else if q < 35.0 { "33" } else { "32" };
        segs.push(colorize(&format!("q {:.0}%", q), qc, use_color));
    }
    if let Some(usd) = cost_usd {
        segs.push(format!("${:.2}", usd));
    }
    if err_streak >= 2 {
        segs.push(colorize(&format!("⚠ loop×{}", err_streak), "1;31", use_color));
    }
    if unhealthy_subs > 0 {
        segs.push(colorize(&format!("⚠ {} sub", unhealthy_subs), "1;31", use_color));
    }
    segs.join(&sep)
}

/// Infallible statusline command entrypoint — ALWAYS exits 0 so Antigravity CLI never trips
/// its 30-consecutive-failure disable latch.
pub fn run_statusline(format: &str, title: bool, no_color: bool) -> Result<()> {
    let raw = read_stdin_nonblocking();
    let input: CliStatusInput = if raw.trim().is_empty() {
        CliStatusInput::default()
    } else {
        serde_json::from_str(&raw).unwrap_or_default()
    };
    let pulse = fetch_daemon_or_tail_pulse(&input);
    let env_no_color = std::env::var_os("NO_COLOR").is_some();
    let use_color = !title && !no_color && !env_no_color && !format.eq_ignore_ascii_case("json");
    let rendered = render_statusline(&input, pulse.as_ref(), format, title, use_color);
    println!("{}", rendered);
    Ok(())
}

fn preferred_binary_path() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        let plugin_bin = home
            .join(".gemini")
            .join("config")
            .join("plugins")
            .join("agy-hyper")
            .join("bin")
            .join(if cfg!(windows) {
                "agy-hyper.exe"
            } else {
                "agy-hyper"
            });
        if plugin_bin.is_file() {
            return plugin_bin;
        }
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("agy-hyper"))
}

pub fn run_cli_setup(apply: bool, format: &str) -> Result<()> {
    let bin = preferred_binary_path();
    let bin_str = bin.display().to_string();
    let fmt_arg = match format.to_ascii_lowercase().as_str() {
        "rich" => "rich",
        "json" => "json",
        _ => "compact",
    };
    let status_cmd = format!("\"{}\" statusline --format {}", bin_str, fmt_arg);
    let title_cmd = format!("\"{}\" statusline --title", bin_str);

    let snippet = json!({
        "statusLine": {
            "type": "command",
            "command": status_cmd
        },
        "title": {
            "type": "command",
            "command": title_cmd
        }
    });

    let settings_path = dirs::home_dir()
        .ok_or_else(|| anyhow!("cannot resolve home directory"))?
        .join(".gemini")
        .join("antigravity-cli")
        .join("settings.json");

    if !apply {
        println!(
            "# Add these keys to {} (or re-run with `agy-hyper cli-setup --apply`):\n",
            settings_path.display()
        );
        println!("{}", serde_json::to_string_pretty(&snippet)?);
        return Ok(());
    }

    let mut root = if settings_path.exists() {
        let existing = fs::read_to_string(&settings_path)?;
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let bak = settings_path.with_extension(format!("json.bak.{}", ts));
        fs::write(&bak, &existing)?;
        println!("Backed up existing settings -> {}", bak.display());
        serde_json::from_str::<Value>(&existing)
            .map_err(|e| anyhow!("existing settings.json is not valid JSON: {}", e))?
    } else {
        if let Some(parent) = settings_path.parent() {
            fs::create_dir_all(parent)?;
        }
        json!({})
    };

    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings.json root must be a JSON object"))?;
    obj.insert("statusLine".into(), snippet["statusLine"].clone());
    obj.insert("title".into(), snippet["title"].clone());

    let serialized = serde_json::to_string_pretty(&root)?;
    let tmp = settings_path.with_extension("json.tmp");
    fs::write(&tmp, format!("{}\n", serialized))?;
    fs::rename(&tmp, &settings_path)?;
    println!("Updated {} with AGY Hyper statusLine & title.", settings_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cli_input() -> CliStatusInput {
        serde_json::from_value(json!({
            "cwd": "/Users/dev/agy-hyper",
            "conversation_id": "conv-123",
            "model": { "id": "claude-opus-4-6", "display_name": "Opus 4.6", "effort": "high" },
            "workspace": { "project_dir": "/Users/dev/agy-hyper" },
            "context_window": {
                "context_window_size": 200000,
                "used_percentage": 32.0,
                "current_usage": {
                    "input_tokens": 4000,
                    "cache_read_input_tokens": 60000,
                    "output_tokens": 1200
                }
            },
            "quota": { "claude": { "remaining_fraction": 0.78, "reset_in_seconds": 3600 } },
            "agent_state": "working",
            "cost": { "total_usd": 0.42 }
        }))
        .unwrap()
    }

    fn sample_pulse() -> StatuslinePulseResponse {
        StatuslinePulseResponse {
            model: Some("claude-opus-4-6".into()),
            effective_cache_ttl_secs: 180,
            cache_ttl_source: "empirical".into(),
            cache_warm_remaining_secs: 132,
            cache_is_cold: false,
            total_tokens: 64_000,
            effective_red_tokens: 175_000,
            cache_hit_ratio: 0.94,
            health: HealthLevel::Green,
            consecutive_error_streak: 0,
            unhealthy_subagents: 0,
            median_turn_ms: 3100,
            median_llm_ms: 2400,
            effective_tps: 58.2,
            hyper_adoption_ratio: 0.81,
            total_turns: 12,
        }
    }

    #[test]
    fn renders_compact_rich_json_and_plain_title_cleanly() {
        let input = sample_cli_input();
        let pulse = sample_pulse();

        let compact = render_statusline(&input, Some(&pulse), "compact", false, false);
        assert!(compact.contains("Opus 4.6 (high)"));
        assert!(compact.contains("ctx 64k/200k (32%)"));
        assert!(compact.contains("warm 2m12s (~3m00s)"));
        assert!(compact.contains("P50 3.1s"));
        assert!(compact.contains("58 t/s"));
        assert!(compact.contains("hyper 81%"));
        assert!(compact.contains("q 78%"));
        assert!(compact.contains("$0.42"));

        let rich = render_statusline(&input, Some(&pulse), "rich", false, false);
        assert!(rich.contains('\n'));
        assert!(rich.contains("LLM 2.4s"));

        // Title MUST contain zero ANSI escape codes and zero newlines even if use_color=true.
        let title = render_statusline(&input, Some(&pulse), "compact", true, true);
        assert!(!title.contains('\x1b'));
        assert!(!title.contains('\n'));
        assert!(title.contains("working"));
        assert!(title.contains("agy-hyper"));
        assert!(title.contains("64k"));

        let json_str = render_statusline(&input, Some(&pulse), "json", false, false);
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["effective_cache_ttl_secs"], 180);
        assert_eq!(parsed["cache_ttl_source"], "empirical");
    }

    #[test]
    fn handles_empty_stdin_without_panicking() {
        let empty = CliStatusInput::default();
        let compact = render_statusline(&empty, None, "compact", false, false);
        assert!(compact.contains("Antigravity"));
        let title = render_statusline(&empty, None, "compact", true, false);
        assert!(!title.is_empty());
        assert!(!title.contains('\n'));
    }
}
