use crate::namer::{
    load_session_registry, resolve_session_identity, ResolvedIdentity, SessionRegistry,
    ALLOWED_TITLE_EMOJIS,
};
use crate::session_index::UnifiedSessionItem;
use crate::sessions::{decode_workspace_uris, sanitize_session_title};
use crate::transcript::parse_iso8601_epoch_ms;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const ACTIVE_CUTOFF_MS: u64 = 12 * 3_600 * 1_000;
const UNREAD_CUTOFF_MS: u64 = 24 * 3_600 * 1_000;
const CODEX_ROLLOUT_INSPECT_WINDOW_MS: u64 = 30 * 60 * 1_000;
const AGY_ERROR_STALL_SECS: u64 = 45;
const AGY_SILENT_STALL_SECS: u64 = 180;
const CODEX_SILENT_STALL_SECS: u64 = 300;
const CODEX_ACTIVE_MAX_IDLE_SECS: u64 = 900;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionLiveState {
    Waiting,
    Stalled,
    Unread,
    Running,
    #[default]
    Idle,
}

impl SessionLiveState {
    pub fn urgency_rank(self) -> u8 {
        match self {
            Self::Waiting => 0,
            Self::Stalled => 1,
            Self::Unread => 2,
            Self::Running => 3,
            Self::Idle => 4,
        }
    }

    pub fn is_attention(self) -> bool {
        !matches!(self, Self::Idle)
    }

