use hyper_core::bench::SessionBenchmark;
use hyper_core::config::HyperConfig;
use hyper_core::context_pack::ContextPackRequest;
use hyper_core::sessions::SessionSummary;
use hyper_core::transcript::{HealthLevel, TranscriptPulse};
use hyper_core::usage::{ModelTtlEstimate, SessionUsageSummary};
use hyper_core::wal::{RollbackResult, WalManifest};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DaemonRequest {
    Ping,
    TelemetryEvent {
        kind: String,
        #[serde(default)]
        conversation_id: Option<String>,
        #[serde(default)]
        workspace: Option<String>,
        #[serde(default)]
        duration_ms: u64,
        #[serde(default)]
        saved_ms: u64,
        #[serde(default)]
        turns_saved: u32,
        #[serde(default)]
        detail: String,
        #[serde(default)]
        blocked: bool,
    },
    StoreHandle {
        label: String,
        content: String,
    },
    DrillHandle {
        handle_id: String,
        #[serde(default)]
        offset_lines: usize,
        #[serde(default = "default_drill_limit")]
        limit_lines: usize,
        #[serde(default)]
        grep: Option<String>,
    },
    WatchWorkspace {
        workspace: String,
    },
    NotifyChange {
        workspace: String,
    },
    LookupPrewarm {
        workspace: String,
        preset: String,
    },
    RecordCheckTarget {
        workspace: String,
        preset: String,
    },
    PackContext {
        workspace: String,
        request: ContextPackRequest,
    },
    Snapshot {
        #[serde(default)]
        conversation_id: Option<String>,
        #[serde(default)]
        transcript_path: Option<String>,
    },
    StatuslinePulse {
        #[serde(default)]
        conversation_id: Option<String>,
        #[serde(default)]
        transcript_path: Option<String>,
        #[serde(default)]
        model: Option<String>,
    },
    CacheTtlReport {
        #[serde(default)]
        force_sync: bool,
    },
    UsageSummary {
        #[serde(default = "default_usage_window")]
        window: String,
        #[serde(default)]
        currency: Option<String>,
        #[serde(default)]
        tz_offset_mins: Option<i32>,
        #[serde(default)]
        force_sync: bool,
    },
    SetProfile {
        profile: String,
    },
    SetToggle {
        key: String,
        value: bool,
    },
    SetCurrency {
        currency: String,
    },
    UpdateFxRates {
        rates: BTreeMap<String, f64>,
        #[serde(default)]
        source: Option<String>,
    },
    RollbackTx {
        tx_id: String,
        #[serde(default)]
        force: bool,
    },
    Shutdown,
}

fn default_drill_limit() -> usize {
    80
}

fn default_usage_window() -> String {
    "7d".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub ts_ms: u64,
    pub kind: String,
    pub workspace: Option<String>,
    pub duration_ms: u64,
    pub saved_ms: u64,
    pub detail: String,
    pub blocked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineStatus {
    pub pid: u32,
    pub version: String,
    pub uptime_secs: u64,
    pub rss_mb: f64,
    pub active_workspaces: Vec<(String, u64)>,
    pub handles_cached: usize,
    pub prewarm_cached: usize,
    pub ast_cache_entries: usize,
    pub ast_cache_hits: u64,
    pub ast_cache_misses: u64,
    pub pack_cache_hits: u64,
    pub pack_cache_misses: u64,
    pub tool_counts: HashMap<String, u64>,
    pub total_calls: u64,
    pub total_saved_ms: u64,
    pub total_turns_saved: u64,
    pub blocked_security_events: u64,
    pub avg_tool_latency_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrewarmedCheck {
    pub workspace: String,
    pub preset: String,
    pub workspace_rev: u64,
    pub command_display: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub total_output_bytes: usize,
    pub summary_lines: Vec<String>,
    pub toc_hint: Option<String>,
    pub truncated: bool,
    pub spill_handle: Option<String>,
    pub completed_ts_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentSnapshot {
    pub conversation_id: String,
    #[serde(default)]
    pub parent_conversation_id: Option<String>,
    #[serde(default)]
    pub agent_name: String,
    pub title: String,
    pub nesting_depth: u32,
    pub active: bool,
    pub mtime_ms: u64,
    #[serde(default)]
    pub idle_secs: u64,
    #[serde(default)]
    pub stalled: bool,
    #[serde(default)]
    pub model: Option<String>,
    pub health: HealthLevel,
    #[serde(default)]
    pub health_reason: String,
    pub total_tokens: u64,
    pub cache_hit_ratio: f64,
    pub max_step: u64,
    pub total_turns: usize,
    pub cumulative_tokens: u64,
    pub hyper_tool_calls: usize,
    pub total_tool_calls: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTreeSummary {
    pub root_conversation_id: String,
    pub selected_is_subagent: bool,
    #[serde(default)]
    pub parent_conversation_id: Option<String>,
    pub subagent_count: usize,
    pub active_subagents: usize,
    pub redline_subagents: usize,
    #[serde(default)]
    pub stalled_subagents: usize,
    pub combined_turns: usize,
    pub combined_tokens: u64,
    pub combined_hyper_calls: usize,
    pub combined_total_tools: usize,
    pub combined_cache_hit_ratio: f64,
    pub subagents: Vec<SubagentSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CockpitSnapshot {
    pub engine: EngineStatus,
    pub config: HyperConfig,
    pub pulse: Option<TranscriptPulse>,
    pub benchmark: Option<SessionBenchmark>,
    pub recent_sessions: Vec<SessionSummary>,
    #[serde(default)]
    pub session_tree: Option<SessionTreeSummary>,
    #[serde(default)]
    pub session_usage: Option<SessionUsageSummary>,
    #[serde(default)]
    pub model_ttl_estimates: Vec<ModelTtlEstimate>,
    pub transactions: Vec<WalManifest>,
    pub recent_events: Vec<AuditEvent>,
}

/// Ultra-lightweight lock-free payload for Antigravity CLI `statusLine` / `title` (`<2ms`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatuslinePulseResponse {
    #[serde(default)]
    pub model: Option<String>,
    pub effective_cache_ttl_secs: u64,
    pub cache_ttl_source: String,
    pub cache_warm_remaining_secs: u64,
    pub cache_is_cold: bool,
    pub total_tokens: u64,
    pub effective_red_tokens: u64,
    pub cache_hit_ratio: f64,
    pub health: HealthLevel,
    #[serde(default)]
    pub consecutive_error_streak: u32,
    pub median_turn_ms: u64,
    pub median_llm_ms: u64,
    pub effective_tps: f64,
    pub hyper_adoption_ratio: f64,
    pub total_turns: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrillOutput {
    pub handle_id: String,
    pub label: String,
    pub total_lines: usize,
    pub returned_lines: usize,
    pub offset_lines: usize,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DaemonResponse {
    Ok {
        data: serde_json::Value,
    },
    Err {
        message: String,
    },
}

impl DaemonResponse {
    pub fn ok<T: Serialize>(v: T) -> Self {
        match serde_json::to_value(v) {
            Ok(data) => Self::Ok { data },
            Err(e) => Self::Err {
                message: e.to_string(),
            },
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self::Err {
            message: msg.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandleStored {
    pub handle_id: String,
    pub total_lines: usize,
    pub total_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackPayload {
    pub result: RollbackResult,
}
