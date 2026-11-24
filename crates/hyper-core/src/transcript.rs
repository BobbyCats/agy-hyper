use crate::config::HyperConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const TAIL_READ_BYTES: u64 = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthLevel {
    Green,
    Yellow,
    Red,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptPulse {
    pub conversation_id: String,
    pub transcript_path: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_context_limit: u64,
    #[serde(default)]
    pub pricing_cliff_tokens: u64,
    #[serde(default)]
    pub effective_yellow_tokens: u64,
    #[serde(default)]
    pub effective_red_tokens: u64,
    pub max_step: u64,
    pub steps_since_compact: u64,
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub cache_hit_ratio: f64,
    pub recent_timeouts: u32,
    pub recent_resets: u32,
    pub media_items_recent: u32,
    #[serde(default)]
    pub checkpoint_count: u32,
    pub last_planner_epoch_ms: Option<i64>,
    pub seconds_since_last_planner: Option<u64>,
    pub cache_warm_remaining_secs: u64,
    #[serde(default = "default_effective_ttl_secs")]
    pub effective_cache_ttl_secs: u64,
    #[serde(default = "default_ttl_source")]
    pub cache_ttl_source: String,
    pub cache_is_cold: bool,
    pub multimodal_cliff_risk: bool,
    #[serde(default)]
    pub recently_compacted: bool,
    #[serde(default)]
    pub model_switched_from: Option<String>,
    #[serde(default)]
    pub consecutive_error_streak: u32,
    #[serde(default)]
    pub repeat_error_signature: Option<String>,
    pub health: HealthLevel,
    #[serde(default)]
    pub health_reason: String,
}

impl TranscriptPulse {
    /// Apply a resolved empirical (or configured) cache TTL in-place without re-reading the transcript.
    pub fn apply_cache_ttl(&mut self, ttl_secs: u64, source: impl Into<String>) {
        if ttl_secs < 30 {
            return;
        }
        self.effective_cache_ttl_secs = ttl_secs;
        self.cache_ttl_source = source.into();
        self.cache_warm_remaining_secs = match self.seconds_since_last_planner {
            Some(elapsed) => ttl_secs.saturating_sub(elapsed),
            None => ttl_secs,
        };
        self.cache_is_cold = self
            .seconds_since_last_planner
            .map(|e| e >= ttl_secs)
            .unwrap_or(false);
    }
}

fn default_effective_ttl_secs() -> u64 {
    crate::config::DEFAULT_CACHE_WARM_SECS
}

fn default_ttl_source() -> String {
    "default".to_string()
}

/// Infer model context window, 2x long-context pricing cliff, and effective guard thresholds.
/// - 1M-window models (Gemini Pro/Neon, Claude 1M): 200k is the 2x pricing cliff & TTFT degradation onset;
///   180k Yellow / 280k Red protect latency, attention quality, and 5m0s stream stability.
/// - 200k-window models (Claude standard, GPT-4o, o1): auto-clamp guard lines to 140k Yellow / 175k Red
///   so sessions never slam into a hard 200k context overflow unwarned.
pub fn infer_model_context_profile(model: Option<&str>, cfg: &HyperConfig) -> (u64, u64, u64, u64) {
    let m = model.unwrap_or("").to_ascii_lowercase();
    if m.contains("gemini") || m.contains("1m") {
        (
            1_000_000,
            200_000,
            cfg.watermark_yellow_tokens,
            cfg.watermark_red_tokens,
        )
    } else if m.contains("gpt-5") || m.contains("o4") || (m.contains("o3") && !m.contains("mini")) {
        (
            400_000,
            200_000,
            cfg.watermark_yellow_tokens.min(180_000),
            cfg.watermark_red_tokens.min(280_000),
        )
    } else if m.contains("deepseek") || m.contains("qwen") || m.contains("llama") || m.contains("mistral") {
        (
            128_000,
            96_000,
            cfg.watermark_yellow_tokens.min(96_000),
            cfg.watermark_red_tokens.min(115_000),
        )
    } else if !m.is_empty() {
        (
            200_000,
            160_000,
            cfg.watermark_yellow_tokens.min(140_000),
            cfg.watermark_red_tokens.min(175_000),
        )
    } else {
        (
            1_000_000,
            200_000,
            cfg.watermark_yellow_tokens,
            cfg.watermark_red_tokens,
        )
    }
}

fn is_compact_user_input(content: &str) -> bool {
    if content.contains("<CONTEXT_SUMMARY>") {
        return true;
    }
    let body = if let (Some(s), Some(e)) = (content.find("<USER_REQUEST>"), content.find("</USER_REQUEST>")) {
        &content[s + "<USER_REQUEST>".len()..e]
    } else {
        content
    };
    body.trim() == "/compact"
}

fn is_user_cancel_error(msg: &str) -> bool {
    let lower = msg.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    lower.contains("context canceled")
        || lower.contains("canceled by user")
        || lower.contains("cancelled by user")
        || lower.contains("aborted by user")
        || lower.contains("user canceled")
        || lower.contains("user cancelled")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportErrorKind {
    None,
    UserCanceled,
    Timeout,
    Reset,
}

/// Pure classifier for Antigravity transport/upstream API errors.
/// Derived from real local transcripts so bare `: EOF`, interrupted streams,
/// TLS/TCP timeouts, and 503 capacity failures are never silently dropped.
pub fn classify_transport_error(msg: &str) -> TransportErrorKind {
    if is_user_cancel_error(msg) {
        return TransportErrorKind::UserCanceled;
    }
    let lower = msg.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return TransportErrorKind::None;
    }
    if lower.contains("stream idle timeout")
        || lower.contains("context deadline exceeded")
        || lower.contains("tls handshake timeout")
        || lower.contains("i/o timeout")
        || lower.contains("operation timed out")
        || lower.contains("code 503")
        || lower.contains("status 503")
        || lower.contains("(503)")
        || lower.contains(" 503:")
        || lower.contains(" 503 ")
    {
        return TransportErrorKind::Timeout;
    }
    let is_bare_eof = lower.ends_with(": eof")
        || lower.contains(": eof\n")
        || lower.contains(": eof ")
        || lower.ends_with(" eof")
        || lower == "eof";
    if lower.contains("connection reset by peer")
        || lower.contains("unexpected eof")
        || is_bare_eof
        || lower.contains("stream was interrupted")
        || lower.contains("broken pipe")
        || lower.contains("use of closed network connection")
        || lower.contains("no such host")
        || ((lower.contains("api error") || lower.contains("request failed"))
            && (lower.contains("read tcp ") || lower.contains("write tcp ") || lower.contains("dial tcp")))
    {
        return TransportErrorKind::Reset;
    }
    TransportErrorKind::None
}

/// Produce a compact, readable single-line error signature that preserves the trailing cause
/// (e.g. `EOF`, `TLS handshake timeout`, `connection reset by peer`) instead of truncating
/// inside long upstream endpoint URLs or leaking local proxy socket addresses.
pub fn summarize_error_line(raw: &str) -> String {
    let first_line = raw
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if first_line.is_empty() {
        return String::new();
    }

    let mut normalized = first_line.to_string();
    if let Some(post_pos) = normalized.find("Post \"http") {
        let after_post = &normalized[post_pos + 6..];
        if let Some(end_q) = after_post.find("\": ") {
            let prefix = normalized[..post_pos]
                .trim_end()
                .trim_end_matches("request failed:")
                .trim_end();
            let tail = after_post[end_q + 3..].trim();
            normalized = if prefix.is_empty() {
                tail.to_string()
            } else {
                format!("{} {}", prefix, tail)
            };
        }
    }

    // Strip ephemeral local socket tuples like `read tcp 127.0.0.1:54321->127.0.0.1:8080: `
    for verb in ["read tcp ", "write tcp ", "dial tcp "] {
        if let Some(pos) = normalized.find(verb) {
            let after = &normalized[pos + verb.len()..];
            if let Some(colon) = after.find(": ") {
                let before = &normalized[..pos];
                let rest = &after[colon + 2..];
                normalized = format!("{}{}", before, rest);
            }
        }
    }

    normalized.chars().take(96).collect()
}

/// Ultra-fast `<1.5ms` tail reader of Antigravity's `transcript.jsonl`.
/// Native zero-dependency Rust guard providing:
/// - Compaction-aware windowing (zero post-`/compact` false-alarm storms!)
/// - Prompt Cache Warmth countdown
/// - >180k Multimodal Image TTFT cliff detection
/// - Live cache hit ratio & exact token breakdown
pub fn inspect_transcript_tail(path: &Path, cfg: &HyperConfig) -> Option<TranscriptPulse> {
    inspect_transcript_tail_with_ttl(path, cfg, None)
}

pub fn inspect_transcript_tail_with_ttl(
    path: &Path,
    cfg: &HyperConfig,
    ttl_override: Option<(u64, &str)>,
) -> Option<TranscriptPulse> {
    let mut file = File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let seeked = size > TAIL_READ_BYTES;
    if seeked {
        file.seek(SeekFrom::Start(size - TAIL_READ_BYTES)).ok()?;
    }
    let mut raw_bytes = Vec::with_capacity((size.min(TAIL_READ_BYTES) + 1) as usize);
    file.read_to_end(&mut raw_bytes).ok()?;
    let raw = String::from_utf8_lossy(&raw_bytes);

    let mut lines: Vec<&str> = raw.split('\n').collect();
    if seeked && !lines.is_empty() {
        lines.remove(0);
    }

    let valid: Vec<&str> = lines
        .into_iter()
        .map(str::trim)
        .filter(|l| l.starts_with("{\"") && l.ends_with('}'))
        .collect();
    if valid.is_empty() {
        return None;
    }

    let slice_start = valid.len().saturating_sub(80);
    let mut parsed: Vec<Value> = Vec::with_capacity(valid.len() - slice_start);
    for ln in &valid[slice_start..] {
        if let Ok(v) = serde_json::from_str::<Value>(ln) {
            parsed.push(v);
        }
    }
    if parsed.is_empty() {
        return None;
    }

    // Detect the most recent `/compact` or `CHECKPOINT` boundary so pre-compaction
    // step counts, images, and timeouts never ghost-trigger false alarms after compaction.
    let mut compact_start_idx = 0usize;
    let mut compact_base_step = 0u64;
    let mut prev_planner_tokens = 0u64;
    let mut checkpoint_count = 0u32;
    for (idx, obj) in parsed.iter().enumerate() {
        let typ = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let step_idx = obj.get("step_index").and_then(Value::as_u64).unwrap_or(0);
        if typ == "CHECKPOINT" {
            checkpoint_count = checkpoint_count.saturating_add(1);
            compact_start_idx = idx;
            compact_base_step = step_idx;
            prev_planner_tokens = 0;
        } else if typ == "USER_INPUT" {
            let content = obj.get("content").and_then(Value::as_str).unwrap_or("");
            if is_compact_user_input(content) {
                compact_start_idx = idx;
                compact_base_step = step_idx;
                prev_planner_tokens = 0;
            }
        } else if typ == "PLANNER_RESPONSE" {
            let inp = obj.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
            let cache = obj.get("cache_read_tokens").and_then(Value::as_u64).unwrap_or(0);
            let tot = inp + cache;
            if tot > 0 {
                if prev_planner_tokens >= 80_000 && tot * 100 <= prev_planner_tokens * 55 {
                    compact_start_idx = idx;
                    compact_base_step = step_idx;
                }
                prev_planner_tokens = tot;
            }
        }
    }

    let mut last_model_before_compact: Option<String> = None;
    for obj in &parsed[..compact_start_idx] {
        if obj.get("type").and_then(Value::as_str) == Some("PLANNER_RESPONSE") {
            if let Some(m) = obj.get("model").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                last_model_before_compact = Some(m.to_string());
            }
        }
    }

    let active_window = &parsed[compact_start_idx..];

    let mut max_step = 0u64;
    let mut input_tokens = 0u64;
    let mut cache_read_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut model: Option<String> = None;
    let mut switched_from: Option<String> = None;
    let mut turns_since_model_switch = 0u32;
    let mut planner_turns_in_window = 0u32;
    let mut last_planner_epoch_ms: Option<i64> = None;
    let mut media_items_recent = 0u32;

    for obj in active_window {
        if let Some(idx) = obj.get("step_index").and_then(Value::as_u64) {
            max_step = max_step.max(idx);
        }
        if let Some(media) = obj.get("media").and_then(Value::as_array) {
            media_items_recent += media.len() as u32;
        }
        if obj.get("type").and_then(Value::as_str) == Some("PLANNER_RESPONSE") {
            planner_turns_in_window = planner_turns_in_window.saturating_add(1);
            if let Some(m) = obj.get("model").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                if let Some(prev) = model.as_deref() {
                    if !prev.eq_ignore_ascii_case(m) {
                        switched_from = Some(prev.to_string());
                        turns_since_model_switch = 0;
                    } else {
                        turns_since_model_switch = turns_since_model_switch.saturating_add(1);
                    }
                }
                model = Some(m.to_string());
            }
            let inp = obj.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
            let cache = obj
                .get("cache_read_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let out = obj
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if inp > 0 || cache > 0 {
                input_tokens = inp;
                cache_read_tokens = cache;
                output_tokens = out;
            }
            if let Some(ts) = obj.get("created_at").and_then(Value::as_str) {
                if let Some(ms) = parse_iso8601_epoch_ms(ts) {
                    last_planner_epoch_ms = Some(ms);
                }
            }
        }
    }

    let recently_compacted = compact_start_idx > 0 && planner_turns_in_window <= 2;
    let model_switched_from = if recently_compacted {
        if switched_from.is_some() {
            switched_from
        } else if let (Some(prev), Some(curr)) =
            (last_model_before_compact.as_deref(), model.as_deref())
        {
            if !prev.eq_ignore_ascii_case(curr) {
                Some(prev.to_string())
            } else {
                None
            }
        } else {
            None
        }
    } else if turns_since_model_switch <= 1 {
        switched_from
    } else {
        None
    };

    let steps_since_compact = if compact_base_step > 0 {
        max_step.saturating_sub(compact_base_step)
    } else {
        max_step
    };

    let mut recent_timeouts = 0u32;
    let mut recent_resets = 0u32;
    let recent_start = active_window.len().saturating_sub(14);
    for obj in &active_window[recent_start..] {
        if obj.get("type").and_then(Value::as_str) == Some("ERROR_MESSAGE") {
            let err = obj
                .get("error")
                .and_then(Value::as_str)
                .or_else(|| obj.get("content").and_then(Value::as_str))
                .unwrap_or("");
            match classify_transport_error(err) {
                TransportErrorKind::Timeout => recent_timeouts += 1,
                TransportErrorKind::Reset => recent_resets += 1,
                TransportErrorKind::UserCanceled | TransportErrorKind::None => {}
            }
        }
    }

    // Detect consecutive tail error streaks (ignoring user-cancelled steps) for loop-breaker guards.
    let mut consecutive_error_streak = 0u32;
    let mut repeat_error_signature: Option<String> = None;
    for obj in active_window.iter().rev() {
        let typ = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let status = obj.get("status").and_then(Value::as_str).unwrap_or("");
        if typ == "SYSTEM_MESSAGE" || typ == "USER_INPUT" {
            continue;
        }
        let is_err = typ == "ERROR_MESSAGE" || status.eq_ignore_ascii_case("ERROR");
        if is_err {
            let err_text = obj
                .get("error")
                .and_then(Value::as_str)
                .or_else(|| obj.get("content").and_then(Value::as_str))
                .unwrap_or("");
            if is_user_cancel_error(err_text) {
                break;
            }
            consecutive_error_streak = consecutive_error_streak.saturating_add(1);
            if repeat_error_signature.is_none() {
                let short = summarize_error_line(if err_text.trim().is_empty() {
                    typ
                } else {
                    err_text
                });
                if !short.is_empty() {
                    repeat_error_signature = Some(short);
                }
            }
        } else {
            break;
        }
    }
    if consecutive_error_streak < 2 {
        repeat_error_signature = None;
    }

    let total_tokens = input_tokens + cache_read_tokens;
    let cache_hit_ratio = if total_tokens > 0 {
        cache_read_tokens as f64 / total_tokens as f64
    } else {
        0.0
    };

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let seconds_since_last_planner = last_planner_epoch_ms.map(|ts| {
        if now_ms > ts {
            ((now_ms - ts) / 1000) as u64
        } else {
            0
        }
    });
    let (warm_win, ttl_source) = match ttl_override {
        Some((secs, src)) if secs >= 30 => (secs, src.to_string()),
        _ => (cfg.cache_warm_window_secs, "default".to_string()),
    };
    let cache_warm_remaining_secs = match seconds_since_last_planner {
        Some(elapsed) => warm_win.saturating_sub(elapsed),
        None => warm_win,
    };
    let cache_is_cold = seconds_since_last_planner
        .map(|e| e >= warm_win)
        .unwrap_or(false);

    let (model_context_limit, pricing_cliff_tokens, effective_yellow_tokens, effective_red_tokens) =
        infer_model_context_profile(model.as_deref(), cfg);

    let multimodal_cliff_risk =
        media_items_recent > 0 && total_tokens >= cfg.multimodal_cliff_tokens.min(effective_yellow_tokens);

    // Ground-truth token watermark takes precedence whenever tokens are reported.
    // Step watermarks only serve as fallback when a provider reports 0 tokens.
    let step_red = total_tokens == 0 && steps_since_compact >= cfg.watermark_red_steps;
    let step_yellow = total_tokens == 0 && steps_since_compact >= cfg.watermark_yellow_steps;

    let (health, health_reason) = if total_tokens >= effective_red_tokens {
        (HealthLevel::Red, "token_red")
    } else if consecutive_error_streak >= 3 {
        (HealthLevel::Red, "error_loop")
    } else if recent_timeouts >= 2 {
        (HealthLevel::Red, "stream_timeout")
    } else if step_red {
        (HealthLevel::Red, "step_red")
    } else if consecutive_error_streak == 2 {
        (HealthLevel::Yellow, "error_loop")
    } else if multimodal_cliff_risk {
        (HealthLevel::Yellow, "multimodal_cliff")
    } else if total_tokens >= effective_yellow_tokens {
        (HealthLevel::Yellow, "token_yellow")
    } else if recent_timeouts == 1 {
        (HealthLevel::Yellow, "stream_timeout")
    } else if recent_resets >= 2 {
        (HealthLevel::Yellow, "proxy_reset")
    } else if step_yellow {
        (HealthLevel::Yellow, "step_yellow")
    } else {
        (HealthLevel::Green, "healthy")
    };

    let conversation_id = path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    Some(TranscriptPulse {
        conversation_id,
        transcript_path: path.display().to_string(),
        model,
        model_context_limit,
        pricing_cliff_tokens,
        effective_yellow_tokens,
        effective_red_tokens,
        max_step,
        steps_since_compact,
        total_tokens,
        input_tokens,
        cache_read_tokens,
        output_tokens,
        cache_hit_ratio,
        recent_timeouts,
        recent_resets,
        media_items_recent,
        checkpoint_count,
        last_planner_epoch_ms,
        seconds_since_last_planner,
        cache_warm_remaining_secs,
        effective_cache_ttl_secs: warm_win,
        cache_ttl_source: ttl_source,
        cache_is_cold,
        multimodal_cliff_risk,
        recently_compacted,
        model_switched_from,
        consecutive_error_streak,
        repeat_error_signature,
        health,
        health_reason: health_reason.to_string(),
    })
}