    pub fn is_active_work(self) -> bool {
        matches!(self, Self::Running | Self::Waiting | Self::Stalled)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Stalled => "stalled",
            Self::Unread => "unread",
            Self::Running => "running",
            Self::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RadarRuntimeLedger {
    /// Epoch ms when a session was observed transitioning from Running/Waiting -> Idle.
    pub completed_at_ms: HashMap<String, u64>,
    /// Epoch ms when the user acknowledged/clicked a session in Hyper Deck or MenuBar.
    pub acknowledged_at_ms: HashMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusSessionRequest {
    pub seq: u64,
    pub client: String,
    pub conversation_id: String,
    #[serde(default)]
    pub split: Option<String>,
    pub requested_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RadarSessionItem {
    pub id: String,
    pub client: String,
    pub title: String,
    pub group: String,
    pub code: String,
    pub workspace: String,
    pub live_state: SessionLiveState,
    pub state_reason: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub open_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRadarResponse {
    pub ok: bool,
    pub waiting_count: usize,
    pub stalled_count: usize,
    pub unread_count: usize,
    pub running_count: usize,
    pub sound_enabled: bool,
    #[serde(default = "crate::config::default_ui_language")]
    pub ui_language: String,
    #[serde(default = "crate::config::default_radar_glyph_style")]
    pub glyph_style: String,
    pub elapsed_ms: f64,
    pub attention_items: Vec<RadarSessionItem>,
    pub recent_items: Vec<RadarSessionItem>,
    #[serde(default)]
    pub focus_request: Option<FocusSessionRequest>,
}

pub fn now_epoch_ms_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn parse_sqlite_datetime_ms(raw: &str) -> u64 {
    let t = raw.trim();
    if t.is_empty() {
        return 0;
    }
    if let Some(ms) = parse_iso8601_epoch_ms(t) {
        return ms.max(0) as u64;
    }
    if t.len() >= 19 && t.as_bytes()[10] == b' ' {
        let mut iso = String::with_capacity(t.len() + 1);
        iso.push_str(&t[..10]);
        iso.push('T');
        iso.push_str(&t[11..]);
        if !iso.ends_with('Z') && !iso[11..].contains('+') && !iso[11..].contains('-') {
            iso.push('Z');
        }
        if let Some(ms) = parse_iso8601_epoch_ms(&iso) {
            return ms.max(0) as u64;
        }
    }
    0
}

fn decode_hex_bytes(hex_str: &str) -> Vec<u8> {
    let clean = hex_str.trim();
    if clean.len() < 2 || (clean.len() & 1) != 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(clean.len() / 2);
    let bytes = clean.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16);
        let lo = (bytes[i + 1] as char).to_digit(16);
        match (hi, lo) {
            (Some(h), Some(l)) => out.push(((h << 4) | l) as u8),
            _ => return Vec::new(),
        }
        i += 2;
    }
    out
}

fn read_proto_varint(buf: &[u8], mut pos: usize) -> Option<(u64, usize)> {
    let mut result = 0u64;
    let mut shift = 0u32;
    while pos < buf.len() {
        let b = buf[pos];
        pos += 1;
        if shift >= 64 {
            return None;
        }
        result |= u64::from(b & 0x7F) << shift;
        if (b & 0x80) == 0 {
            return Some((result, pos));
        }
        shift += 7;
    }
    None
}

/// Inspects `conversation_summaries.raw_summary` protobuf wire bytes for field #8 (`waiting_steps`, wire tag `0x42`).
pub fn raw_summary_has_waiting_steps(buf: &[u8]) -> bool {
    let mut pos = 0usize;
    while pos < buf.len() {
        let Some((tag, next_pos)) = read_proto_varint(buf, pos) else {
            break;
        };
        pos = next_pos;
        let field_num = tag >> 3;
        let wire_type = tag & 0x07;
        match wire_type {
            0 => {
                let Some((val, p2)) = read_proto_varint(buf, pos) else {
                    break;
                };
                if field_num == 8 && val > 0 {
                    return true;
                }
                pos = p2;
            }
            1 => {
                if pos + 8 > buf.len() {
                    break;
                }
                pos += 8;
            }
            2 => {
                let Some((len, p2)) = read_proto_varint(buf, pos) else {
                    break;
                };
                let len_usize = len as usize;
                if p2 + len_usize > buf.len() {
                    break;
                }
                if field_num == 8 && len_usize > 0 {
                    return true;
                }
                pos = p2 + len_usize;
            }
            5 => {
                if pos + 4 > buf.len() {
                    break;
                }
                pos += 4;
            }
            _ => break,
        }
    }
    false
}

/// Parses `last_user_view_time:{seconds:1795707024 nanos:708000000}` from `~/.gemini/antigravity/annotations/<cid>.pbtxt`.
pub fn parse_annotation_last_view_ms(pbtxt: &str) -> u64 {
    let Some(idx) = pbtxt.find("last_user_view_time") else {
        return 0;
    };
    let tail = &pbtxt[idx..];
    let Some(open_brace) = tail.find('{') else {
        return 0;
    };
    let inside = &tail[open_brace + 1..];
    let Some(close_brace) = inside.find('}') else {
        return 0;
    };
    let block = &inside[..close_brace];
    let mut secs: u64 = 0;
    let mut nanos: u64 = 0;
    for token_pair in block.split_whitespace().collect::<Vec<_>>().windows(1) {
        let tok = token_pair[0];
        if let Some(rest) = tok.strip_prefix("seconds:") {
            secs = rest.trim().parse::<u64>().unwrap_or(0);
        } else if let Some(rest) = tok.strip_prefix("nanos:") {
            nanos = rest.trim().parse::<u64>().unwrap_or(0);
        }
    }
    if secs == 0 {
        let mut iter = block.split_whitespace();
        while let Some(tok) = iter.next() {
            if tok == "seconds:" {
                if let Some(v) = iter.next() {
                    secs = v.parse::<u64>().unwrap_or(0);
                }
            } else if tok == "nanos:" {
                if let Some(v) = iter.next() {
                    nanos = v.parse::<u64>().unwrap_or(0);
                }
            }
        }
    }
    if secs == 0 {
        0
    } else {
        secs.saturating_mul(1_000)
            .saturating_add(nanos / 1_000_000)
    }
}

fn read_annotation_view_ms(annotations_dirs: &[PathBuf], cid: &str) -> u64 {
    for dir in annotations_dirs {
        let p = dir.join(format!("{}.pbtxt", cid));
        if let Ok(raw) = fs::read_to_string(&p) {
            let ms = parse_annotation_last_view_ms(&raw);
            if ms > 0 {
                return ms;
            }
        }
    }
    0
}

#[derive(Debug, Default)]
struct AgyTranscriptTailSignal {
    mtime_ms: u64,
    last_is_error: bool,
    waiting_question: bool,
}

fn inspect_agy_transcript_signal(brain_dirs: &[PathBuf], cid: &str) -> AgyTranscriptTailSignal {
    for bdir in brain_dirs {
        let tpath = bdir
            .join(cid)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        let Ok(meta) = fs::metadata(&tpath) else {
            continue;
        };
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let Ok(mut f) = File::open(&tpath) else {
            return AgyTranscriptTailSignal {
                mtime_ms,
                ..Default::default()
            };
        };
        let len = meta.len();
        let seek_pos = len.saturating_sub(6144);
        let _ = f.seek(SeekFrom::Start(seek_pos));
        let mut buf = Vec::with_capacity(6144);
        let _ = f.read_to_end(&mut buf);
        let text = String::from_utf8_lossy(&buf);
        let mut last_type = String::new();
        let mut waiting_question = false;
        for line in text.lines().rev() {
            let trimmed = line.trim();
            if !trimmed.starts_with('{') {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            let typ = v.get("type").and_then(Value::as_str).unwrap_or("");
            if last_type.is_empty() && !typ.is_empty() {
                last_type = typ.to_string();
                if typ == "PLANNER_RESPONSE" {
                    if let Some(calls) = v.get("tool_calls").and_then(Value::as_array) {
                        if calls.iter().any(|c| {
                            c.get("name")
                                .and_then(Value::as_str)
                                .is_some_and(|n| n == "ask_question")
                        }) {
                            waiting_question = true;
                        }
                    }
                }
                break;
            }
        }
        return AgyTranscriptTailSignal {
            mtime_ms,
            last_is_error: last_type == "ERROR_MESSAGE",
            waiting_question,
        };
    }
    AgyTranscriptTailSignal::default()
}

/// Inspects the tail of a Codex rollout `.jsonl` file to classify open turns, approval gates, or stalls.
pub fn classify_codex_rollout_tail(path: &Path, now_ms: u64) -> (SessionLiveState, &'static str) {
    let Ok(meta) = fs::metadata(path) else {
        return (SessionLiveState::Idle, "recent");
    };
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if now_ms.saturating_sub(mtime_ms) > CODEX_ROLLOUT_INSPECT_WINDOW_MS {
        return (SessionLiveState::Idle, "recent");
    }
    let Ok(mut f) = File::open(path) else {
        return (SessionLiveState::Idle, "recent");
    };
    let len = meta.len();
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(12_288)));
    let mut buf = Vec::with_capacity(12_288);
    if f.read_to_end(&mut buf).is_err() {
        return (SessionLiveState::Idle, "recent");
    }

    let mut turn_open = false;
    let mut waiting_gate = false;
    let mut saw_lifecycle = false;
    for line in String::from_utf8_lossy(&buf).lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let top_type = v.get("type").and_then(Value::as_str).unwrap_or("");
        let sub_type = v
            .get("payload")
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");

        match (top_type, sub_type) {
            ("event_msg", "task_complete" | "turn_aborted" | "shutdown_complete") => {
                saw_lifecycle = true;
                turn_open = false;
                waiting_gate = false;
            }
            (
                "event_msg",
                "exec_approval_request" | "apply_patch_approval_request" | "request_user_input",
            ) => {
                saw_lifecycle = true;
                turn_open = true;
                waiting_gate = true;
            }
            ("turn_context", _)
            | ("event_msg", "task_started" | "user_message" | "agent_reasoning")
            | (
                "response_item",
                "function_call"
                | "custom_tool_call"
                | "function_call_output"
                | "custom_tool_call_output"
                | "reasoning",
            ) => {
                saw_lifecycle = true;
                turn_open = true;
                waiting_gate = false;
            }
            _ => {}
        }
    }

    if !saw_lifecycle || !turn_open {
        return (SessionLiveState::Idle, "recent");
    }
    if waiting_gate {
        return (SessionLiveState::Waiting, "approval_gate");
    }
    let idle_secs = now_ms.saturating_sub(mtime_ms) / 1_000;
    if idle_secs > CODEX_ACTIVE_MAX_IDLE_SECS {
        return (SessionLiveState::Idle, "recent");
    }
    if idle_secs >= CODEX_SILENT_STALL_SECS {
        return (SessionLiveState::Stalled, "silent_stalled");
    }
    (SessionLiveState::Running, "active_turn")
}

fn resolve_identity_from_title_prefix(
    registry: Option<&SessionRegistry>,
    raw_title: &str,
) -> Option<ResolvedIdentity> {
    let reg = registry?;
    let trimmed = raw_title.trim();
    if trimmed.is_empty() {
        return None;
    }
    let after_emoji = if let Some(rest) = trimmed.strip_prefix("⚙ ") {
        Some(rest.trim())
    } else {
        ALLOWED_TITLE_EMOJIS.iter().find_map(|em| {
            trimmed
                .strip_prefix(em)
                .and_then(|r| r.strip_prefix(' '))
                .map(str::trim)
        })
    }?;
    let head = after_emoji.split('｜').next().unwrap_or(after_emoji).trim();
    let (code_cand, _) = head.split_once('·')?;
    let code_trimmed = code_cand.trim();
    if code_trimmed.is_empty() {
        return None;
    }
    for rule in &reg.rules {
        if rule.code.trim().eq_ignore_ascii_case(code_trimmed) {
            let code = (!rule.code.trim().is_empty()).then(|| rule.code.trim().to_string());
            let group = if !rule.group.trim().is_empty() {
                Some(rule.group.trim().to_string())
            } else {
                code.clone()
            };
            let tag = (!rule.tag.trim().is_empty()).then(|| rule.tag.trim().to_string());
            return Some(ResolvedIdentity {
                rule_id: (!rule.id.trim().is_empty()).then(|| rule.id.trim().to_string()),
                prefix: code,
                tag,
                group,
            });
        }
    }
    if !reg.default_code.trim().is_empty()
        && reg.default_code.trim().eq_ignore_ascii_case(code_trimmed)
    {
        let def = reg.default_code.trim().to_string();
        return Some(ResolvedIdentity {
            rule_id: None,
            prefix: Some(def.clone()),
            tag: None,
            group: Some(def),
        });
    }
    None
}

fn apply_identity_labels(identity: &ResolvedIdentity, fallback_ws: &str) -> (String, String) {
    let group = identity
        .group
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| (!fallback_ws.is_empty()).then(|| fallback_ws.to_string()))
        .unwrap_or_else(|| "General".to_string());
    let code = identity
        .prefix
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| group.clone());
    (group, code)
}

