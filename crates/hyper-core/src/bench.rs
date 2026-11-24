use crate::json_util::unquote_json_string;
use crate::transcript::{classify_transport_error, parse_iso8601_epoch_ms, TransportErrorKind};
use crate::usage::{model_ttl_family, normalize_model_key, TokenBucket, UNATTRIBUTED_MODEL};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub use crate::sessions::{
    discover_primary_transcript, discover_session_summaries, discover_transcripts,
    sanitize_session_title, SessionSummary,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnWaterfall {
    pub turn_index: usize,
    pub step_index: u64,
    pub started_ms: i64,
    pub total_turn_ms: u64,
    /// Estimated TTFT + model inference before tool dispatch (excluding stream retry backoff).
    pub llm_inference_ms: u64,
    /// Tool execution span (from planner response -> last tool completion before next planner).
    pub tool_exec_ms: u64,
    /// Upstream transport/stream errors (`Timeout` / `Reset`) preceding this planner response.
    #[serde(default)]
    pub transport_errors: u32,
    /// Wall time lost to upstream stream resets/timeouts before the winning planner response.
    #[serde(default)]
    pub retry_backoff_ms: u64,
    /// Tool call count in this turn.
    pub tool_calls: usize,
    pub used_hyper_tools: bool,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub tool_names: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextCategoryShare {
    pub key: String,
    pub bytes: u64,
    pub share_pct: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextBreakdown {
    pub file_read_bytes: u64,
    pub command_output_bytes: u64,
    pub hyper_tool_bytes: u64,
    pub other_tool_bytes: u64,
    pub planner_thinking_bytes: u64,
    pub planner_text_bytes: u64,
    pub user_system_bytes: u64,
    pub media_items: u32,
    pub total_bytes: u64,
    pub categories: Vec<ContextCategoryShare>,
}

impl ContextBreakdown {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn finalize(&mut self) {
        self.total_bytes = self
            .file_read_bytes
            .saturating_add(self.command_output_bytes)
            .saturating_add(self.hyper_tool_bytes)
            .saturating_add(self.other_tool_bytes)
            .saturating_add(self.planner_thinking_bytes)
            .saturating_add(self.planner_text_bytes)
            .saturating_add(self.user_system_bytes);
        let denom = self.total_bytes.max(1) as f64;
        let items = [
            ("file_reads", self.file_read_bytes),
            ("command_outputs", self.command_output_bytes),
            ("hyper_tools", self.hyper_tool_bytes),
            ("other_tools", self.other_tool_bytes),
            ("thinking_text", self.planner_thinking_bytes.saturating_add(self.planner_text_bytes)),
            ("user_system", self.user_system_bytes),
        ];
        self.categories = items
            .into_iter()
            .filter(|(_, b)| *b > 0)
            .map(|(k, b)| ContextCategoryShare {
                key: k.to_string(),
                bytes: b,
                share_pct: ((b as f64 / denom) * 1000.0).round() / 10.0,
            })
            .collect();
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelSwitchEvent {
    pub step_index: u64,
    pub from_model: String,
    pub to_model: String,
    pub from_raw_model: String,
    pub to_raw_model: String,
    pub same_family: bool,
    pub prev_cache_hit_ratio: f64,
    pub post_cache_hit_ratio: f64,
    pub cache_drop_ratio: f64,
    pub uncached_input_tokens: u64,
    pub prompt_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionSplitStats {
    pub primary_model: Option<String>,
    pub distinct_models: Vec<String>,
    pub model_switch_count: usize,
    pub cross_family_switch_count: usize,
    pub switch_uncached_input_tokens: u64,
    pub avg_switch_pre_hit_ratio: f64,
    pub avg_switch_post_hit_ratio: f64,
    pub avg_switch_cache_drop: f64,
    pub first_turn_input_tokens: u64,
    pub first_turn_cache_read_tokens: u64,
    pub steady_input_tokens: u64,
    pub steady_cache_read_tokens: u64,
    pub steady_turns: usize,
    pub delegation_tool_calls: usize,
    pub model_switches: Vec<ModelSwitchEvent>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionBenchmark {
    pub conversation_id: String,
    pub transcript_path: String,
    pub total_turns: usize,
    pub valid_turns: usize,
    pub dropped_idle_gap_turns: usize,
    pub total_wall_ms: u64,
    pub total_llm_ms: u64,
    pub total_tool_ms: u64,
    pub median_turn_ms: u64,
    pub p90_turn_ms: u64,
    pub median_llm_ms: u64,
    /// Strictly paired effective tokens/sec (ONLY sums output_tokens from turns whose valid duration was included!).
    pub effective_tps: f64,
    pub total_input_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_output_tokens: u64,
    pub cache_hit_ratio: f64,
    #[serde(default)]
    pub cache_break_turns: usize,
    pub hyper_tool_calls: usize,
    pub total_tool_calls: usize,
    #[serde(default)]
    pub hyper_adoption_ratio: f64,
    #[serde(default)]
    pub model_usage: BTreeMap<String, TokenBucket>,
    #[serde(default)]
    pub context_breakdown: ContextBreakdown,
    #[serde(default)]
    pub first_edit_context_tokens: Option<u64>,
    #[serde(default)]
    pub pre_edit_delta_tokens: Option<u64>,
    #[serde(default)]
    pub transport_error_count: usize,
    #[serde(default)]
    pub eof_reset_count: usize,
    #[serde(default)]
    pub total_retry_backoff_ms: u64,
    #[serde(default)]
    pub max_parallel_reads: usize,
    #[serde(default)]
    pub blanket_read_turns: usize,
    #[serde(default)]
    pub split_stats: SessionSplitStats,
    pub turns: Vec<TurnWaterfall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregateBenchmark {
    pub sessions_analyzed: usize,
    pub total_turns: usize,
    pub median_turn_ms: u64,
    pub p90_turn_ms: u64,
    pub median_llm_ms: u64,
    pub avg_cache_hit_ratio: f64,
    pub effective_tps: f64,
    pub hyper_adoption_ratio: f64,
    pub sessions: Vec<SessionBenchmark>,
}

/// Mathematically rigorous single-session waterfall analyzer matching Antigravity
/// `transcript.jsonl` timestamp semantics.
pub fn analyze_transcript(path: &Path) -> Result<SessionBenchmark> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let conv_id = path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    #[derive(Clone, Copy)]
    enum ToolCat {
        FileRead,
        CommandOutput,
        HyperTool,
        OtherTool,
    }

    struct RawStep {
        step_index: u64,
        kind: String,
        ts_ms: i64,
        completed_ms: Option<i64>,
        model: String,
        input_tokens: u64,
        cache_read_tokens: u64,
        output_tokens: u64,
        tool_names: Vec<String>,
        tool_cats: Vec<ToolCat>,
        hyper_tool_count: usize,
        content_bytes: u64,
        thinking_bytes: u64,
        tool_args_bytes: u64,
        media_count: u32,
        is_compact_marker: bool,
        has_edit_tool: bool,
        read_file_count: usize,
        delegation_tool_count: usize,
        transport_error: TransportErrorKind,
    }

    let mut steps: Vec<RawStep> = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let Some(ts_str) = v.get("created_at").and_then(Value::as_str) else {
            continue;
        };
        let Some(ts_ms) = parse_iso8601_epoch_ms(ts_str) else {
            continue;
        };
        let kind = v
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let transport_error = if kind == "ERROR_MESSAGE" {
            classify_error_step_value(&v)
        } else {
            TransportErrorKind::None
        };
        let step_index = v.get("step_index").and_then(Value::as_u64).unwrap_or(0);
        let model = normalize_model_key(
            v.get("model")
                .or_else(|| v.get("model_name"))
                .and_then(Value::as_str),
        );
        let input_tokens = v.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        let cache_read_tokens = v
            .get("cache_read_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output_tokens = v.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);

        let content_str = v.get("content").and_then(Value::as_str).unwrap_or("");
        let content_bytes = content_str.len() as u64;
        let thinking_bytes = v
            .get("thinking")
            .and_then(Value::as_str)
            .map(|s| s.len() as u64)
            .unwrap_or(0);
        let media_count = v
            .get("media")
            .and_then(Value::as_array)
            .map(|a| a.len() as u32)
            .unwrap_or(0);
        let is_compact_marker = kind == "USER_INPUT"
            && (content_str.contains("<CONTEXT_SUMMARY>") || content_str.trim() == "/compact");

        // Extract `Completed At: <RFC3339>` from Antigravity tool step headers when present.
        let completed_ms = extract_completed_at_ms(content_str);

        let mut tool_names = Vec::new();
        let mut tool_cats = Vec::new();
        let mut hyper_tool_count = 0usize;
        let mut tool_args_bytes = 0u64;
        let mut has_edit_tool = false;
        let mut read_file_count = 0usize;
        let mut delegation_tool_count = 0usize;
        if let Some(arr) = v.get("tool_calls").and_then(Value::as_array) {
            for tc in arr {
                let raw_name = tc
                    .get("name")
                    .or_else(|| tc.get("function").and_then(|f| f.get("name")))
                    .and_then(Value::as_str);
                if let Some(name) = raw_name {
                    // Inspect Antigravity lazy MCP calls (`call_mcp_tool` with ServerName/ToolName).
                    // Antigravity stores `args` values as JSON-encoded strings (e.g. `"\"hyper_pack\""`).
                    let args = tc
                        .get("arguments")
                        .or_else(|| tc.get("args"));
                    if let Some(a) = args {
                        tool_args_bytes = tool_args_bytes
                            .saturating_add(serde_json::to_string(a).map(|s| s.len() as u64).unwrap_or(0));
                    }
                    let server_name = args
                        .and_then(|a| a.get("ServerName").or_else(|| a.get("serverName")))
                        .and_then(Value::as_str)
                        .map(unquote_json_string)
                        .unwrap_or_default();
                    let lazy_tool = args
                        .and_then(|a| a.get("ToolName").or_else(|| a.get("toolName")))
                        .and_then(Value::as_str)
                        .map(unquote_json_string)
                        .unwrap_or_default();

                    let is_hyper = name.contains("hyper_")
                        || name.contains("agy-hyper")
                        || server_name.contains("agy-hyper")
                        || lazy_tool.starts_with("hyper_");
                    if is_hyper {
                        hyper_tool_count += 1;
                    }
                    let eff_name = if name == "call_mcp_tool" && !lazy_tool.is_empty() {
                        lazy_tool.clone()
                    } else {
                        name.to_string()
                    };
                    if eff_name == "read_file" {
                        read_file_count += 1;
                    }
                    if matches!(
                        eff_name.as_str(),
                        "edit_file" | "write_file" | "replace_file_content" | "multi_replace_file_content" | "hyper_edit"
                    ) {
                        has_edit_tool = true;
                    }
                    if matches!(
                        eff_name.as_str(),
                        "invoke_subagent" | "swarm" | "run_workflow" | "delegate"
                    ) {
                        delegation_tool_count += 1;
                    }
                    let cat = if is_hyper {
                        ToolCat::HyperTool
                    } else if eff_name == "read_file" {
                        ToolCat::FileRead
                    } else if eff_name == "run_command" {
                        ToolCat::CommandOutput
                    } else {
                        ToolCat::OtherTool
                    };
                    tool_cats.push(cat);
                    tool_names.push(eff_name);
                }
            }
        }
        steps.push(RawStep {
            step_index,
            kind,
            ts_ms,
            completed_ms,
            model,
            input_tokens,
            cache_read_tokens,
            output_tokens,
            tool_names,
            tool_cats,
            hyper_tool_count,
            content_bytes,
            thinking_bytes,
            tool_args_bytes,
            media_count,
            is_compact_marker,
            has_edit_tool,
            read_file_count,
            delegation_tool_count,
            transport_error,
        });
    }

    let mut turns: Vec<TurnWaterfall> = Vec::new();
    let mut dropped_idle = 0usize;
    let mut paired_output_tokens = 0u64;
    let mut paired_llm_ms = 0u64;
    let mut total_input = 0u64;
    let mut total_cache = 0u64;
    let mut total_output = 0u64;
    let mut hyper_tool_calls = 0usize;
    let mut total_tool_calls = 0usize;
    let mut cache_break_turns = 0usize;
    let mut prev_planner_ms: Option<i64> = None;
    let mut prev_planner_total: u64 = 0;
    let mut model_usage: BTreeMap<String, TokenBucket> = BTreeMap::new();
    let mut context_breakdown = ContextBreakdown::default();
    let mut first_edit_context_tokens: Option<u64> = None;
    let mut pre_edit_delta_tokens: Option<u64> = None;
    let mut session_first_turn_tokens: Option<u64> = None;
    let mut request_baseline_tokens: Option<u64> = None;
    let mut transport_error_count = 0usize;
    let mut eof_reset_count = 0usize;
    let mut orphan_retry_backoff_ms = 0u64;
    let mut turn_trigger_ms: Option<i64> = None;
    let mut pending_turn_errors = 0u32;
    let mut pending_first_err_ms: Option<i64> = None;
    let mut pending_last_err_ms: Option<i64> = None;
    let mut max_parallel_reads = 0usize;
    let mut blanket_read_turns = 0usize;
    let mut split_stats = SessionSplitStats::default();
    let mut prev_model: Option<String> = None;
    let mut prev_hit_ratio: Option<f64> = None;
    let mut first_turn_recorded = false;

    const MAX_VALID_SPAN_MS: i64 = 180_000;
    const MAX_RETRY_STORM_SPAN_MS: i64 = 600_000;

    let mut i = 0usize;
    while i < steps.len() {
        if steps[i].kind != "PLANNER_RESPONSE" {
            if steps[i].is_compact_marker {
                context_breakdown.reset();
            }
            if steps[i].kind == "USER_INPUT" {
                request_baseline_tokens = None;
                if pending_turn_errors > 0 {
                    if let Some(last_err) = pending_last_err_ms.take() {
                        let start = turn_trigger_ms.or(pending_first_err_ms).unwrap_or(last_err);
                        let span = last_err.saturating_sub(start);
                        if (0..=MAX_RETRY_STORM_SPAN_MS).contains(&span) {
                            orphan_retry_backoff_ms =
                                orphan_retry_backoff_ms.saturating_add(span as u64);
                        }
                    }
                    pending_turn_errors = 0;
                    pending_first_err_ms = None;
                }
                turn_trigger_ms = Some(steps[i].ts_ms);
                context_breakdown.user_system_bytes = context_breakdown
                    .user_system_bytes
                    .saturating_add(steps[i].content_bytes);
            } else if steps[i].kind == "SYSTEM_MESSAGE" {
                if pending_turn_errors == 0 {
                    turn_trigger_ms = Some(steps[i].ts_ms);
                }
                context_breakdown.user_system_bytes = context_breakdown
                    .user_system_bytes
                    .saturating_add(steps[i].content_bytes);
            } else if steps[i].kind == "ERROR_MESSAGE" {
                match steps[i].transport_error {
                    TransportErrorKind::Reset => {
                        transport_error_count += 1;
                        eof_reset_count += 1;
                        pending_turn_errors = pending_turn_errors.saturating_add(1);
                        pending_first_err_ms.get_or_insert(steps[i].ts_ms);
                        pending_last_err_ms = Some(steps[i].ts_ms);
                    }
                    TransportErrorKind::Timeout => {
                        transport_error_count += 1;
                        pending_turn_errors = pending_turn_errors.saturating_add(1);
                        pending_first_err_ms.get_or_insert(steps[i].ts_ms);
                        pending_last_err_ms = Some(steps[i].ts_ms);
                    }
                    TransportErrorKind::UserCanceled | TransportErrorKind::None => {}
                }
            }
            context_breakdown.media_items = context_breakdown
                .media_items
                .saturating_add(steps[i].media_count);
            i += 1;
            continue;
        }
        let planner = &steps[i];
        let turn_transport_errors = pending_turn_errors;
        let turn_first_err_ms = pending_first_err_ms.take();
        let turn_last_err_ms = pending_last_err_ms.take();
        pending_turn_errors = 0;
        let trigger_before_planner_ms = turn_trigger_ms;

        let cur_total = planner.input_tokens + planner.cache_read_tokens;
        if prev_planner_total >= 80_000 && cur_total > 0 && cur_total * 100 <= prev_planner_total * 55 {
            context_breakdown.reset();
            request_baseline_tokens = Some(cur_total);
        }
        if cur_total > 0 {
            session_first_turn_tokens.get_or_insert(cur_total);
            request_baseline_tokens.get_or_insert(cur_total);
        }
        context_breakdown.planner_text_bytes = context_breakdown
            .planner_text_bytes
            .saturating_add(planner.content_bytes);
        context_breakdown.planner_thinking_bytes = context_breakdown
            .planner_thinking_bytes
            .saturating_add(planner.thinking_bytes);
        context_breakdown.other_tool_bytes = context_breakdown
            .other_tool_bytes
            .saturating_add(planner.tool_args_bytes);
        context_breakdown.media_items = context_breakdown
            .media_items
            .saturating_add(planner.media_count);

        if planner.read_file_count > max_parallel_reads {
            max_parallel_reads = planner.read_file_count;
        }
        if planner.read_file_count >= 4 {
            blanket_read_turns += 1;
        }
        if first_edit_context_tokens.is_none() && planner.has_edit_tool && cur_total > 0 {
            first_edit_context_tokens = Some(cur_total);
            let baseline = request_baseline_tokens
                .or(session_first_turn_tokens)
                .unwrap_or(cur_total);
            pre_edit_delta_tokens = Some(cur_total.saturating_sub(baseline));
        }

        if let Some(prev_ms) = prev_planner_ms {
            let gap_ms = planner.ts_ms.saturating_sub(prev_ms);
            if prev_planner_total >= 50_000
                && cur_total >= 50_000
                && gap_ms <= 120_000
                && (planner.cache_read_tokens as f64 / cur_total as f64) < 0.15
            {
                cache_break_turns += 1;
            }
        }
        if cur_total > 0 {
            let cur_hit = planner.cache_read_tokens as f64 / cur_total as f64;
            if !first_turn_recorded {
                split_stats.first_turn_input_tokens = planner.input_tokens;
                split_stats.first_turn_cache_read_tokens = planner.cache_read_tokens;
            }
            let mut is_switch_turn = false;
            if planner.model != UNATTRIBUTED_MODEL {
                if !split_stats
                    .distinct_models
                    .iter()
                    .any(|m| m.eq_ignore_ascii_case(&planner.model))
                {
                    split_stats.distinct_models.push(planner.model.clone());
                }
                if let Some(ref pm) = prev_model {
                    if !pm.eq_ignore_ascii_case(&planner.model) {
                        is_switch_turn = true;
                        let from_fam = model_ttl_family(Some(pm));
                        let to_fam = model_ttl_family(Some(&planner.model));
                        let same_family = from_fam == to_fam;
                        let prev_hit = prev_hit_ratio.unwrap_or(0.0);
                        let drop = (prev_hit - cur_hit).max(0.0);
                        split_stats.model_switch_count += 1;
                        if !same_family {
                            split_stats.cross_family_switch_count += 1;
                        }
                        split_stats.switch_uncached_input_tokens = split_stats
                            .switch_uncached_input_tokens
                            .saturating_add(planner.input_tokens);
                        split_stats.model_switches.push(ModelSwitchEvent {
                            step_index: planner.step_index,
                            from_model: from_fam,
                            to_model: to_fam,
                            from_raw_model: pm.clone(),
                            to_raw_model: planner.model.clone(),
                            same_family,
                            prev_cache_hit_ratio: round4(prev_hit),
                            post_cache_hit_ratio: round4(cur_hit),
                            cache_drop_ratio: round4(drop),
                            uncached_input_tokens: planner.input_tokens,
                            prompt_tokens: cur_total,
                        });
                    }
                }
                prev_model = Some(planner.model.clone());
            }
            if first_turn_recorded && !is_switch_turn {
                split_stats.steady_input_tokens = split_stats
                    .steady_input_tokens
                    .saturating_add(planner.input_tokens);
                split_stats.steady_cache_read_tokens = split_stats
                    .steady_cache_read_tokens
                    .saturating_add(planner.cache_read_tokens);
                split_stats.steady_turns += 1;
            }
            first_turn_recorded = true;
            prev_hit_ratio = Some(cur_hit);
            prev_planner_ms = Some(planner.ts_ms);
            prev_planner_total = cur_total;
        }

        split_stats.delegation_tool_calls = split_stats
            .delegation_tool_calls
            .saturating_add(planner.delegation_tool_count);

        total_input += planner.input_tokens;
        total_cache += planner.cache_read_tokens;
        total_output += planner.output_tokens;
        let mb = model_usage.entry(planner.model.clone()).or_default();
        mb.turns = mb.turns.saturating_add(1);
        mb.input_tokens = mb.input_tokens.saturating_add(planner.input_tokens);
        mb.cache_read_tokens = mb.cache_read_tokens.saturating_add(planner.cache_read_tokens);
        mb.output_tokens = mb.output_tokens.saturating_add(planner.output_tokens);
        total_tool_calls += planner.tool_names.len();
        hyper_tool_calls += planner.hyper_tool_count;

        // Collect all tool/hook steps strictly belonging to THIS planner turn.
        let mut j = i + 1;
        let mut first_tool_start_ms: Option<i64> = None;
        let mut last_tool_end_ms: Option<i64> = None;
        let mut tool_resp_idx = 0usize;

        while j < steps.len()
            && steps[j].kind != "PLANNER_RESPONSE"
            && steps[j].kind != "USER_INPUT"
            && steps[j].kind != "ERROR_MESSAGE"
        {
            let st = &steps[j];
            let is_tool_step = st.kind == "GENERIC"
                || st.kind == "TOOL_RESPONSE"
                || st.kind.contains("TOOL");
            context_breakdown.media_items = context_breakdown
                .media_items
                .saturating_add(st.media_count);
            if is_tool_step {
                let cat = planner
                    .tool_cats
                    .get(tool_resp_idx)
                    .copied()
                    .unwrap_or(ToolCat::OtherTool);
                tool_resp_idx += 1;
                match cat {
                    ToolCat::FileRead => {
                        context_breakdown.file_read_bytes = context_breakdown
                            .file_read_bytes
                            .saturating_add(st.content_bytes);
                    }
                    ToolCat::CommandOutput => {
                        context_breakdown.command_output_bytes = context_breakdown
                            .command_output_bytes
                            .saturating_add(st.content_bytes);
                    }
                    ToolCat::HyperTool => {
                        context_breakdown.hyper_tool_bytes = context_breakdown
                            .hyper_tool_bytes
                            .saturating_add(st.content_bytes);
                    }
                    ToolCat::OtherTool => {
                        context_breakdown.other_tool_bytes = context_breakdown
                            .other_tool_bytes
                            .saturating_add(st.content_bytes);
                    }
                }
                if first_tool_start_ms.is_none() {
                    first_tool_start_ms = Some(st.ts_ms);
                }
            } else if st.kind == "SYSTEM_MESSAGE" {
                context_breakdown.user_system_bytes = context_breakdown
                    .user_system_bytes
                    .saturating_add(st.content_bytes);
            }
            let step_end = st.completed_ms.unwrap_or(st.ts_ms).max(st.ts_ms);
            last_tool_end_ms = Some(last_tool_end_ms.map_or(step_end, |prev| prev.max(step_end)));
            j += 1;
        }

        // If the very next step is another PLANNER_RESPONSE (autonomous continuation), its
        // `created_at` also marks exact tool+hook completion time.
        if j < steps.len() && steps[j].kind == "PLANNER_RESPONSE" {
            if let Some(prev_end) = last_tool_end_ms {
                let next_ts = steps[j].ts_ms;
                if next_ts >= prev_end && (next_ts - prev_end) <= 15_000 {
                    last_tool_end_ms = Some(next_ts);
                }
            }
        }

        let turn_start_ms = if turn_transport_errors > 0 {
            trigger_before_planner_ms
                .or(turn_first_err_ms)
                .unwrap_or(planner.ts_ms)
                .min(planner.ts_ms)
        } else {
            planner.ts_ms
        };

        // Only measure turns where we have an exact LLM-end boundary (tool dispatch, or a retry-storm
        // recovery boundary when upstream transport errors preceded the planner response).
        let llm_end_ms = if let Some(first_tool) = first_tool_start_ms {
            first_tool
        } else if turn_transport_errors > 0 && planner.ts_ms > turn_start_ms {
            planner.ts_ms
        } else {
            turn_trigger_ms = Some(planner.ts_ms);
            i = j;
            continue;
        };

        let raw_llm_delta = (llm_end_ms - turn_start_ms).max(0);
        let tools_done_ms = last_tool_end_ms.unwrap_or(llm_end_ms).max(llm_end_ms);
        let tool_delta = (tools_done_ms - llm_end_ms).max(0);
        turn_trigger_ms = Some(tools_done_ms);

        let max_valid_span = if turn_transport_errors > 0 {
            MAX_RETRY_STORM_SPAN_MS
        } else {
            MAX_VALID_SPAN_MS
        };

        if raw_llm_delta <= max_valid_span && tool_delta <= max_valid_span {
            let raw_llm_ms = raw_llm_delta as u64;
            let tool_ms = tool_delta as u64;
            let total_turn_ms = raw_llm_ms + tool_ms;
            let retry_backoff_ms = if turn_transport_errors > 0 {
                turn_last_err_ms
                    .map(|last_err| last_err.saturating_sub(turn_start_ms).max(0) as u64)
                    .unwrap_or(0)
                    .min(raw_llm_ms)
                    .min(total_turn_ms)
            } else {
                0
            };
            let llm_ms = raw_llm_ms.saturating_sub(retry_backoff_ms);
            if llm_ms > 0 {
                paired_llm_ms += llm_ms;
                paired_output_tokens += planner.output_tokens;
            }
            turns.push(TurnWaterfall {
                turn_index: turns.len() + 1,
                step_index: planner.step_index,
                started_ms: turn_start_ms,
                total_turn_ms,
                llm_inference_ms: llm_ms,
                tool_exec_ms: tool_ms,
                transport_errors: turn_transport_errors,
                retry_backoff_ms,
                tool_calls: planner.tool_names.len(),
                used_hyper_tools: planner.hyper_tool_count > 0,
                input_tokens: planner.input_tokens,
                cache_read_tokens: planner.cache_read_tokens,
                output_tokens: planner.output_tokens,
                tool_names: planner.tool_names.clone(),
            });
        } else {
            dropped_idle += 1;
        }
        i = j;
    }

    if pending_turn_errors > 0 {
        if let Some(last_err) = pending_last_err_ms {
            let start = turn_trigger_ms.or(pending_first_err_ms).unwrap_or(last_err);
            let span = last_err.saturating_sub(start);
            if (0..=MAX_RETRY_STORM_SPAN_MS).contains(&span) {
                orphan_retry_backoff_ms = orphan_retry_backoff_ms.saturating_add(span as u64);
            }
        }
    }

    let mut turn_durs: Vec<u64> = turns.iter().map(|t| t.total_turn_ms).collect();
    let mut llm_durs: Vec<u64> = turns.iter().map(|t| t.llm_inference_ms).collect();
    let total_llm_ms: u64 = turns.iter().map(|t| t.llm_inference_ms).sum();
    let total_tool_ms: u64 = turns.iter().map(|t| t.tool_exec_ms).sum();
    let total_retry_backoff_ms: u64 = turns
        .iter()
        .map(|t| t.retry_backoff_ms)
        .sum::<u64>()
        .saturating_add(orphan_retry_backoff_ms);
    let total_wall_ms: u64 = turns.iter().map(|t| t.total_turn_ms).sum();

    let effective_tps = if paired_llm_ms > 0 {
        paired_output_tokens as f64 / (paired_llm_ms as f64 / 1000.0)
    } else {
        0.0
    };
    let denom = total_input + total_cache;
    let cache_hit_ratio = if denom > 0 {
        total_cache as f64 / denom as f64
    } else {
        0.0
    };

    let hyper_adoption_ratio = if total_tool_calls > 0 {
        hyper_tool_calls as f64 / total_tool_calls as f64
    } else {
        0.0
    };

    context_breakdown.finalize();

    split_stats.primary_model = model_usage
        .iter()
        .filter(|(m, _)| m.as_str() != UNATTRIBUTED_MODEL)
        .max_by_key(|(_, b)| (b.turns, b.total_tokens()))
        .map(|(m, _)| m.clone());
    if !split_stats.model_switches.is_empty() {
        let n = split_stats.model_switches.len() as f64;
        let pre_sum: f64 = split_stats
            .model_switches
            .iter()
            .map(|e| e.prev_cache_hit_ratio)
            .sum();
        let post_sum: f64 = split_stats
            .model_switches
            .iter()
            .map(|e| e.post_cache_hit_ratio)
            .sum();
        let drop_sum: f64 = split_stats
            .model_switches
            .iter()
            .map(|e| e.cache_drop_ratio)
            .sum();
        split_stats.avg_switch_pre_hit_ratio = round4(pre_sum / n);
        split_stats.avg_switch_post_hit_ratio = round4(post_sum / n);
        split_stats.avg_switch_cache_drop = round4(drop_sum / n);
    }

    Ok(SessionBenchmark {
        conversation_id: conv_id,
        transcript_path: path.display().to_string(),
        total_turns: turns.len() + dropped_idle,
        valid_turns: turns.len(),
        dropped_idle_gap_turns: dropped_idle,
        total_wall_ms,
        total_llm_ms,
        total_tool_ms,
        median_turn_ms: percentile(&mut turn_durs, 0.50),
        p90_turn_ms: percentile(&mut turn_durs, 0.90),
        median_llm_ms: percentile(&mut llm_durs, 0.50),
        effective_tps,
        total_input_tokens: total_input,
        total_cache_read_tokens: total_cache,
        total_output_tokens: total_output,
        cache_hit_ratio,
        cache_break_turns,
        hyper_tool_calls,
        total_tool_calls,
        hyper_adoption_ratio,
        model_usage,
        context_breakdown,
        first_edit_context_tokens,
        pre_edit_delta_tokens,
        transport_error_count,
        eof_reset_count,
        total_retry_backoff_ms,
        max_parallel_reads,
        blanket_read_turns,
        split_stats,
        turns,
    })
}

pub fn aggregate_sessions(paths: &[PathBuf]) -> AggregateBenchmark {
    let mut sessions = Vec::new();
    let mut all_turn_ms = Vec::new();
    let mut all_llm_ms = Vec::new();
    let mut total_in = 0u64;
    let mut total_cache = 0u64;
    let mut paired_out = 0u64;
    let mut paired_llm = 0u64;
    let mut hyper_calls = 0usize;
    let mut all_calls = 0usize;

    for p in paths {
        if let Ok(s) = analyze_transcript(p) {
            if s.valid_turns == 0 {
                continue;
            }
            for t in &s.turns {
                all_turn_ms.push(t.total_turn_ms);
                all_llm_ms.push(t.llm_inference_ms);
                paired_out += t.output_tokens;
                paired_llm += t.llm_inference_ms;
            }
            total_in += s.total_input_tokens;
            total_cache += s.total_cache_read_tokens;
            hyper_calls += s.hyper_tool_calls;
            all_calls += s.total_tool_calls;
            sessions.push(s);
        }
    }

    let total_turns = all_turn_ms.len();
    let denom = total_in + total_cache;
    let avg_cache_hit_ratio = if denom > 0 {
        total_cache as f64 / denom as f64
    } else {
        0.0
    };
    let effective_tps = if paired_llm > 0 {
        paired_out as f64 / (paired_llm as f64 / 1000.0)
    } else {
        0.0
    };
    let hyper_adoption_ratio = if all_calls > 0 {
        hyper_calls as f64 / all_calls as f64
    } else {
        0.0
    };

    AggregateBenchmark {
        sessions_analyzed: sessions.len(),
        total_turns,
        median_turn_ms: percentile(&mut all_turn_ms, 0.50),
        p90_turn_ms: percentile(&mut all_turn_ms, 0.90),
        median_llm_ms: percentile(&mut all_llm_ms, 0.50),
        avg_cache_hit_ratio,
        effective_tps,
        hyper_adoption_ratio,
        sessions,
    }
}

/// Generate a crisp, dark-mode shareable SVG Waterfall Flamegraph for terminal/README export.
pub fn render_waterfall_svg(session: &SessionBenchmark) -> String {
    let shown: Vec<&TurnWaterfall> = session.turns.iter().rev().take(16).rev().collect();
    let max_ms = shown
        .iter()
        .map(|t| t.total_turn_ms)
        .max()
        .unwrap_or(1000)
        .max(1000);
    let row_h = 24usize;
    let height = 96 + shown.len() * row_h + 24;
    let bar_w = 560.0f64;

    let mut rows = String::new();
    for (idx, t) in shown.iter().enumerate() {
        let y = 84 + idx * row_h;
        let llm_w = ((t.llm_inference_ms as f64 / max_ms as f64) * bar_w).max(2.0);
        let tool_w = ((t.tool_exec_ms as f64 / max_ms as f64) * bar_w).max(0.0);
        let tool_color = if t.used_hyper_tools { "#38BDF8" } else { "#F59E0B" };
        let label = format!("T{:02}", t.turn_index);
        let dur_txt = format!(
            "{:.2}s ({:.1}s llm / {:.2}s tool)",
            t.total_turn_ms as f64 / 1000.0,
            t.llm_inference_ms as f64 / 1000.0,
            t.tool_exec_ms as f64 / 1000.0
        );
        rows.push_str(&format!(
            "  <text x=\"20\" y=\"{}\" fill=\"#94A3B8\" font-size=\"11\" font-family=\"ui-monospace,Menlo,monospace\">{}</text>\n\
             \x20 <rect x=\"58\" y=\"{}\" width=\"{:.1}\" height=\"14\" rx=\"3\" fill=\"#6366F1\"/>\n\
             \x20 <rect x=\"{:.1}\" y=\"{}\" width=\"{:.1}\" height=\"14\" rx=\"3\" fill=\"{}\"/>\n\
             \x20 <text x=\"630\" y=\"{}\" fill=\"#CBD5E1\" font-size=\"11\" font-family=\"ui-monospace,Menlo,monospace\">{}</text>\n",
            y + 11,
            label,
            y,
            llm_w,
            58.0 + llm_w + 2.0,
            y,
            tool_w,
            tool_color,
            y + 11,
            dur_txt
        ));
    }

    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"840\" height=\"{}\" viewBox=\"0 0 840 {}\">\n\
         \x20 <rect width=\"840\" height=\"{}\" rx=\"12\" fill=\"#0B0F17\" stroke=\"#1E293B\" stroke-width=\"1.5\"/>\n\
         \x20 <text x=\"20\" y=\"32\" fill=\"#F8FAFC\" font-size=\"15\" font-weight=\"700\" font-family=\"system-ui,-apple-system,sans-serif\">AGY HYPER · TURN WATERFALL TELEMETRY</text>\n\
         \x20 <text x=\"20\" y=\"52\" fill=\"#94A3B8\" font-size=\"11.5\" font-family=\"ui-monospace,Menlo,monospace\">session={} · turns={} · P50={:.2}s · P90={:.2}s · cache={:.1}% · TPS={:.1}</text>\n\
         \x20 <rect x=\"590\" y=\"22\" width=\"10\" height=\"10\" rx=\"2\" fill=\"#6366F1\"/><text x=\"605\" y=\"31\" fill=\"#CBD5E1\" font-size=\"11\" font-family=\"system-ui,sans-serif\">LLM Inference</text>\n\
         \x20 <rect x=\"695\" y=\"22\" width=\"10\" height=\"10\" rx=\"2\" fill=\"#38BDF8\"/><text x=\"710\" y=\"31\" fill=\"#CBD5E1\" font-size=\"11\" font-family=\"system-ui,sans-serif\">Hyper Tool</text>\n\
         \x20 <line x1=\"20\" y1=\"66\" x2=\"820\" y2=\"66\" stroke=\"#1E293B\" stroke-width=\"1\"/>\n\
         {}\
         </svg>\n",
        height,
        height,
        height,
        &session.conversation_id[..session.conversation_id.len().min(8)],
        session.valid_turns,
        session.median_turn_ms as f64 / 1000.0,
        session.p90_turn_ms as f64 / 1000.0,
        session.cache_hit_ratio * 100.0,
        session.effective_tps,
        rows
    )
}

fn classify_error_step_value(v: &Value) -> TransportErrorKind {
    let candidates = [
        v.get("content").and_then(Value::as_str),
        v.get("message").and_then(Value::as_str),
        v.get("error").and_then(Value::as_str),
        v.get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str),
    ];
    let mut saw_cancel = false;
    for cand in candidates.into_iter().flatten() {
        match classify_transport_error(cand) {
            TransportErrorKind::Reset => return TransportErrorKind::Reset,
            TransportErrorKind::Timeout => return TransportErrorKind::Timeout,
            TransportErrorKind::UserCanceled => saw_cancel = true,
            TransportErrorKind::None => {}
        }
    }
    if saw_cancel {
        TransportErrorKind::UserCanceled
    } else {
        TransportErrorKind::None
    }
}

fn extract_completed_at_ms(content: &str) -> Option<i64> {
    for line in content.lines().take(5) {
        if let Some(rest) = line.strip_prefix("Completed At:") {
            let trimmed = rest.trim();
            if let Some(ms) = parse_iso8601_epoch_ms(trimmed) {
                return Some(ms);
            }
        }
    }
    None
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

fn percentile(vals: &mut [u64], p: f64) -> u64 {
    if vals.is_empty() {
        return 0;
    }
    vals.sort_unstable();
    let idx = ((vals.len() as f64 - 1.0) * p).round() as usize;
    vals[idx.min(vals.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn bench_computes_paired_tps_and_disjoint_waterfall_spans() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("transcript.jsonl");
        // Matches real Antigravity transcript semantics:
        // PLANNER_RESPONSE.created_at = turn start (10:00:00)
        // GENERIC.created_at          = LLM finished streaming & invoked tool (10:00:05 -> llm=5s)
        // GENERIC.Completed At        = tool completed (10:00:06 -> tool=1s)
        let data = concat!(
            "{\"step_index\":1,\"type\":\"USER_INPUT\",\"created_at\":\"2026-11-20T10:00:00Z\"}\n",
            "{\"step_index\":2,\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-20T10:00:00Z\",\"input_tokens\":1000,\"cache_read_tokens\":9000,\"output_tokens\":250,\"tool_calls\":[{\"name\":\"call_mcp_tool\",\"arguments\":{\"ServerName\":\"agy-hyper_core\",\"ToolName\":\"hyper_pack\"}}]}\n",
            "{\"step_index\":3,\"type\":\"GENERIC\",\"created_at\":\"2026-11-20T10:00:05Z\",\"content\":\"Created At: 2026-11-20T10:00:05Z\\nCompleted At: 2026-11-20T10:00:06Z\\nOK\"}\n",
            // Idle stall of 400s before tool dispatch -> must be dropped from BOTH duration and paired output tokens!
            "{\"step_index\":4,\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-20T10:00:06Z\",\"input_tokens\":1000,\"cache_read_tokens\":9000,\"output_tokens\":9999,\"tool_calls\":[{\"name\":\"read_file\"}]}\n",
            "{\"step_index\":5,\"type\":\"GENERIC\",\"created_at\":\"2026-11-20T10:06:46Z\",\"content\":\"Created At: 2026-11-20T10:06:46Z\\nCompleted At: 2026-11-20T10:06:47Z\\nOK\"}\n"
        );
        fs::write(&p, data).unwrap();
        let b = analyze_transcript(&p).unwrap();
        assert_eq!(b.valid_turns, 1);
        assert_eq!(b.dropped_idle_gap_turns, 1);
        assert_eq!(b.turns[0].llm_inference_ms, 5_000);
        assert_eq!(b.turns[0].tool_exec_ms, 1_000);
        assert!(b.turns[0].used_hyper_tools);
        assert_eq!(b.hyper_tool_calls, 1);
        assert_eq!(b.total_tool_calls, 2);
        assert!((b.hyper_adoption_ratio - 0.50).abs() < 0.01);
        assert!((b.effective_tps - 50.0).abs() < 0.01);
        assert!((b.cache_hit_ratio - 0.90).abs() < 0.01);
        assert_eq!(b.cache_break_turns, 0);
    }

    #[test]
    fn detects_short_gap_cache_break_turns() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("transcript.jsonl");
        let data = concat!(
            "{\"step_index\":1,\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-20T10:00:00Z\",\"input_tokens\":5000,\"cache_read_tokens\":195000,\"output_tokens\":100,\"tool_calls\":[{\"name\":\"run_command\"}]}\n",
            "{\"step_index\":2,\"type\":\"GENERIC\",\"created_at\":\"2026-11-20T10:00:05Z\",\"content\":\"Created At: 2026-11-20T10:00:05Z\\nCompleted At: 2026-11-20T10:00:06Z\\nOK\"}\n",
            "{\"step_index\":3,\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-20T10:00:25Z\",\"input_tokens\":202000,\"cache_read_tokens\":0,\"output_tokens\":80,\"tool_calls\":[{\"name\":\"read_file\"}]}\n",
            "{\"step_index\":4,\"type\":\"GENERIC\",\"created_at\":\"2026-11-20T10:00:27Z\",\"content\":\"Created At: 2026-11-20T10:00:27Z\\nCompleted At: 2026-11-20T10:00:28Z\\nOK\"}\n"
        );
        fs::write(&p, data).unwrap();
        let b = analyze_transcript(&p).unwrap();
        assert_eq!(b.cache_break_turns, 1);
    }

    #[test]
    fn unquotes_antigravity_double_encoded_mcp_args_and_sanitizes_urls() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("transcript.jsonl");
        let data = concat!(
            "{\"step_index\":1,\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-20T10:00:00Z\",\"input_tokens\":500,\"cache_read_tokens\":4500,\"output_tokens\":100,\"tool_calls\":[{\"name\":\"call_mcp_tool\",\"args\":{\"ServerName\":\"\\\"agy-hyper_core\\\"\",\"ToolName\":\"\\\"hyper_pack\\\"\"}}]}\n",
            "{\"step_index\":2,\"type\":\"GENERIC\",\"created_at\":\"2026-11-20T10:00:02Z\",\"content\":\"Created At: 2026-11-20T10:00:02Z\\nCompleted At: 2026-11-20T10:00:03Z\\nOK\"}\n"
        );
        fs::write(&p, data).unwrap();
        let b = analyze_transcript(&p).unwrap();
        assert_eq!(b.hyper_tool_calls, 1);
        assert!((b.hyper_adoption_ratio - 1.0).abs() < 0.001);
        assert_eq!(b.turns[0].tool_names, vec!["hyper_pack"]);

        let clean = sanitize_session_title(
            "https://github.com/example/repo Refactor auth middleware /Users/alice/dev",
            "auth-svc",
            "12345678-aaaa",
        );
        assert_eq!(clean, "Refactor auth middleware");
        assert!(!clean.contains("https://"));
        assert!(!clean.contains("/Users/"));
    }

    #[test]
    fn pairs_generic_tool_outputs_into_context_breakdown_and_resets_on_compact() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("transcript.jsonl");
        let data = concat!(
            "{\"step_index\":1,\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-20T10:00:00Z\",\"input_tokens\":10000,\"cache_read_tokens\":90000,\"output_tokens\":200,\"tool_calls\":[{\"name\":\"read_file\"},{\"name\":\"read_file\"},{\"name\":\"read_file\"},{\"name\":\"read_file\"}]}\n",
            "{\"step_index\":2,\"type\":\"GENERIC\",\"created_at\":\"2026-11-20T10:00:01Z\",\"content\":\"pre-compact-huge-read-aaaaaaaaaa\"}\n",
            "{\"step_index\":3,\"type\":\"USER_INPUT\",\"created_at\":\"2026-11-20T10:00:02Z\",\"content\":\"/compact\"}\n",
            "{\"step_index\":4,\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-20T10:00:05Z\",\"input_tokens\":20000,\"cache_read_tokens\":0,\"output_tokens\":150,\"thinking\":\"plan\",\"tool_calls\":[{\"name\":\"read_file\"},{\"name\":\"run_command\"},{\"name\":\"edit_file\"}]}\n",
            "{\"step_index\":5,\"type\":\"GENERIC\",\"created_at\":\"2026-11-20T10:00:06Z\",\"content\":\"file-body-1234567890\"}\n",
            "{\"step_index\":6,\"type\":\"GENERIC\",\"created_at\":\"2026-11-20T10:00:07Z\",\"content\":\"cmd-output-12345678901234567890\"}\n"
        );
        fs::write(&p, data).unwrap();
        let b = analyze_transcript(&p).unwrap();
        assert_eq!(b.max_parallel_reads, 4);
        assert_eq!(b.blanket_read_turns, 1);
        assert_eq!(b.first_edit_context_tokens, Some(20_000));
        assert_eq!(b.context_breakdown.file_read_bytes, "file-body-1234567890".len() as u64);
        assert_eq!(
            b.context_breakdown.command_output_bytes,
            "cmd-output-12345678901234567890".len() as u64
        );
        assert!(!b.context_breakdown.categories.is_empty());
    }

    #[test]
    fn tracks_model_switch_cache_drop_and_delegation_calls() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("transcript.jsonl");
        let data = concat!(
            "{\"step_index\":1,\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-20T10:00:00Z\",\"model\":\"gemini-4-neon-xhigh\",\"input_tokens\":25000,\"cache_read_tokens\":0,\"output_tokens\":200,\"tool_calls\":[{\"name\":\"read_file\"}]}\n",
            "{\"step_index\":2,\"type\":\"GENERIC\",\"created_at\":\"2026-11-20T10:00:02Z\",\"content\":\"Created At: 2026-11-20T10:00:02Z\\nCompleted At: 2026-11-20T10:00:03Z\\nOK\"}\n",
            "{\"step_index\":3,\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-20T10:00:05Z\",\"model\":\"gemini-4-neon-xhigh\",\"input_tokens\":2000,\"cache_read_tokens\":98000,\"output_tokens\":150,\"tool_calls\":[{\"name\":\"invoke_subagent\"}]}\n",
            "{\"step_index\":4,\"type\":\"GENERIC\",\"created_at\":\"2026-11-20T10:00:07Z\",\"content\":\"Created At: 2026-11-20T10:00:07Z\\nCompleted At: 2026-11-20T10:00:08Z\\nOK\"}\n",
            "{\"step_index\":5,\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-20T10:00:12Z\",\"model\":\"gemini-4-carbon-medium\",\"input_tokens\":105000,\"cache_read_tokens\":0,\"output_tokens\":180,\"tool_calls\":[{\"name\":\"edit_file\"}]}\n",
            "{\"step_index\":6,\"type\":\"GENERIC\",\"created_at\":\"2026-11-20T10:00:14Z\",\"content\":\"Created At: 2026-11-20T10:00:14Z\\nCompleted At: 2026-11-20T10:00:15Z\\nOK\"}\n"
        );
        fs::write(&p, data).unwrap();
        let b = analyze_transcript(&p).unwrap();
        assert_eq!(b.split_stats.primary_model.as_deref(), Some("gemini-4-neon-xhigh"));
        assert_eq!(b.split_stats.distinct_models.len(), 2);
        assert_eq!(b.split_stats.model_switch_count, 1);
        assert_eq!(b.split_stats.cross_family_switch_count, 1);
        assert_eq!(b.split_stats.switch_uncached_input_tokens, 105_000);
        assert!((b.split_stats.avg_switch_pre_hit_ratio - 0.98).abs() < 1e-4);
        assert!((b.split_stats.avg_switch_post_hit_ratio - 0.0).abs() < 1e-4);
        assert!((b.split_stats.avg_switch_cache_drop - 0.98).abs() < 1e-4);
        assert_eq!(b.split_stats.first_turn_input_tokens, 25_000);
        assert_eq!(b.split_stats.first_turn_cache_read_tokens, 0);
        assert_eq!(b.split_stats.steady_turns, 1);
        assert_eq!(b.split_stats.steady_cache_read_tokens, 98_000);
        assert_eq!(b.split_stats.delegation_tool_calls, 1);
        assert_eq!(b.split_stats.model_switches.len(), 1);
        assert_eq!(b.split_stats.model_switches[0].from_model, "gemini-4-neon");
        assert_eq!(b.split_stats.model_switches[0].to_model, "gemini-4-carbon");
        assert!(!b.split_stats.model_switches[0].same_family);
    }

    #[test]
    fn deserializes_legacy_session_benchmark_without_split_stats() {
        let legacy = serde_json::json!({
            "conversation_id": "legacy-conv",
            "transcript_path": "/tmp/legacy.jsonl",
            "total_turns": 1,
            "valid_turns": 1,
            "dropped_idle_gap_turns": 0,
            "total_wall_ms": 1000,
            "total_llm_ms": 800,
            "total_tool_ms": 200,
            "median_turn_ms": 1000,
            "p90_turn_ms": 1000,
            "median_llm_ms": 800,
            "effective_tps": 50.0,
            "total_input_tokens": 1000,
            "total_cache_read_tokens": 9000,
            "total_output_tokens": 40,
            "cache_hit_ratio": 0.9,
            "hyper_tool_calls": 0,
            "total_tool_calls": 1,
            "turns": []
        });
        let parsed: SessionBenchmark = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed.split_stats.model_switch_count, 0);
        assert!(parsed.split_stats.model_switches.is_empty());
        assert_eq!(parsed.split_stats.primary_model, None);
        assert_eq!(parsed.transport_error_count, 0);
        assert_eq!(parsed.eof_reset_count, 0);
        assert_eq!(parsed.total_retry_backoff_ms, 0);
        assert_eq!(parsed.pre_edit_delta_tokens, None);
    }

    #[test]
    fn attributes_eof_retry_storm_waterfall_and_separates_pre_edit_delta_tokens() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("transcript.jsonl");
        // Multi-task session:
        // Task 1 starts at 140k tokens (heavy global skills/MCP baseline), chats up to 150k without editing.
        // Task 2 starts with USER_INPUT at 10:05:00Z, hits a 220s EOF + TLS timeout retry storm (>180s normal cap,
        // <=600s retry ceiling), recovers at 10:08:40Z (baseline=155k), then edits on the next turn at 162k (+7k delta).
        let data = concat!(
            "{\"step_index\":1,\"type\":\"USER_INPUT\",\"created_at\":\"2026-11-20T10:00:00Z\",\"content\":\"Explain architecture\"}\n",
            "{\"step_index\":2,\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-20T10:00:00Z\",\"input_tokens\":140000,\"cache_read_tokens\":0,\"output_tokens\":120,\"tool_calls\":[{\"name\":\"read_file\"}]}\n",
            "{\"step_index\":3,\"type\":\"GENERIC\",\"created_at\":\"2026-11-20T10:00:03Z\",\"content\":\"Created At: 2026-11-20T10:00:03Z\\nCompleted At: 2026-11-20T10:00:04Z\\nOK\"}\n",
            "{\"step_index\":4,\"type\":\"USER_INPUT\",\"created_at\":\"2026-11-20T10:05:00Z\",\"content\":\"Fix handler\"}\n",
            "{\"step_index\":5,\"type\":\"ERROR_MESSAGE\",\"created_at\":\"2026-11-20T10:06:10Z\",\"error\":\"Post \\\"https://api.example.invalid/v1/stream\\\": EOF\"}\n",
            "{\"step_index\":6,\"type\":\"ERROR_MESSAGE\",\"created_at\":\"2026-11-20T10:07:30Z\",\"message\":\"TLS handshake timeout\"}\n",
            "{\"step_index\":7,\"type\":\"ERROR_MESSAGE\",\"created_at\":\"2026-11-20T10:08:40Z\",\"content\":\"stream was interrupted\"}\n",
            "{\"step_index\":8,\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-20T10:08:42Z\",\"input_tokens\":5000,\"cache_read_tokens\":150000,\"output_tokens\":200,\"tool_calls\":[{\"name\":\"call_mcp_tool\",\"arguments\":{\"ServerName\":\"agy-hyper_core\",\"ToolName\":\"hyper_pack\"}}]}\n",
            "{\"step_index\":9,\"type\":\"GENERIC\",\"created_at\":\"2026-11-20T10:08:46Z\",\"content\":\"Created At: 2026-11-20T10:08:46Z\\nCompleted At: 2026-11-20T10:08:47Z\\nOK\"}\n",
            "{\"step_index\":10,\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-20T10:08:47Z\",\"input_tokens\":7000,\"cache_read_tokens\":155000,\"output_tokens\":150,\"tool_calls\":[{\"name\":\"hyper_edit\"}]}\n",
            "{\"step_index\":11,\"type\":\"GENERIC\",\"created_at\":\"2026-11-20T10:08:50Z\",\"content\":\"Created At: 2026-11-20T10:08:50Z\\nCompleted At: 2026-11-20T10:08:51Z\\nOK\"}\n"
        );
        fs::write(&p, data).unwrap();
        let b = analyze_transcript(&p).unwrap();

        assert_eq!(b.dropped_idle_gap_turns, 0, "220s retry-storm turn must be kept under 600s ceiling");
        assert_eq!(b.valid_turns, 3);
        assert_eq!(b.transport_error_count, 3);
        assert_eq!(b.eof_reset_count, 2);
        assert_eq!(b.total_retry_backoff_ms, 220_000);

        let storm_turn = &b.turns[1];
        assert_eq!(storm_turn.transport_errors, 3);
        assert_eq!(storm_turn.retry_backoff_ms, 220_000);
        assert_eq!(storm_turn.llm_inference_ms, 6_000);
        assert_eq!(storm_turn.tool_exec_ms, 1_000);
        assert_eq!(storm_turn.total_turn_ms, 227_000);

        assert_eq!(b.first_edit_context_tokens, Some(162_000));
        assert_eq!(b.pre_edit_delta_tokens, Some(7_000));
    }
}