pub fn resolve_transcript_path(
    explicit: Option<&str>,
    conversation_id: Option<&str>,
) -> Option<PathBuf> {
    if let Some(p) = explicit {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    if let Some(conv) = conversation_id.and_then(crate::config::sanitize_id) {
        if let Ok(custom) = std::env::var("AGY_HYPER_BRAIN_DIR") {
            let trimmed = custom.trim();
            if !trimmed.is_empty() {
                let candidate = PathBuf::from(trimmed)
                    .join(conv)
                    .join(".system_generated")
                    .join("logs")
                    .join("transcript.jsonl");
                if candidate.exists() {
                    return Some(candidate);
                }
                return None;
            }
        }
        let home = dirs::home_dir()?;
        for product in ["antigravity", "antigravity-ide", "antigravity-cli"] {
            let candidate = home
                .join(".gemini")
                .join(product)
                .join("brain")
                .join(conv)
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

pub fn parse_iso8601_epoch_ms(ts: &str) -> Option<i64> {
    let odt =
        time::OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339).ok()?;
    Some((odt.unix_timestamp_nanos() / 1_000_000) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn detects_red_watermark_and_multimodal_cliff() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("transcript.jsonl");
        let content = concat!(
            "{\"step_index\":10,\"type\":\"USER_INPUT\",\"media\":[{\"mime_type\":\"image/png\",\"uri\":\"/tmp/a.png\"}]}\n",
            "{\"step_index\":11,\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":40000,\"cache_read_tokens\":250000,\"output_tokens\":800,\"created_at\":\"2026-11-20T12:00:00Z\"}\n"
        );
        fs::write(&p, content).unwrap();
        let cfg = HyperConfig::default();
        let pulse = inspect_transcript_tail(&p, &cfg).unwrap();
        assert_eq!(pulse.total_tokens, 290_000);
        assert_eq!(pulse.health, HealthLevel::Red);
        assert!(pulse.multimodal_cliff_risk);
        assert!(pulse.cache_hit_ratio > 0.85);
    }

    #[test]
    fn post_compact_high_step_count_resets_to_green_healthy() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("transcript.jsonl");
        let content = concat!(
            "{\"step_index\":260,\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":30000,\"cache_read_tokens\":250000,\"output_tokens\":500,\"created_at\":\"2026-11-20T12:00:00Z\"}\n",
            "{\"step_index\":261,\"type\":\"ERROR_MESSAGE\",\"error\":\"stream idle timeout\"}\n",
            "{\"step_index\":262,\"type\":\"USER_INPUT\",\"content\":\"<USER_REQUEST>\\n/compact \\n</USER_REQUEST>\\n<ADDITIONAL_METADATA>\"}\n",
            "{\"step_index\":310,\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":12000,\"cache_read_tokens\":35000,\"output_tokens\":400,\"created_at\":\"2026-11-20T12:05:00Z\"}\n"
        );
        fs::write(&p, content).unwrap();
        let cfg = HyperConfig::default();
        let pulse = inspect_transcript_tail(&p, &cfg).unwrap();
        assert_eq!(pulse.total_tokens, 47_000);
        assert_eq!(pulse.max_step, 310);
        assert_eq!(pulse.steps_since_compact, 48);
        assert_eq!(pulse.recent_timeouts, 0);
        assert_eq!(pulse.health, HealthLevel::Green);
        assert_eq!(pulse.health_reason, "healthy");
    }

    #[test]
    fn model_aware_watermark_clamps_200k_models_and_tags_proxy_reset_reason() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("transcript.jsonl");
        let content = "{\"step_index\":1,\"type\":\"PLANNER_RESPONSE\",\"model\":\"claude-sonnet-4\",\"input_tokens\":25000,\"cache_read_tokens\":155000,\"output_tokens\":200,\"created_at\":\"2026-11-20T12:00:00Z\"}\n";
        fs::write(&p, content).unwrap();
        let cfg = HyperConfig::default();
        let pulse = inspect_transcript_tail(&p, &cfg).unwrap();
        assert_eq!(pulse.model.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(pulse.model_context_limit, 200_000);
        assert_eq!(pulse.effective_yellow_tokens, 140_000);
        assert_eq!(pulse.effective_red_tokens, 175_000);
        assert_eq!(pulse.health, HealthLevel::Red);
        assert_eq!(pulse.health_reason, "token_red");

        let p2 = dir.path().join("transcript2.jsonl");
        let content2 = concat!(
            "{\"step_index\":1,\"type\":\"ERROR_MESSAGE\",\"error\":\"read: connection reset by peer\"}\n",
            "{\"step_index\":2,\"type\":\"ERROR_MESSAGE\",\"error\":\"read: connection reset by peer\"}\n",
            "{\"step_index\":3,\"type\":\"PLANNER_RESPONSE\",\"model\":\"gemini-4-neon-xhigh\",\"input_tokens\":10000,\"cache_read_tokens\":35000,\"output_tokens\":200,\"created_at\":\"2026-11-20T12:00:00Z\"}\n"
        );
        fs::write(&p2, content2).unwrap();
        let pulse2 = inspect_transcript_tail(&p2, &cfg).unwrap();
        assert_eq!(pulse2.health, HealthLevel::Yellow);
        assert_eq!(pulse2.health_reason, "proxy_reset");
    }

    #[test]
    fn ignores_user_canceled_errors_in_health_classification() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("transcript.jsonl");
        let content = concat!(
            "{\"step_index\":1,\"type\":\"ERROR_MESSAGE\",\"error\":\"context canceled: stream idle timeout\"}\n",
            "{\"step_index\":2,\"type\":\"ERROR_MESSAGE\",\"error\":\"cancelled by user: connection reset by peer\"}\n",
            "{\"step_index\":3,\"type\":\"ERROR_MESSAGE\",\"error\":\"aborted by user\"}\n",
            "{\"step_index\":4,\"type\":\"PLANNER_RESPONSE\",\"model\":\"gemini-4-neon-xhigh\",\"input_tokens\":10000,\"cache_read_tokens\":40000,\"output_tokens\":200,\"created_at\":\"2026-11-20T12:00:00Z\"}\n"
        );
        fs::write(&p, content).unwrap();
        let cfg = HyperConfig::default();
        let pulse = inspect_transcript_tail(&p, &cfg).unwrap();
        assert_eq!(pulse.recent_timeouts, 0);
        assert_eq!(pulse.recent_resets, 0);
        assert_eq!(pulse.health, HealthLevel::Green);
        assert_eq!(pulse.health_reason, "healthy");
    }

    #[test]
    fn detects_recent_compaction_and_cross_model_switch() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("transcript.jsonl");
        let content = concat!(
            "{\"step_index\":50,\"type\":\"PLANNER_RESPONSE\",\"model\":\"gemini-4-hydrogen-tiered-xhigh\",\"input_tokens\":30000,\"cache_read_tokens\":220000,\"output_tokens\":500,\"created_at\":\"2026-11-20T12:00:00Z\"}\n",
            "{\"step_index\":51,\"type\":\"USER_INPUT\",\"content\":\"<USER_REQUEST>\\n/compact\\n</USER_REQUEST>\"}\n",
            "{\"step_index\":52,\"type\":\"PLANNER_RESPONSE\",\"model\":\"gemini-4-carbon-medium\",\"input_tokens\":32000,\"cache_read_tokens\":0,\"output_tokens\":300,\"created_at\":\"2026-11-20T12:01:00Z\"}\n"
        );
        fs::write(&p, content).unwrap();
        let cfg = HyperConfig::default();
        let pulse = inspect_transcript_tail(&p, &cfg).unwrap();
        assert!(pulse.recently_compacted);
        assert_eq!(
            pulse.model_switched_from.as_deref(),
            Some("gemini-4-hydrogen-tiered-xhigh")
        );
        assert_eq!(pulse.model.as_deref(), Some("gemini-4-carbon-medium"));
        assert_eq!(pulse.health, HealthLevel::Green);
    }

    #[test]
    fn detects_consecutive_error_streak_and_applies_empirical_ttl() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("transcript.jsonl");
        let content = concat!(
            "{\"step_index\":1,\"type\":\"PLANNER_RESPONSE\",\"model\":\"gemini-4-neon-xhigh\",\"input_tokens\":5000,\"cache_read_tokens\":55000,\"output_tokens\":100,\"created_at\":\"2026-11-20T12:00:00Z\"}\n",
            "{\"step_index\":2,\"type\":\"GENERIC\",\"status\":\"ERROR\",\"content\":\"exit code 1: test failed\"}\n",
            "{\"step_index\":3,\"type\":\"ERROR_MESSAGE\",\"error\":\"exit code 1: test failed\"}\n"
        );
        fs::write(&p, content).unwrap();
        let cfg = HyperConfig::default();
        let pulse = inspect_transcript_tail_with_ttl(&p, &cfg, Some((180, "empirical"))).unwrap();
        assert_eq!(pulse.effective_cache_ttl_secs, 180);
        assert_eq!(pulse.cache_ttl_source, "empirical");
        assert_eq!(pulse.consecutive_error_streak, 2);
        assert_eq!(pulse.health, HealthLevel::Yellow);
        assert_eq!(pulse.health_reason, "error_loop");
        assert!(pulse.repeat_error_signature.as_deref().unwrap().contains("exit code 1"));
    }

    #[test]
    fn classifies_bare_eof_interrupted_tls_and_escalates_error_loop() {
        assert_eq!(
            classify_transport_error(
                "API error (attempt 6): request failed: Post \"https://example.invalid/v1:streamGenerateContent?alt=sse\": EOF"
            ),
            TransportErrorKind::Reset
        );
        assert_eq!(
            classify_transport_error(
                "The stream was interrupted. Please continue the task you were working on."
            ),
            TransportErrorKind::Reset
        );
        assert_eq!(
            classify_transport_error(
                "API error (attempt 2): request failed: Post \"https://example.invalid/v1:streamGenerateContent\": net/http: TLS handshake timeout"
            ),
            TransportErrorKind::Timeout
        );
        assert_eq!(
            classify_transport_error("context canceled: Post \"https://example.invalid\": EOF"),
            TransportErrorKind::UserCanceled
        );
        assert_eq!(
            summarize_error_line(
                "API error (attempt 6): request failed: Post \"https://example.invalid/v1internal:streamGenerateContent?alt=sse\": EOF"
            ),
            "API error (attempt 6): EOF"
        );
        assert_eq!(
            summarize_error_line(
                "API error (attempt 1): request failed: Post \"https://example.invalid/v1\": read tcp 127.0.0.1:51234->127.0.0.1:8080: read: connection reset by peer"
            ),
            "API error (attempt 1): read: connection reset by peer"
        );

        let dir = tempdir().unwrap();

        // Single transient EOF attempt stays Green (no wolf-crying on attempt 1).
        let p_one = dir.path().join("single_eof.jsonl");
        let single_content = concat!(
            "{\"step_index\":10,\"type\":\"PLANNER_RESPONSE\",\"model\":\"gemini-4-hydrogen-tiered-high\",\"input_tokens\":5000,\"cache_read_tokens\":65000,\"output_tokens\":200,\"created_at\":\"2026-11-23T03:54:00Z\"}\n",
            "{\"step_index\":11,\"type\":\"ERROR_MESSAGE\",\"status\":\"DONE\",\"error\":\"API error (attempt 1): request failed: Post \\\"https://example.invalid/v1\\\": EOF\"}\n"
        );
        fs::write(&p_one, single_content).unwrap();
        let cfg = HyperConfig::default();
        let pulse_one = inspect_transcript_tail(&p_one, &cfg).unwrap();
        assert_eq!(pulse_one.recent_resets, 1);
        assert_eq!(pulse_one.consecutive_error_streak, 1);
        assert_eq!(pulse_one.health, HealthLevel::Green);
        assert_eq!(pulse_one.health_reason, "healthy");

        // Multi-checkpoint subagent with 6 consecutive EOF/TLS errors escalates to Red error_loop.
        let p_loop = dir.path().join("crash_loop.jsonl");
        let loop_content = concat!(
            "{\"step_index\":86,\"type\":\"CHECKPOINT\",\"content\":\"# Browser Session Checkpoint\"}\n",
            "{\"step_index\":131,\"type\":\"CHECKPOINT\",\"content\":\"# Browser Session Checkpoint\"}\n",
            "{\"step_index\":153,\"type\":\"PLANNER_RESPONSE\",\"model\":\"gemini-4-hydrogen-tiered-high\",\"input_tokens\":5060,\"cache_read_tokens\":71231,\"output_tokens\":300,\"created_at\":\"2026-11-23T03:55:00Z\"}\n",
            "{\"step_index\":154,\"type\":\"ERROR_MESSAGE\",\"status\":\"DONE\",\"error\":\"API error (attempt 1): request failed: Post \\\"https://example.invalid/v1\\\": EOF\"}\n",
            "{\"step_index\":155,\"type\":\"ERROR_MESSAGE\",\"status\":\"DONE\",\"error\":\"API error (attempt 2): request failed: Post \\\"https://example.invalid/v1\\\": net/http: TLS handshake timeout\"}\n",
            "{\"step_index\":156,\"type\":\"ERROR_MESSAGE\",\"status\":\"DONE\",\"error\":\"API error (attempt 3): request failed: Post \\\"https://example.invalid/v1\\\": EOF\"}\n",
            "{\"step_index\":157,\"type\":\"ERROR_MESSAGE\",\"status\":\"DONE\",\"error\":\"API error (attempt 4): request failed: Post \\\"https://example.invalid/v1\\\": EOF\"}\n",
            "{\"step_index\":158,\"type\":\"ERROR_MESSAGE\",\"status\":\"DONE\",\"error\":\"API error (attempt 5): request failed: Post \\\"https://example.invalid/v1\\\": EOF\"}\n",
            "{\"step_index\":159,\"type\":\"ERROR_MESSAGE\",\"status\":\"DONE\",\"error\":\"API error (attempt 6): request failed: Post \\\"https://example.invalid/v1\\\": EOF\"}\n"
        );
        fs::write(&p_loop, loop_content).unwrap();
        let pulse_loop = inspect_transcript_tail(&p_loop, &cfg).unwrap();
        assert_eq!(pulse_loop.checkpoint_count, 2);
        assert_eq!(pulse_loop.recent_resets, 5);
        assert_eq!(pulse_loop.recent_timeouts, 1);
        assert_eq!(pulse_loop.consecutive_error_streak, 6);
        assert_eq!(pulse_loop.health, HealthLevel::Red);
        assert_eq!(pulse_loop.health_reason, "error_loop");
        assert_eq!(
            pulse_loop.repeat_error_signature.as_deref(),
            Some("API error (attempt 6): EOF")
        );
    }
}