fn default_agy_db_path(home: Option<&Path>) -> Option<PathBuf> {
    std::env::var("AGY_HYPER_SUMMARIES_DB")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let h = home?;
            ["antigravity", "antigravity-ide", "antigravity-cli"]
                .into_iter()
                .map(|p| h.join(".gemini").join(p).join("conversation_summaries.db"))
                .find(|p| p.is_file())
        })
}

fn default_brain_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    if let Ok(custom) = std::env::var("AGY_HYPER_BRAIN_DIR") {
        let t = custom.trim();
        if !t.is_empty() {
            return vec![PathBuf::from(t)];
        }
    }
    let Some(h) = home else {
        return Vec::new();
    };
    ["antigravity", "antigravity-ide", "antigravity-cli"]
        .into_iter()
        .map(|p| h.join(".gemini").join(p).join("brain"))
        .collect()
}

fn default_annotations_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    if let Ok(custom) = std::env::var("AGY_HYPER_ANNOTATIONS_DIR") {
        let t = custom.trim();
        if !t.is_empty() {
            return vec![PathBuf::from(t)];
        }
    }
    let Some(h) = home else {
        return Vec::new();
    };
    ["antigravity", "antigravity-ide", "antigravity-cli"]
        .into_iter()
        .map(|p| h.join(".gemini").join(p).join("annotations"))
        .collect()
}

