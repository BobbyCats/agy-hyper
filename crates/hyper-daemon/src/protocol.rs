use hyper_core::bench::SessionBenchmark;
use hyper_core::config::HyperConfig;
use hyper_core::context_pack::ContextPackRequest;
use hyper_core::sessions::SessionSummary;
use hyper_core::transcript::TranscriptPulse;
use hyper_core::wal::{RollbackResult, WalManifest};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
    SetProfile {
        profile: String,
    },
    SetToggle {
        key: String,
        value: bool,
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
pub struct CockpitSnapshot {
    pub engine: EngineStatus,
    pub config: HyperConfig,
    pub pulse: Option<TranscriptPulse>,
    pub benchmark: Option<SessionBenchmark>,
    pub recent_sessions: Vec<SessionSummary>,
    pub transactions: Vec<WalManifest>,
    pub recent_events: Vec<AuditEvent>,
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
