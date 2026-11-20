use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub conversation_id: String,
    pub transcript_path: String,
    pub mtime_ms: u64,
    pub size_bytes: u64,
    pub is_subagent: bool,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub workspace: String,
    pub preview: String,
}

#[derive(Debug, Clone)]
struct DbSessionMeta {
    title: String,
    workspace: String,
    is_subagent: bool,
}

#[derive(Debug, Default)]
struct CachedTitleIndex {
    stamp: (u128, u64),
    entries: HashMap<String, DbSessionMeta>,
}

/// Strip URLs, prompt XML tags, `@[...]` directives, and home paths so labels stay clean and private.
pub fn sanitize_session_title(raw: &str, workspace: &str, conv_id: &str) -> String {
    let stripped = raw
        .replace("<CONTEXT_SUMMARY>", " ")
        .replace("</CONTEXT_SUMMARY>", " ")
        .replace("<USER_REQUEST>", " ")
        .replace("</USER_REQUEST>", " ")
        .replace(['\n', '\r'], " ");

    let clean_tokens: Vec<&str> = stripped
        .split_whitespace()
        .filter(|tok| {
            let lower = tok.to_ascii_lowercase();
            !(lower.starts_with("http://")
                || lower.starts_with("https://")
                || lower.starts_with("file://")
                || lower.starts_with("codex://")
                || tok.starts_with("@[")
                || tok.starts_with("/Users/")
                || tok.starts_with("/home/")
                || tok.starts_with("C:\\Users\\"))
        })
        .collect();

    let joined = clean_tokens.join(" ");
    let trimmed = joined
        .trim_matches(|c: char| c.is_whitespace() || matches!(c, '-' | ':' | '|' | '·' | '/' | ','));
    if !trimmed.is_empty() {
        return trimmed.chars().take(48).collect();
    }
    let short_id = &conv_id[..conv_id.len().min(8)];
    if !workspace.is_empty() {
        format!("{} · #{}", workspace, short_id)
    } else {
        format!("Session #{}", short_id)
    }
}