fn default_codex_db_path(home: Option<&Path>) -> Option<PathBuf> {
    std::env::var("AGY_HYPER_CODEX_DB")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".codex").join("state_5.sqlite")))
}

fn query_active_agy_children(db_path: &Path) -> HashMap<String, u64> {
    let sql = "SELECT coalesce(parent_conversation_id, ''), max(coalesce(last_modified_time, '')) \
               FROM conversation_summaries \
               WHERE coalesce(parent_conversation_id, '') != '' AND coalesce(not_fully_idle, 0) = 1 \
               GROUP BY parent_conversation_id;";
    let Ok(out) = Command::new("sqlite3")
        .args(["-readonly", "-cmd", ".timeout 600", "-separator", "\t"])
        .arg(db_path)
        .arg(sql)
        .output()
    else {
        return HashMap::new();
    };
    if !out.status.success() {
        return HashMap::new();
    }
    let mut map = HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((parent_id, ts_str)) = line.split_once('\t') {
            let pid = parent_id.trim();
            if !pid.is_empty() {
                map.insert(pid.to_string(), parse_sqlite_datetime_ms(ts_str));
            }
        }
    }
    map
}

pub fn build_session_radar(
    recent_limit: usize,
    sound_enabled: bool,
    ledger: Option<&RadarRuntimeLedger>,
    focus_request: Option<FocusSessionRequest>,
) -> SessionRadarResponse {
    let home = dirs::home_dir();
    let agy_db = default_agy_db_path(home.as_deref());
    let brain_dirs = default_brain_dirs(home.as_deref());
    let ann_dirs = default_annotations_dirs(home.as_deref());
    let codex_db = default_codex_db_path(home.as_deref());
    let registry = load_session_registry();
    build_session_radar_with_paths(
        recent_limit,
        sound_enabled,
        agy_db.as_deref(),
        &brain_dirs,
        &ann_dirs,
        codex_db.as_deref(),
        registry.as_ref(),
        home.as_deref(),
        ledger,
        focus_request,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn build_session_radar_with_paths(
    recent_limit: usize,
    sound_enabled: bool,
    agy_db_path: Option<&Path>,
    brain_dirs: &[PathBuf],
    annotations_dirs: &[PathBuf],
    codex_db_path: Option<&Path>,
    registry: Option<&SessionRegistry>,
    home_dir: Option<&Path>,
    ledger: Option<&RadarRuntimeLedger>,
    focus_request: Option<FocusSessionRequest>,
) -> SessionRadarResponse {
    let t0 = Instant::now();
    let now_ms = now_epoch_ms_u64();
    let mut all_candidates: Vec<RadarSessionItem> = Vec::new();

    if let Some(db) = agy_db_path.filter(|p| p.is_file()) {
        let active_children = query_active_agy_children(db);
        let sql = "SELECT s.conversation_id, \
                          replace(replace(coalesce(s.title, ''), char(9), ' '), char(10), ' '), \
                          replace(replace(coalesce(s.preview, ''), char(9), ' '), char(10), ' '), \
                          replace(coalesce(s.workspace_uris, ''), char(9), ' '), \
                          coalesce(s.project_id, ''), \
                          coalesce(s.last_modified_time, ''), \
                          coalesce(s.last_user_input_time, ''), \
                          coalesce(s.not_fully_idle, 0), \
                          CASE WHEN coalesce(s.not_fully_idle, 0) = 1 THEN hex(coalesce(s.raw_summary, X'')) ELSE '' END \
                   FROM conversation_summaries s \
                   LEFT JOIN conversation_parents p ON p.conversation_id = s.conversation_id \
                   WHERE s.nesting_depth = 0 \
                     AND coalesce(s.parent_conversation_id, '') = '' \
                     AND coalesce(p.parent_id, '') = '' \
                     AND coalesce(s.agent_name, '') = '' \
                     AND coalesce(s.killed, 0) = 0 \
                     AND coalesce(s.project_id, '') != 'default-cli-project' \
                     AND (s.step_count > 0 OR s.title != '' OR s.preview != '') \
                   ORDER BY s.not_fully_idle DESC, s.last_modified_time DESC LIMIT 80;";
        if let Ok(out) = Command::new("sqlite3")
            .args(["-readonly", "-cmd", ".timeout 700", "-separator", "\t"])
            .arg(db)
            .arg(sql)
            .output()
        {
            if out.status.success() {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    let cols: Vec<&str> = line.split('\t').collect();
                    if cols.len() < 9 {
                        continue;
                    }
                    let cid = cols[0].trim();
                    if cid.is_empty() {
                        continue;
                    }
                    let raw_title = cols[1].trim();
                    let raw_preview = cols[2].trim();
                    let workspaces = decode_workspace_uris(cols[3].trim());
                    if workspaces
                        .iter()
                        .any(|w| w.contains("/agy_hyper_ab_") || w.contains("/agy-hyper-namer-"))
                        || raw_preview.starts_with("You are a deterministic session title editor.")
                    {
                        continue;
                    }
                    let project_id = cols[4].trim();
                    let updated_ms = parse_sqlite_datetime_ms(cols[5]);
                    let input_ms = parse_sqlite_datetime_ms(cols[6]);
                    let nfi = cols[7].trim() == "1";
                    let raw_hex = cols[8].trim();

                    let ws_basename = workspaces
                        .first()
                        .and_then(|w| {
                            let p = Path::new(w);
                            if home_dir.is_some_and(|h| p == h) {
                                None
                            } else {
                                p.file_name().and_then(|s| s.to_str())
                            }
                        })
                        .unwrap_or("")
                        .to_string();

                    let identity = resolve_identity_from_title_prefix(registry, raw_title)
                        .unwrap_or_else(|| {
                            let combined = format!("{} {}", raw_title, raw_preview);
                            resolve_session_identity(
                                registry,
                                &workspaces,
                                &combined,
                                project_id,
                                home_dir,
                            )
                        });
                    let (group, code) = apply_identity_labels(&identity, &ws_basename);
                    let display_title = if !raw_title.is_empty() {
                        raw_title.to_string()
                    } else {
                        sanitize_session_title(raw_preview, &ws_basename, cid)
                    };

                    let (live_state, reason) = classify_agy_state(
                        cid,
                        nfi,
                        raw_hex,
                        updated_ms,
                        now_ms,
                        brain_dirs,
                        annotations_dirs,
                        &active_children,
                        ledger,
                    );

                    all_candidates.push(RadarSessionItem {
                        id: cid.to_string(),
                        client: "agy".to_string(),
                        title: display_title,
                        group,
                        code,
                        workspace: ws_basename,
                        live_state,
                        state_reason: reason.to_string(),
                        created_at_ms: if input_ms > 0 { input_ms } else { updated_ms },
                        updated_at_ms: updated_ms,
                        open_url: None,
                    });
                }
            }
        }
    }

    if let Some(cdb) = codex_db_path.filter(|p| p.is_file()) {
        let sql = "SELECT id, \
                          replace(replace(coalesce(nullif(trim(name), ''), nullif(trim(title), ''), ''), char(9), ' '), char(10), ' '), \
                          replace(coalesce(cwd, ''), char(9), ' '), \
                          CASE WHEN coalesce(created_at_ms, 0) > 0 THEN created_at_ms ELSE coalesce(created_at, 0) * 1000 END, \
                          CASE WHEN coalesce(updated_at_ms, 0) > 0 THEN updated_at_ms ELSE coalesce(updated_at, 0) * 1000 END, \
                          replace(coalesce(rollout_path, ''), char(9), ' '), \
                          replace(replace(substr(coalesce(nullif(trim(preview), ''), nullif(trim(first_user_message), ''), ''), 1, 240), char(9), ' '), char(10), ' ') \
                   FROM threads \
                   WHERE archived = 0 \
                     AND attached_thread_parent_id IS NULL \
                     AND source IN ('vscode', 'cli') \
                   ORDER BY updated_at DESC LIMIT 60;";
        if let Ok(out) = Command::new("sqlite3")
            .args(["-readonly", "-cmd", ".timeout 700", "-separator", "\t"])
            .arg(cdb)
            .arg(sql)
            .output()
        {
            if out.status.success() {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    let cols: Vec<&str> = line.split('\t').collect();
                    if cols.len() < 7 {
                        continue;
                    }
                    let id = cols[0].trim();
                    if id.is_empty() {
                        continue;
                    }
                    let raw_title = cols[1].trim();
                    let cwd = cols[2].trim();
                    let created_ms = cols[3].trim().parse::<u64>().unwrap_or(0);
                    let updated_ms = cols[4].trim().parse::<u64>().unwrap_or(created_ms);
                    let rollout_path = cols[5].trim();
                    let raw_preview = cols[6].trim();

                    let ws_basename = if !cwd.is_empty() {
                        let p = Path::new(cwd);
                        if home_dir.is_some_and(|h| p == h) {
                            ""
                        } else {
                            p.file_name().and_then(|s| s.to_str()).unwrap_or("")
                        }
                    } else {
                        ""
                    };
                    let ws_vec = if cwd.is_empty() {
                        Vec::new()
                    } else {
                        vec![cwd.to_string()]
                    };
                    let identity = resolve_identity_from_title_prefix(registry, raw_title)
                        .unwrap_or_else(|| {
                            let combined = format!("{} {}", raw_title, raw_preview);
                            resolve_session_identity(registry, &ws_vec, &combined, "", home_dir)
                        });
                    let (group, code) = apply_identity_labels(&identity, ws_basename);

                    let title = if raw_title.is_empty() {
                        format!("Thread #{}", &id[..id.len().min(8)])
                    } else {
                        raw_title.chars().take(64).collect()
                    };

                    let (mut live_state, mut reason) = if !rollout_path.is_empty()
                        && now_ms.saturating_sub(updated_ms) <= CODEX_ROLLOUT_INSPECT_WINDOW_MS
                    {
                        classify_codex_rollout_tail(Path::new(rollout_path), now_ms)
                    } else {
                        (SessionLiveState::Idle, "recent")
                    };

                    if live_state == SessionLiveState::Idle
                        && now_ms.saturating_sub(updated_ms) <= UNREAD_CUTOFF_MS
                    {
                        if let Some(ldg) = ledger {
                            let comp_ms = ldg.completed_at_ms.get(id).copied().unwrap_or(0);
                            let ack_ms = ldg.acknowledged_at_ms.get(id).copied().unwrap_or(0);
                            if comp_ms > ack_ms.saturating_add(1_000) {
                                live_state = SessionLiveState::Unread;
                                reason = "unread_complete";
                            }
                        }
                    }

                    all_candidates.push(RadarSessionItem {
                        id: id.to_string(),
                        client: "codex".to_string(),
                        title,
                        group,
                        code,
                        workspace: ws_basename.to_string(),
                        live_state,
                        state_reason: reason.to_string(),
                        created_at_ms: created_ms,
                        updated_at_ms: updated_ms.max(created_ms),
                        open_url: Some(format!("codex://threads/{}", id)),
                    });
                }
            }
        }
    }

    let mut waiting_count = 0usize;
    let mut stalled_count = 0usize;
    let mut unread_count = 0usize;
    let mut running_count = 0usize;
    let mut attention_items = Vec::new();
    let mut idle_items = Vec::new();

    for item in all_candidates {
        match item.live_state {
            SessionLiveState::Waiting => {
                waiting_count += 1;
                attention_items.push(item);
            }
            SessionLiveState::Stalled => {
                stalled_count += 1;
                attention_items.push(item);
            }
            SessionLiveState::Unread => {
                unread_count += 1;
                attention_items.push(item);
            }
            SessionLiveState::Running => {
                running_count += 1;
                attention_items.push(item);
            }
            SessionLiveState::Idle => {
                idle_items.push(item);
            }
        }
    }

    attention_items.sort_by(|a, b| {
        a.live_state
            .urgency_rank()
            .cmp(&b.live_state.urgency_rank())
            .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
    });
    idle_items.sort_by_key(|b| std::cmp::Reverse(b.updated_at_ms));
    idle_items.truncate(recent_limit.clamp(1, 24));

    let elapsed_ms = (t0.elapsed().as_secs_f64() * 1000.0 * 100.0).round() / 100.0;
    SessionRadarResponse {
        ok: true,
        waiting_count,
        stalled_count,
        unread_count,
        running_count,
        sound_enabled,
        ui_language: crate::config::default_ui_language(),
        glyph_style: crate::config::default_radar_glyph_style(),
        elapsed_ms,
        attention_items,
        recent_items: idle_items,
        focus_request,
    }
}

