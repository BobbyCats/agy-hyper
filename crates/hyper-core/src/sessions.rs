use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub conversation_id: String,
    pub transcript_path: String,
    pub mtime_ms: u64,
    pub size_bytes: u64,
    pub is_subagent: bool,
    #[serde(default)]
    pub parent_conversation_id: Option<String>,
    #[serde(default)]
    pub nesting_depth: u32,
    #[serde(default)]
    pub agent_name: String,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub subagent_count: usize,
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
    parent_conversation_id: Option<String>,
    nesting_depth: u32,
    agent_name: String,
    not_fully_idle: bool,
    killed: bool,
}

#[derive(Debug, Default)]
struct CachedTitleIndex {
    stamp: (u128, u64, u128, u64),
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

pub fn percent_decode_utf8(input: &str) -> String {
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

pub fn decode_workspace_uris(workspace_uris_json: &str) -> Vec<String> {
    let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(workspace_uris_json) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(Value::as_str)
        .map(|s| percent_decode_utf8(s.strip_prefix("file://").unwrap_or(s)))
        .filter(|s| !s.trim().is_empty())
        .collect()
}

pub fn extract_workspace_basename(workspace_uris_json: &str, home_dir: Option<&Path>) -> String {
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
    let wal_path = PathBuf::from(format!("{}-wal", db.display()));
    let (wal_mtime_ns, wal_len) = fs::metadata(&wal_path)
        .ok()
        .map(|wm| {
            let mt = wm
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            (mt, wm.len())
        })
        .unwrap_or((0, 0));
    let stamp = (mtime_ns, meta.len(), wal_mtime_ns, wal_len);

    let lock = CACHE.get_or_init(|| Mutex::new(CachedTitleIndex::default()));
    if let Ok(guard) = lock.lock() {
        if guard.stamp == stamp && !guard.entries.is_empty() {
            return guard.entries.clone();
        }
    }

    let sql_joined = "SELECT s.conversation_id, replace(replace(coalesce(s.title, ''), char(9), ' '), char(10), ' '), replace(coalesce(s.workspace_uris, ''), char(9), ' '), coalesce(s.nesting_depth, 0), coalesce(nullif(s.parent_conversation_id, ''), p.parent_id, ''), coalesce(s.agent_name, ''), coalesce(s.not_fully_idle, 0), coalesce(s.killed, 0) FROM conversation_summaries s LEFT JOIN conversation_parents p ON p.conversation_id = s.conversation_id ORDER BY s.last_modified_time DESC LIMIT 300;";
    let sql_fallback = "SELECT conversation_id, replace(replace(coalesce(title, ''), char(9), ' '), char(10), ' '), replace(coalesce(workspace_uris, ''), char(9), ' '), coalesce(nesting_depth, 0), coalesce(parent_conversation_id, ''), coalesce(agent_name, ''), coalesce(not_fully_idle, 0), coalesce(killed, 0) FROM conversation_summaries ORDER BY last_modified_time DESC LIMIT 300;";

    let immutable_uri = format!(
        "file:{}?immutable=1",
        db.display().to_string().replace('?', "%3F").replace('#', "%23")
    );
    let run_query = |sql: &str| {
        let ro = Command::new("sqlite3")
            .args(["-readonly", "-cmd", ".timeout 500", "-separator", "\t"])
            .arg(&db)
            .arg(sql)
            .output();
        if let Ok(o) = ro {
            if o.status.success() {
                return Some(o);
            }
        }
        if wal_len == 0 {
            if let Ok(o) = Command::new("sqlite3")
                .args(["-cmd", ".timeout 500", "-separator", "\t"])
                .arg(&immutable_uri)
                .arg(sql)
                .output()
            {
                if o.status.success() {
                    return Some(o);
                }
            }
        }
        None
    };

    let Some(out) = run_query(sql_joined).or_else(|| run_query(sql_fallback)) else {
        return HashMap::new();
    };

    let mut entries = HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 8 {
            continue;
        }
        let cid = cols[0].trim();
        if cid.is_empty() {
            continue;
        }
        let nesting_depth: u32 = cols[3].trim().parse::<i64>().unwrap_or(0).max(0) as u32;
        let parent_raw = cols[4].trim();
        let parent_conversation_id = (!parent_raw.is_empty()).then(|| parent_raw.to_string());
        let agent_name = cols[5].trim().to_string();
        let not_fully_idle = cols[6].trim() == "1";
        let killed = cols[7].trim() == "1";
        entries.insert(
            cid.to_string(),
            DbSessionMeta {
                title: cols[1].trim().to_string(),
                workspace: extract_workspace_basename(cols[2].trim(), Some(&home)),
                // `run_workflow` nodes set `nesting_depth = 0` and `parent_conversation_id = ""`,
                // while populating `agent_name` (top-level user chats always have `agent_name == ""`).
                is_subagent: nesting_depth > 0
                    || parent_conversation_id.is_some()
                    || !agent_name.is_empty(),
                parent_conversation_id,
                nesting_depth,
                agent_name,
                not_fully_idle,
                killed,
            },
        );
    }

    if let Ok(mut guard) = lock.lock() {
        guard.stamp = stamp;
        guard.entries = entries.clone();
    }
    entries
}

#[derive(Debug, Clone)]
pub struct ActiveChildSession {
    pub conversation_id: String,
    pub agent_name: String,
    pub title: String,
    pub transcript_path: PathBuf,
    pub mtime_ms: u64,
}

/// Single-lookup role metadata from Antigravity's native summary DB: `(is_subagent, agent_name)`.
pub fn lookup_session_role(conv_id: &str) -> (bool, String) {
    let trimmed = conv_id.trim();
    if trimmed.is_empty() {
        return (false, String::new());
    }
    load_antigravity_title_index()
        .get(trimmed)
        .map(|m| (m.is_subagent, m.agent_name.clone()))
        .unwrap_or((false, String::new()))
}

pub fn is_subagent_conversation(conv_id: &str) -> bool {
    lookup_session_role(conv_id).0
}

/// Fast O(active_children) lookup of live (non-killed, not-fully-idle) child subagents
/// under `parent_conv_id` without scanning the full `brain/` directory on `PreInvocation`.
pub fn discover_active_child_sessions(parent_conv_id: &str) -> Vec<ActiveChildSession> {
    let root = parent_conv_id.trim();
    if root.is_empty() {
        return Vec::new();
    }
    let db_index = load_antigravity_title_index();
    let mut out = Vec::new();
    for (cid, meta) in &db_index {
        if cid == root || !meta.is_subagent || meta.killed || !meta.not_fully_idle {
            continue;
        }
        // Walk parent links (bounded) so nested grandchildren under this root are caught too.
        let mut cur = meta.parent_conversation_id.as_deref();
        let mut belongs = false;
        let mut hops = 0usize;
        while let Some(pid) = cur {
            if hops >= 16 || pid.is_empty() {
                break;
            }
            if pid == root {
                belongs = true;
                break;
            }
            cur = db_index
                .get(pid)
                .and_then(|pm| pm.parent_conversation_id.as_deref());
            hops += 1;
        }
        if !belongs {
            continue;
        }
        let Some(tpath) = crate::transcript::resolve_transcript_path(None, Some(cid)) else {
            continue;
        };
        let mtime_ms = fs::metadata(&tpath)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let title = if !meta.title.is_empty() {
            sanitize_session_title(&meta.title, &meta.workspace, cid)
        } else if !meta.agent_name.is_empty() {
            format!("{} #{}", meta.agent_name, &cid[..cid.len().min(8)])
        } else {
            format!("subagent #{}", &cid[..cid.len().min(8)])
        };
        out.push(ActiveChildSession {
            conversation_id: cid.clone(),
            agent_name: meta.agent_name.clone(),
            title,
            transcript_path: tpath,
            mtime_ms,
        });
    }
    out.sort_by_key(|b| std::cmp::Reverse(b.mtime_ms));
    out
}

/// Fast global lookup of currently active (non-killed, not-fully-idle) subagents updated within
/// `max_age_secs` seconds. Used by the background daemon watchdog with zero full `brain/` scans.
pub fn discover_all_active_subagents(max_age_secs: u64) -> Vec<ActiveChildSession> {
    let db_index = load_antigravity_title_index();
    let now_ms = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let max_age_ms = max_age_secs.saturating_mul(1000);
    let mut out = Vec::new();
    for (cid, meta) in &db_index {
        if !meta.is_subagent || meta.killed || !meta.not_fully_idle {
            continue;
        }
        let Some(tpath) = crate::transcript::resolve_transcript_path(None, Some(cid)) else {
            continue;
        };
        let mtime_ms = fs::metadata(&tpath)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if mtime_ms == 0 || (max_age_ms > 0 && now_ms.saturating_sub(mtime_ms) > max_age_ms) {
            continue;
        }
        let title = if !meta.title.is_empty() {
            sanitize_session_title(&meta.title, &meta.workspace, cid)
        } else if !meta.agent_name.is_empty() {
            format!("{} #{}", meta.agent_name, &cid[..cid.len().min(8)])
        } else {
            format!("subagent #{}", &cid[..cid.len().min(8)])
        };
        out.push(ActiveChildSession {
            conversation_id: cid.clone(),
            agent_name: meta.agent_name.clone(),
            title,
            transcript_path: tpath,
            mtime_ms,
        });
    }
    out.sort_by_key(|b| std::cmp::Reverse(b.mtime_ms));
    out
}

const SESSION_SUMMARY_CACHE_TTL: Duration = Duration::from_secs(4);

#[derive(Debug, Clone)]
struct CachedSessionSummaries {
    cached_at: Instant,
    brain_dirs_mtime_fingerprint: Vec<(PathBuf, u128, u64)>,
    cached_limit: usize,
    summaries: Vec<SessionSummary>,
}

fn compute_brain_dirs_fingerprint(brain_dirs: &[PathBuf]) -> Vec<(PathBuf, u128, u64)> {
    brain_dirs
        .iter()
        .map(|dir| {
            if let Ok(meta) = fs::metadata(dir) {
                let mtime_ns = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                (dir.clone(), mtime_ns, meta.len())
            } else {
                (dir.clone(), 0, 0)
            }
        })
        .collect()
}

pub fn discover_session_summaries(limit: usize) -> Vec<SessionSummary> {
    static SUMMARY_CACHE: OnceLock<Mutex<Option<CachedSessionSummaries>>> = OnceLock::new();

    let clamped_limit = limit.clamp(1, 200);
    let has_custom_brain_env = std::env::var_os("AGY_HYPER_BRAIN_DIR").is_some();
    let custom_brain = std::env::var("AGY_HYPER_BRAIN_DIR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);

    if has_custom_brain_env {
        let Some(cb) = custom_brain else {
            return Vec::new();
        };
        return scan_session_summaries(clamped_limit, &[cb], &HashMap::new());
    }

    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let brain_dirs: Vec<PathBuf> = ["antigravity", "antigravity-ide", "antigravity-cli"]
        .into_iter()
        .map(|product| home.join(".gemini").join(product).join("brain"))
        .collect();

    discover_session_summaries_cached(
        clamped_limit,
        &brain_dirs,
        SUMMARY_CACHE.get_or_init(|| Mutex::new(None)),
        SESSION_SUMMARY_CACHE_TTL,
        load_antigravity_title_index,
    )
}

fn discover_session_summaries_cached<F>(
    clamped_limit: usize,
    brain_dirs: &[PathBuf],
    cache_lock: &Mutex<Option<CachedSessionSummaries>>,
    ttl: Duration,
    load_db: F,
) -> Vec<SessionSummary>
where
    F: FnOnce() -> HashMap<String, DbSessionMeta>,
{
    let fingerprint = compute_brain_dirs_fingerprint(brain_dirs);
    if let Ok(guard) = cache_lock.lock() {
        if let Some(cached) = guard.as_ref() {
            if cached.cached_at.elapsed() <= ttl
                && cached.brain_dirs_mtime_fingerprint == fingerprint
                && cached.cached_limit >= clamped_limit
            {
                return cached
                    .summaries
                    .iter()
                    .take(clamped_limit)
                    .cloned()
                    .collect();
            }
        }
    }

    let db_index = load_db();
    let summaries = scan_session_summaries(clamped_limit, brain_dirs, &db_index);
    if let Ok(mut guard) = cache_lock.lock() {
        *guard = Some(CachedSessionSummaries {
            cached_at: Instant::now(),
            brain_dirs_mtime_fingerprint: fingerprint,
            cached_limit: clamped_limit,
            summaries: summaries.clone(),
        });
    }
    summaries
}

fn scan_session_summaries(
    clamped_limit: usize,
    brain_dirs: &[PathBuf],
    db_index: &HashMap<String, DbSessionMeta>,
) -> Vec<SessionSummary> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut found: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    for brain in brain_dirs {
        let Ok(entries) = fs::read_dir(brain) else {
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
    found.sort_by_key(|b| std::cmp::Reverse(b.0));
    // Scan enough candidates so wide 20-50 node `run_workflow` bursts never evict their own
    // parent root conversation before workflow linkage runs.
    let scan_cap = clamped_limit.clamp(64, 250);
    let mut summaries: Vec<SessionSummary> = found
        .into_iter()
        .take(scan_cap)
        .map(|(mtime, size_bytes, p)| {
            let conv_id = p
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();
            let mtime_ms = mtime
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let recently_written = now_ms.saturating_sub(mtime_ms) <= 45_000;

            let (
                is_subagent,
                parent_conversation_id,
                nesting_depth,
                agent_name,
                active,
                workspace,
                title,
            ) = if let Some(dbm) = db_index.get(&conv_id) {
                let raw_t = if !dbm.title.is_empty() {
                    dbm.title.clone()
                } else {
                    sniff_session_header(&p, size_bytes).2
                };
                let fallback_title = if dbm.is_subagent && raw_t.is_empty() {
                    if !dbm.agent_name.is_empty() {
                        format!(
                            "{} #{}",
                            dbm.agent_name,
                            &conv_id[..conv_id.len().min(8)]
                        )
                    } else {
                        format!("subagent #{}", &conv_id[..conv_id.len().min(8)])
                    }
                } else {
                    sanitize_session_title(&raw_t, &dbm.workspace, &conv_id)
                };
                (
                    dbm.is_subagent,
                    dbm.parent_conversation_id.clone(),
                    dbm.nesting_depth,
                    dbm.agent_name.clone(),
                    (dbm.not_fully_idle && !dbm.killed) || recently_written,
                    dbm.workspace.clone(),
                    fallback_title,
                )
            } else {
                let (heur_sub, sniff_parent, sniffed) = sniff_session_header(&p, size_bytes);
                (
                    heur_sub,
                    sniff_parent,
                    u32::from(heur_sub),
                    String::new(),
                    recently_written,
                    String::new(),
                    sanitize_session_title(&sniffed, "", &conv_id),
                )
            };

            SessionSummary {
                conversation_id: conv_id,
                transcript_path: p.display().to_string(),
                mtime_ms,
                size_bytes,
                is_subagent,
                parent_conversation_id,
                nesting_depth,
                agent_name,
                active,
                subagent_count: 0,
                preview: title.clone(),
                title,
                workspace,
            }
        })
        .collect();

    let mut counts: HashMap<String, usize> = HashMap::new();
    attach_workflow_children(&mut summaries, brain_dirs, now_ms);
    for s in &summaries {
        if s.is_subagent {
            let root = resolve_root_conversation_id(&s.conversation_id, &summaries);
            if root != s.conversation_id {
                *counts.entry(root).or_insert(0) += 1;
            }
        }
    }
    for s in &mut summaries {
        if let Some(&cnt) = counts.get(&s.conversation_id) {
            s.subagent_count = cnt;
        }
    }
    if summaries.len() > clamped_limit {
        // Preserve all linked descendants of kept roots when caller asked for a larger window,
        // otherwise trim to `clamped_limit` while guaranteeing at least one non-subagent root if present.
        let has_root_in_head = summaries
            .iter()
            .take(clamped_limit)
            .any(|s| !s.is_subagent);
        if !has_root_in_head {
            if let Some(root_pos) = summaries.iter().position(|s| !s.is_subagent) {
                summaries.swap(clamped_limit - 1, root_pos);
            }
        }
        summaries.truncate(clamped_limit);
    }
    summaries
}

pub fn resolve_root_conversation_id(conv_id: &str, sessions: &[SessionSummary]) -> String {
    let parent_by_id: HashMap<&str, Option<&str>> = sessions
        .iter()
        .map(|s| {
            (
                s.conversation_id.as_str(),
                s.parent_conversation_id.as_deref(),
            )
        })
        .collect();
    let mut cur = conv_id;
    let mut hops = 0usize;
    while hops < 16 {
        let Some(Some(parent)) = parent_by_id.get(cur) else {
            break;
        };
        if parent.is_empty() || *parent == cur {
            break;
        }
        cur = parent;
        hops += 1;
    }
    cur.to_string()
}

pub fn collect_descendant_sessions(root_id: &str, sessions: &[SessionSummary]) -> Vec<SessionSummary> {
    let mut out: Vec<SessionSummary> = sessions
        .iter()
        .filter(|s| {
            s.conversation_id != root_id
                && s.is_subagent
                && resolve_root_conversation_id(&s.conversation_id, sessions) == root_id
        })
        .cloned()
        .collect();
    out.sort_by(|a, b| {
        b.active
            .cmp(&a.active)
            .then_with(|| b.mtime_ms.cmp(&a.mtime_ms))
    });
    out
}

fn sniff_session_header(path: &Path, size_bytes: u64) -> (bool, Option<String>, String) {
    let Ok(file) = File::open(path) else {
        return (false, None, String::new());
    };
    let mut user_inputs = 0usize;
    let mut first_preview = String::new();
    let mut looks_delegated = false;
    let mut hosts_workflow = false;
    let mut explicit_is_subagent: Option<bool> = None;
    let mut explicit_parent: Option<String> = None;

    for line in BufReader::new(file).lines().take(24).map_while(Result::ok) {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(flag) = v.get("is_subagent").and_then(Value::as_bool) {
            explicit_is_subagent = Some(flag);
            if flag {
                looks_delegated = true;
            }
        }
        if explicit_parent.is_none() {
            if let Some(pid) = v
                .get("parent_conversation_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                explicit_parent = Some(pid.to_string());
                looks_delegated = true;
            }
        }
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "SYSTEM_MESSAGE" {
            let content = v.get("content").and_then(Value::as_str).unwrap_or("");
            if content.contains("<swarm_worker>") || content.contains("Your assignment (recorded on the board") {
                looks_delegated = true;
            }
        }
        if matches!(kind, "GENERIC" | "SYSTEM_MESSAGE" | "USER_INPUT") {
            let content = v.get("content").and_then(Value::as_str).unwrap_or("");
            if content.contains("Workflow \"") && content.contains("- [agent:") {
                hosts_workflow = true;
            }
        }
        if kind == "USER_INPUT" {
            user_inputs += 1;
            let content = v.get("content").and_then(Value::as_str).unwrap_or("").trim();
            if content.contains("call the finish tool with your result") {
                looks_delegated = true;
            }
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
    let is_sub = match explicit_is_subagent {
        Some(flag) => flag,
        None if hosts_workflow => false,
        None => looks_delegated || (user_inputs <= 1 && size_bytes < 12_000),
    };
    (is_sub, explicit_parent, first_preview)
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowChildLink {
    pub child_conversation_id: String,
    pub agent_type: String,
    pub role: String,
}

fn looks_like_uuid(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 36
        && bytes[8] == b'-'
        && bytes[13] == b'-'
        && bytes[18] == b'-'
        && bytes[23] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| matches!(i, 8 | 13 | 18 | 23) || b.is_ascii_hexdigit())
}

/// Parses `- [agent:<type>] <Role>: <status> (<uuid>)` lines emitted by Antigravity's
/// `run_workflow` tool steps in parent `transcript.jsonl` files.
pub fn extract_workflow_children_from_transcript(path: &Path) -> Vec<WorkflowChildLink> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for raw_line in BufReader::new(file).lines().map_while(Result::ok) {
        if !raw_line.contains("[agent:") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(raw_line.trim()) else {
            continue;
        };
        let step_type = v.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(step_type, "GENERIC" | "SYSTEM_MESSAGE" | "USER_INPUT") {
            continue;
        }
        let Some(content) = v.get("content").and_then(Value::as_str) else {
            continue;
        };
        if !content.contains("Workflow \"") {
            continue;
        }
        for line in content.lines() {
            let trimmed = line.trim();
            let Some(rest) = trimmed.strip_prefix("- [agent:") else {
                continue;
            };
            let Some((agent_type, after_bracket)) = rest.split_once(']') else {
                continue;
            };
            let after_bracket = after_bracket.trim();
            let mut cursor = after_bracket;
            let mut found_uuid: Option<(&str, &str)> = None;
            while let Some(open_idx) = cursor.find('(') {
                let candidate_after = &cursor[open_idx + 1..];
                if candidate_after.len() >= 37 && candidate_after.as_bytes()[36] == b')' {
                    let cand = &candidate_after[..36];
                    if looks_like_uuid(cand) {
                        let prefix_len = after_bracket.len() - cursor.len() + open_idx;
                        found_uuid = Some((&after_bracket[..prefix_len], cand));
                        break;
                    }
                }
                cursor = &cursor[open_idx + 1..];
            }
            let Some((before_paren, uuid)) = found_uuid else {
                continue;
            };
            if !seen.insert(uuid.to_string()) {
                continue;
            }
            let role = before_paren
                .rsplit_once(':')
                .map(|(r, _status)| r.trim())
                .unwrap_or_else(|| before_paren.trim());
            out.push(WorkflowChildLink {
                child_conversation_id: uuid.to_string(),
                agent_type: agent_type.trim().to_string(),
                role: role.to_string(),
            });
        }
    }
    out
}

fn attach_workflow_children(
    summaries: &mut Vec<SessionSummary>,
    brain_dirs: &[PathBuf],
    now_ms: u64,
) {
    let mut index_by_id: HashMap<String, usize> = summaries
        .iter()
        .enumerate()
        .map(|(i, s)| (s.conversation_id.clone(), i))
        .collect();

    let parent_candidates: Vec<(String, String, String, u32)> = summaries
        .iter()
        .filter(|s| !s.is_subagent || s.parent_conversation_id.is_some())
        .map(|s| {
            (
                s.conversation_id.clone(),
                s.transcript_path.clone(),
                s.workspace.clone(),
                s.nesting_depth,
            )
        })
        .collect();

    for (parent_id, parent_tpath, parent_ws, parent_depth) in parent_candidates {
        let links = extract_workflow_children_from_transcript(Path::new(&parent_tpath));
        if links.is_empty() {
            continue;
        }
        for link in links {
            if link.child_conversation_id == parent_id {
                continue;
            }
            let wf_agent = if link.agent_type.is_empty() {
                "wf:self".to_string()
            } else {
                format!("wf:{}", link.agent_type)
            };
            let clean_role = if !link.role.is_empty() {
                sanitize_session_title(&link.role, &parent_ws, &link.child_conversation_id)
            } else {
                format!(
                    "{} #{}",
                    wf_agent,
                    &link.child_conversation_id[..link.child_conversation_id.len().min(8)]
                )
            };

            if let Some(&idx) = index_by_id.get(&link.child_conversation_id) {
                let entry = &mut summaries[idx];
                entry.is_subagent = true;
                if entry.parent_conversation_id.is_none() {
                    entry.parent_conversation_id = Some(parent_id.clone());
                }
                entry.nesting_depth = entry.nesting_depth.max(parent_depth.saturating_add(1));
                entry.agent_name = wf_agent;
                if !link.role.is_empty() {
                    entry.title = clean_role.clone();
                    entry.preview = clean_role;
                }
                if entry.workspace.is_empty() {
                    entry.workspace = parent_ws.clone();
                }
                continue;
            }

            let Some(child_tpath) = brain_dirs
                .iter()
                .map(|b| {
                    b.join(&link.child_conversation_id)
                        .join(".system_generated")
                        .join("logs")
                        .join("transcript.jsonl")
                })
                .find(|p| p.is_file())
            else {
                continue;
            };
            let Ok(meta) = fs::metadata(&child_tpath) else {
                continue;
            };
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let active = now_ms.saturating_sub(mtime_ms) <= 15_000;
            let next_idx = summaries.len();
            summaries.push(SessionSummary {
                conversation_id: link.child_conversation_id.clone(),
                transcript_path: child_tpath.display().to_string(),
                mtime_ms,
                size_bytes: meta.len(),
                is_subagent: true,
                parent_conversation_id: Some(parent_id.clone()),
                nesting_depth: parent_depth.saturating_add(1),
                agent_name: wf_agent,
                active,
                subagent_count: 0,
                title: clean_role.clone(),
                preview: clean_role,
                workspace: parent_ws.clone(),
            });
            index_by_id.insert(link.child_conversation_id, next_idx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(id: &str, parent: Option<&str>, depth: u32, active: bool, mtime_ms: u64) -> SessionSummary {
        SessionSummary {
            conversation_id: id.into(),
            transcript_path: format!("/tmp/{}/transcript.jsonl", id),
            mtime_ms,
            size_bytes: 2048,
            is_subagent: depth > 0 || parent.is_some(),
            parent_conversation_id: parent.map(String::from),
            nesting_depth: depth,
            agent_name: if depth > 0 { "self".into() } else { String::new() },
            active,
            subagent_count: 0,
            title: id.into(),
            workspace: "demo".into(),
            preview: id.into(),
        }
    }

    #[test]
    fn resolves_nested_subagent_trees_and_guards_cycles() {
        let sessions = vec![
            mk("root-1", None, 0, false, 100),
            mk("child-a", Some("root-1"), 1, false, 150),
            mk("child-b", Some("root-1"), 1, true, 120),
            mk("grandchild", Some("child-a"), 2, false, 160),
            mk("cyc-a", Some("cyc-b"), 1, false, 90),
            mk("cyc-b", Some("cyc-a"), 1, false, 95),
        ];

        assert_eq!(resolve_root_conversation_id("grandchild", &sessions), "root-1");
        assert_eq!(resolve_root_conversation_id("root-1", &sessions), "root-1");
        let _ = resolve_root_conversation_id("cyc-a", &sessions);

        let kids = collect_descendant_sessions("root-1", &sessions);
        let ids: Vec<&str> = kids.iter().map(|s| s.conversation_id.as_str()).collect();
        assert_eq!(ids, vec!["child-b", "grandchild", "child-a"]);
    }

    #[test]
    fn session_summary_cache_respects_ttl_limit_and_dir_mtime_fingerprint() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let brain = dir.path().join("brain");
        let write_conv = |cid: &str| {
            let log_dir = brain.join(cid).join(".system_generated").join("logs");
            fs::create_dir_all(&log_dir).unwrap();
            let line = format!(
                "{{\"step_index\":1,\"type\":\"USER_INPUT\",\"created_at\":\"2026-11-20T10:00:00Z\",\"content\":\"Task {} {}\"}}\n",
                cid,
                "x".repeat(240)
            );
            fs::write(log_dir.join("transcript.jsonl"), line).unwrap();
        };
        write_conv("conv-alpha");
        write_conv("conv-beta");

        let cache: Mutex<Option<CachedSessionSummaries>> = Mutex::new(None);
        let db_loads = AtomicUsize::new(0);
        let brain_dirs = vec![brain.clone()];

        // 1. Cold scan with limit=10 populates cache
        let first = discover_session_summaries_cached(10, &brain_dirs, &cache, Duration::from_secs(4), || {
            db_loads.fetch_add(1, Ordering::SeqCst);
            HashMap::new()
        });
        assert_eq!(first.len(), 2);
        assert_eq!(db_loads.load(Ordering::SeqCst), 1);

        // 2. Smaller/equal limit within 4s TTL and unchanged brain dir mtime hits cache without rescanning
        let hit_slice = discover_session_summaries_cached(1, &brain_dirs, &cache, Duration::from_secs(4), || {
            db_loads.fetch_add(1, Ordering::SeqCst);
            HashMap::new()
        });
        assert_eq!(hit_slice.len(), 1);
        assert_eq!(db_loads.load(Ordering::SeqCst), 1, "expected cache hit for limit <= cached_limit");

        // 3. Larger limit > cached_limit misses and refreshes
        let larger = discover_session_summaries_cached(25, &brain_dirs, &cache, Duration::from_secs(4), || {
            db_loads.fetch_add(1, Ordering::SeqCst);
            HashMap::new()
        });
        assert_eq!(larger.len(), 2);
        assert_eq!(db_loads.load(Ordering::SeqCst), 2);

        // 4. Adding a new conversation directory updates `brain` mtime fingerprint -> invalidates immediately
        std::thread::sleep(Duration::from_millis(5));
        write_conv("conv-gamma");
        let after_new_dir =
            discover_session_summaries_cached(25, &brain_dirs, &cache, Duration::from_secs(4), || {
                db_loads.fetch_add(1, Ordering::SeqCst);
                HashMap::new()
            });
        assert_eq!(after_new_dir.len(), 3);
        assert_eq!(db_loads.load(Ordering::SeqCst), 3);

        // 5. Expired TTL forces refresh even with identical fingerprint
        std::thread::sleep(Duration::from_millis(8));
        let _ = discover_session_summaries_cached(10, &brain_dirs, &cache, Duration::from_millis(2), || {
            db_loads.fetch_add(1, Ordering::SeqCst);
            HashMap::new()
        });
        assert_eq!(db_loads.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn links_run_workflow_nodes_to_parent_and_keeps_them_out_of_top_level_list() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let brain = dir.path().join("brain");
        let root_id = "00000000-0000-4000-8000-000000000001";
        let wf1_id = "00000000-0000-4000-8000-000000000002";
        let wf2_id = "a1b2c3d4-1111-2222-3333-444455556666";

        let root_dir = brain.join(root_id).join(".system_generated").join("logs");
        fs::create_dir_all(&root_dir).unwrap();
        let root_log = format!(
            "{{\"step_index\":1,\"type\":\"USER_INPUT\",\"created_at\":\"2026-11-25T05:00:00Z\",\"content\":\"Audit routes in repo {}\"}}\n\
             {{\"step_index\":2,\"type\":\"GENERIC\",\"created_at\":\"2026-11-25T05:01:00Z\",\"content\":\"Workflow \\\"Audit\\\" completed.\\nLogs:\\n- [agent:research] Scan routes.rs: done ({}): {{}}\\nOutput:\\nok\"}}\n\
             {{\"step_index\":3,\"type\":\"SYSTEM_MESSAGE\",\"created_at\":\"2026-11-25T05:01:05Z\",\"content\":\"Task finished with result:\\nWorkflow \\\"Verify\\\" completed.\\nLogs:\\n- [agent:self] Verify routes.rs: done ({}): true\\nOutput:\\nok\"}}\n",
            "x".repeat(120),
            wf1_id,
            wf2_id
        );
        fs::write(root_dir.join("transcript.jsonl"), root_log).unwrap();

        for cid in [wf1_id, wf2_id] {
            let cdir = brain.join(cid).join(".system_generated").join("logs");
            fs::create_dir_all(&cdir).unwrap();
            let clog = format!(
                "{{\"step_index\":0,\"type\":\"USER_INPUT\",\"created_at\":\"2026-11-25T05:00:10Z\",\"content\":\"Audit file.\\n\\nWhen you are done, call the finish tool with your result. {}\"}}\n",
                "y".repeat(200)
            );
            fs::write(cdir.join("transcript.jsonl"), clog).unwrap();
        }

        // Even with limit=3 (or limit=1 for root discovery), workflow nodes link to root and stay out of top-level
        let summaries = scan_session_summaries(3, std::slice::from_ref(&brain), &HashMap::new());
        let top_level: Vec<&SessionSummary> = summaries.iter().filter(|s| !s.is_subagent).collect();
        assert_eq!(top_level.len(), 1);
        assert_eq!(top_level[0].conversation_id, root_id);
        assert_eq!(top_level[0].subagent_count, 2);

        let descendants = collect_descendant_sessions(root_id, &summaries);
        assert_eq!(descendants.len(), 2);
        assert!(descendants
            .iter()
            .any(|d| d.conversation_id == wf1_id && d.agent_name == "wf:research" && d.title == "Scan routes.rs"));
        assert!(descendants
            .iter()
            .any(|d| d.conversation_id == wf2_id && d.agent_name == "wf:self" && d.title == "Verify routes.rs"));

        // Verify limit=1 still surfaces the non-subagent root even when workflow children have newer mtimes
        let head_one = scan_session_summaries(1, &[brain], &HashMap::new());
        assert_eq!(head_one.len(), 1);
        assert!(!head_one[0].is_subagent);
        assert_eq!(head_one[0].conversation_id, root_id);
    }
}