fn percent_decode_utf8(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn extract_workspace_basename(workspace_uris_json: &str, home_dir: Option<&Path>) -> String {
    let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(workspace_uris_json) else {
        return String::new();
    };
    let Some(first) = arr.first().and_then(Value::as_str) else {
        return String::new();
    };
    let decoded = percent_decode_utf8(first.strip_prefix("file://").unwrap_or(first));
    let p = Path::new(&decoded);
    if home_dir.is_some_and(|h| p == h) {
        return String::new();
    }
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

/// Optional mtime-cached read of Antigravity's native title database when `sqlite3` is present;
/// gracefully degrades to pure-Rust transcript sniffing on minimal Linux / Windows hosts.
fn load_antigravity_title_index() -> HashMap<String, DbSessionMeta> {
    static CACHE: OnceLock<Mutex<CachedTitleIndex>> = OnceLock::new();
    let Some(home) = dirs::home_dir() else {
        return HashMap::new();
    };
    let Some(db) = ["antigravity", "antigravity-ide", "antigravity-cli"]
        .into_iter()
        .map(|p| home.join(".gemini").join(p).join("conversation_summaries.db"))
        .find(|p| p.is_file())
    else {
        return HashMap::new();
    };
    let Ok(meta) = fs::metadata(&db) else {
        return HashMap::new();
    };
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let stamp = (mtime_ns, meta.len());

    let lock = CACHE.get_or_init(|| Mutex::new(CachedTitleIndex::default()));
    if let Ok(guard) = lock.lock() {
        if guard.stamp == stamp && !guard.entries.is_empty() {
            return guard.entries.clone();
        }
    }

    let sql = "SELECT conversation_id, replace(replace(title, char(9), ' '), char(10), ' '), replace(workspace_uris, char(9), ' '), nesting_depth, parent_conversation_id FROM conversation_summaries ORDER BY last_modified_time DESC LIMIT 200;";
    let Ok(out) = Command::new("sqlite3")
        .args(["-readonly", "-separator", "\t"])
        .arg(&db)
        .arg(sql)
        .output()
    else {
        return HashMap::new();
    };
    if !out.status.success() {
        return HashMap::new();
    }

    let mut entries = HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 5 {
            continue;
        }
        let cid = cols[0].trim();
        if cid.is_empty() {
            continue;
        }
        let nesting_depth: i64 = cols[3].trim().parse().unwrap_or(0);
        entries.insert(
            cid.to_string(),
            DbSessionMeta {
                title: cols[1].trim().to_string(),
                workspace: extract_workspace_basename(cols[2].trim(), Some(&home)),
                is_subagent: nesting_depth > 0 || !cols[4].trim().is_empty(),
            },
        );
    }

    if let Ok(mut guard) = lock.lock() {
        guard.stamp = stamp;
        guard.entries = entries.clone();
    }
    entries
}

pub fn discover_session_summaries(limit: usize) -> Vec<SessionSummary> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let db_index = load_antigravity_title_index();
    let mut found: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    for product in ["antigravity", "antigravity-ide", "antigravity-cli"] {
        let brain = home.join(".gemini").join(product).join("brain");
        let Ok(entries) = fs::read_dir(&brain) else {
            continue;
        };
        for ent in entries.flatten() {
            let p = ent
                .path()
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl");
            if let Ok(meta) = fs::metadata(&p) {
                if meta.len() > 200 {
                    found.push((meta.modified().unwrap_or(UNIX_EPOCH), meta.len(), p));
                }
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found
        .into_iter()
        .take(limit.clamp(1, 200))
        .map(|(mtime, size_bytes, p)| {
            let conv_id = p
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();

            let (is_subagent, workspace, title) = if let Some(dbm) = db_index.get(&conv_id) {
                let raw_t = if !dbm.title.is_empty() {
                    dbm.title.clone()
                } else {
                    sniff_session_header(&p, size_bytes).1
                };
                (
                    dbm.is_subagent,
                    dbm.workspace.clone(),
                    sanitize_session_title(&raw_t, &dbm.workspace, &conv_id),
                )
            } else {
                let (heur_sub, sniffed) = sniff_session_header(&p, size_bytes);
                (heur_sub, String::new(), sanitize_session_title(&sniffed, "", &conv_id))
            };

            SessionSummary {
                conversation_id: conv_id,
                transcript_path: p.display().to_string(),
                mtime_ms: mtime.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
                size_bytes,
                is_subagent,
                preview: title.clone(),
                title,
                workspace,
            }
        })
        .collect()
}

fn sniff_session_header(path: &Path, size_bytes: u64) -> (bool, String) {
    let Ok(file) = File::open(path) else {
        return (false, String::new());
    };
    let mut user_inputs = 0usize;
    let mut first_preview = String::new();
    let mut looks_delegated = false;

    for line in BufReader::new(file).lines().take(24).map_while(Result::ok) {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) == Some("USER_INPUT") {
            user_inputs += 1;
            let content = v.get("content").and_then(Value::as_str).unwrap_or("").trim();
            if first_preview.is_empty() && !content.is_empty() {
                let cleaned = content
                    .replace("<CONTEXT_SUMMARY>", "")
                    .replace("</CONTEXT_SUMMARY>", "")
                    .replace("<USER_REQUEST>", "")
                    .replace("</USER_REQUEST>", "")
                    .replace('\n', " ");
                let tc = cleaned.trim();
                if tc.starts_with("You are ")
                    || tc.starts_with("Audit ")
                    || tc.starts_with("Research ")
                    || tc.starts_with("Role:")
                {
                    looks_delegated = true;
                }
                first_preview = tc.chars().take(48).collect();
            }
        }
    }
    (looks_delegated || (user_inputs <= 1 && size_bytes < 12_000), first_preview)
}

pub fn discover_transcripts(limit: usize) -> Vec<PathBuf> {
    discover_session_summaries(limit)
        .into_iter()
        .map(|s| PathBuf::from(s.transcript_path))
        .collect()
}

pub fn discover_primary_transcript() -> Option<PathBuf> {
    let all = discover_session_summaries(15);
    all.iter()
        .find(|s| !s.is_subagent)
        .or_else(|| all.first())
        .map(|s| PathBuf::from(&s.transcript_path))
}