#[allow(clippy::too_many_arguments)]
fn classify_agy_state(
    cid: &str,
    not_fully_idle: bool,
    raw_hex: &str,
    updated_ms: u64,
    now_ms: u64,
    brain_dirs: &[PathBuf],
    annotations_dirs: &[PathBuf],
    active_children: &HashMap<String, u64>,
    ledger: Option<&RadarRuntimeLedger>,
) -> (SessionLiveState, &'static str) {
    if not_fully_idle {
        if now_ms.saturating_sub(updated_ms) > ACTIVE_CUTOFF_MS {
            return (SessionLiveState::Idle, "stale_cutoff");
        }
        if !raw_hex.is_empty() {
            let proto_bytes = decode_hex_bytes(raw_hex);
            if raw_summary_has_waiting_steps(&proto_bytes) {
                return (SessionLiveState::Waiting, "approval_gate");
            }
        }
        let sig = inspect_agy_transcript_signal(brain_dirs, cid);
        if sig.waiting_question {
            return (SessionLiveState::Waiting, "question_gate");
        }
        let child_mtime = active_children.get(cid).copied().unwrap_or(0);
        let freshest_ms = updated_ms.max(sig.mtime_ms).max(child_mtime);
        let idle_secs = now_ms.saturating_sub(freshest_ms) / 1_000;
        if sig.last_is_error && idle_secs >= AGY_ERROR_STALL_SECS {
            return (SessionLiveState::Stalled, "error_stalled");
        }
        if child_mtime == 0 && idle_secs >= AGY_SILENT_STALL_SECS {
            return (SessionLiveState::Stalled, "silent_stalled");
        }
        return (SessionLiveState::Running, "active_turn");
    }

    // Idle conversation: check whether it has fresh unread output (<24h)
    if now_ms.saturating_sub(updated_ms) <= UNREAD_CUTOFF_MS {
        let pb_view_ms = read_annotation_view_ms(annotations_dirs, cid);
        let ack_ms = ledger
            .and_then(|l| l.acknowledged_at_ms.get(cid).copied())
            .unwrap_or(0);
        let comp_ms = ledger
            .and_then(|l| l.completed_at_ms.get(cid).copied())
            .unwrap_or(0);
        let effective_view_ms = pb_view_ms.max(ack_ms);

        if pb_view_ms > 0 && updated_ms > effective_view_ms.saturating_add(2_500) {
            return (SessionLiveState::Unread, "unread_complete");
        }
        if comp_ms > effective_view_ms.saturating_add(1_000) {
            return (SessionLiveState::Unread, "unread_complete");
        }
    }
    (SessionLiveState::Idle, "recent")
}

/// Enriches `UnifiedSessionItem` rows from `session_index` using the exact same radar authority.
pub fn enrich_unified_sessions_live_state(
    items: &mut [UnifiedSessionItem],
    agy_db_path: Option<&Path>,
    brain_dirs: &[PathBuf],
    codex_db_path: Option<&Path>,
    home_dir: Option<&Path>,
) {
    let ann_dirs = default_annotations_dirs(home_dir);
    let radar = build_session_radar_with_paths(
        1,
        false,
        agy_db_path,
        brain_dirs,
        &ann_dirs,
        codex_db_path,
        None,
        home_dir,
        None,
        None,
    );
    let mut state_map: HashMap<(String, String), SessionLiveState> = HashMap::new();
    for att in radar.attention_items {
        state_map.insert((att.client, att.id), att.live_state);
    }
    for it in items.iter_mut() {
        let st = state_map
            .get(&(it.client.clone(), it.id.clone()))
            .copied()
            .unwrap_or(SessionLiveState::Idle);
        it.live_state = st;
        it.active = st.is_active_work();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn detects_protobuf_field_8_waiting_steps_and_annotations_view_time() {
        // Protobuf: field 1 = "abc" (0x0A 0x03 'a' 'b' 'c'), field 8 = 4 bytes (0x42 0x04 1 2 3 4)
        let with_f8 = [0x0A, 0x03, b'a', b'b', b'c', 0x42, 0x04, 1, 2, 3, 4];
        assert!(raw_summary_has_waiting_steps(&with_f8));

        // Protobuf without field 8 (field 1 + field 9)
        let without_f8 = [0x0A, 0x02, b'o', b'k', 0x4A, 0x01, 0x01];
        assert!(!raw_summary_has_waiting_steps(&without_f8));

        let pb = r#"title:"💬 Hyper·Test"  tags:"namer:auto"  last_user_view_time:{seconds:1795707024  nanos:708000000}  pinned:false"#;
        assert_eq!(parse_annotation_last_view_ms(pb), 1_795_707_024_708);
    }

    #[test]
    fn classifies_codex_rollout_states_accurately() {
        let dir = tempdir().unwrap();
        let now_ms = now_epoch_ms_u64();

        let done_path = dir.path().join("done.jsonl");
        fs::write(
            &done_path,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_settings_applied\"}}\n"
            ),
        )
        .unwrap();
        assert_eq!(
            classify_codex_rollout_tail(&done_path, now_ms),
            (SessionLiveState::Idle, "recent")
        );

        let running_path = dir.path().join("running.jsonl");
        fs::write(
            &running_path,
            concat!(
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n",
                "{\"type\":\"turn_context\",\"payload\":{}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\"}}\n"
            ),
        )
        .unwrap();
        assert_eq!(
            classify_codex_rollout_tail(&running_path, now_ms),
            (SessionLiveState::Running, "active_turn")
        );

        let waiting_path = dir.path().join("waiting.jsonl");
        fs::write(
            &waiting_path,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"exec_approval_request\"}}\n"
            ),
        )
        .unwrap();
        assert_eq!(
            classify_codex_rollout_tail(&waiting_path, now_ms),
            (SessionLiveState::Waiting, "approval_gate")
        );
    }
}
