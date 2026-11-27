use crate::namer::{
    load_session_registry, resolve_session_identity, ResolvedIdentity, SessionRegistry,
    ALLOWED_TITLE_EMOJIS,
};
use crate::radar::{enrich_unified_sessions_live_state, SessionLiveState};
use crate::sessions::{decode_workspace_uris, sanitize_session_title};
use crate::transcript::parse_iso8601_epoch_ms;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSearchQuery {
    #[serde(default)]
    pub query: String,
    #[serde(default = "default_client_filter")]
    pub client: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub days: u32,
    #[serde(default = "default_sort_mode")]
    pub sort: String,
    #[serde(default = "default_result_limit")]
    pub limit: usize,
}

fn default_client_filter() -> String {
    "all".to_string()
}

fn default_sort_mode() -> String {
    "updated".to_string()
}

fn default_result_limit() -> usize {
    150
}

impl Default for SessionSearchQuery {
    fn default() -> Self {
        Self {
            query: String::new(),
            client: default_client_filter(),
            group: String::new(),
            days: 0,
            sort: default_sort_mode(),
            limit: default_result_limit(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedSessionItem {
    pub id: String,
    pub client: String,
    pub title: String,
    pub snippet: String,
    pub workspace: String,
    pub group: String,
    pub code: String,
    #[serde(default)]
    pub tag: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub active: bool,
    #[serde(default)]
    pub live_state: SessionLiveState,
    #[serde(default)]
    pub open_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGroupFacet {
    pub group: String,
    pub code: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSearchResponse {
    pub ok: bool,
    pub query: String,
    pub client_filter: String,
    pub group_filter: String,
    pub days_filter: u32,
    pub sort: String,
    pub total_matches: usize,
    pub agy_matches: usize,
    pub codex_matches: usize,
    pub elapsed_ms: f64,
    pub groups: Vec<SessionGroupFacet>,
    pub sessions: Vec<UnifiedSessionItem>,
}

fn now_epoch_ms_u64() -> u64 {
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
    // Antigravity SQLite stores `2026-11-01 14:45:04.476678+00:00` (space separator).
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

fn sql_escape_like_term(term: &str) -> String {
    let mut out = String::with_capacity(term.len() + 4);
    for c in term.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            '%' => out.push_str("\\%"),
            '_' => out.push_str("\\_"),
            '\0' | '\n' | '\r' | '\t' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

fn clean_snippet_text(raw: &str) -> String {
    let stripped = raw
        .replace("<USER_REQUEST>", " ")
        .replace("</USER_REQUEST>", " ")
        .replace("<CONTEXT_SUMMARY>", " ")
        .replace("</CONTEXT_SUMMARY>", " ")
        .replace("<SYSTEM_MESSAGE>", " ")
        .replace("</SYSTEM_MESSAGE>", " ")
        .replace(['\n', '\r', '\t'], " ");
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn extract_match_snippet(text: &str, terms: &[String], max_chars: usize) -> String {
    let clean = clean_snippet_text(text);
    if clean.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = clean.chars().collect();
    if chars.len() <= max_chars {
        return clean;
    }
    if terms.is_empty() {
        let mut head: String = chars.into_iter().take(max_chars).collect();
        head.push('…');
        return head;
    }

    let lower_clean = clean.to_lowercase();
    let mut first_char_pos: Option<usize> = None;
    for t in terms {
        if t.is_empty() {
            continue;
        }
        if let Some(byte_idx) = lower_clean.find(t.as_str()) {
            let char_idx = lower_clean[..byte_idx].chars().count();
            first_char_pos = Some(first_char_pos.map_or(char_idx, |cur| cur.min(char_idx)));
        }
    }

    let anchor = first_char_pos.unwrap_or(0);
    let context_before = max_chars / 3;
    let start = anchor.saturating_sub(context_before);
    let end = (start + max_chars).min(chars.len());
    let adj_start = end.saturating_sub(max_chars);

    let mut out = String::new();
    if adj_start > 0 {
        out.push('…');
    }
    out.extend(chars[adj_start..end].iter().copied());
    if end < chars.len() {
        out.push('…');
    }
    out
}

fn resolve_agy_created_ms(
    cid: &str,
    brain_dirs: &[PathBuf],
    fallback_input_ms: u64,
    fallback_mod_ms: u64,
) -> u64 {
    static CREATED_CACHE: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    let cache = CREATED_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock() {
        if let Some(&ts) = guard.get(cid) {
            if ts > 0 {
                return ts;
            }
        }
    }

    let mut resolved = 0u64;
    for bdir in brain_dirs {
        let conv_dir = bdir.join(cid);
        let tpath = conv_dir
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        if let Ok(mut f) = File::open(&tpath) {
            let mut head = [0u8; 768];
            if let Ok(n) = f.read(&mut head) {
                let s = String::from_utf8_lossy(&head[..n]);
                if let Some(first_line) = s.lines().find(|l| l.trim().starts_with('{')) {
                    if let Ok(v) = serde_json::from_str::<Value>(first_line.trim()) {
                        if let Some(ct) = v.get("created_at").and_then(Value::as_str) {
                            if let Some(ms) = parse_iso8601_epoch_ms(ct) {
                                if ms > 0 {
                                    resolved = ms as u64;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        if resolved == 0 {
            if let Ok(meta) = fs::metadata(&conv_dir) {
                if let Ok(created) = meta.created() {
                    if let Ok(d) = created.duration_since(UNIX_EPOCH) {
                        resolved = d.as_millis() as u64;
                        break;
                    }
                }
            }
        }
    }

    if resolved == 0 {
        resolved = if fallback_input_ms > 0 && fallback_input_ms <= fallback_mod_ms {
            fallback_input_ms
        } else {
            fallback_mod_ms
        };
    }

    if resolved > 0 {
        if let Ok(mut guard) = cache.lock() {
            if guard.len() < 4096 {
                guard.insert(cid.to_string(), resolved);
            }
        }
    }
    resolved
}

fn is_ephemeral_bench_or_workflow_noise(
    title: &str,
    preview: &str,
    workspaces: &[String],
) -> bool {
    if workspaces
        .iter()
        .any(|w| w.contains("/agy_hyper_ab_") || w.contains("/agy-hyper-namer-"))
    {
        return true;
    }
    let p = preview.trim();
    if p.starts_with("You are a deterministic session title editor.") {
        return true;
    }
    if title.trim().is_empty() && p.starts_with("Audit ") && p.contains(" ONLY for this objective")
    {
        return true;
    }
    false
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

fn apply_identity_labels(
    identity: &ResolvedIdentity,
    fallback_ws: &str,
) -> (String, String, Option<String>) {
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
    (group, code, identity.tag.clone())
}

fn clean_codex_title(raw: &str, id: &str) -> String {
    let mut s = raw.replace(['\n', '\r', '\t'], " ");
    // Strip leading `[$skill](/path/to/SKILL.md)` markdown prefix when `threads.name` was empty.
    if s.trim_start().starts_with("[$") {
        if let Some(close_paren) = s.find(')') {
            let rest = s[close_paren + 1..].trim();
            if !rest.is_empty() {
                s = rest.to_string();
            }
        }
    }
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        format!("Thread #{}", &id[..id.len().min(8)])
    } else {
        collapsed.chars().take(64).collect()
    }
}

pub fn search_sessions_with_paths(
    req: &SessionSearchQuery,
    agy_db_path: Option<&Path>,
    brain_dirs: &[PathBuf],
    codex_db_path: Option<&Path>,
    registry: Option<&SessionRegistry>,
    home_dir: Option<&Path>,
) -> SessionSearchResponse {
    let t0 = Instant::now();
    let client_norm = match req.client.trim().to_ascii_lowercase().as_str() {
        "agy" | "antigravity" => "agy",
        "codex" => "codex",
        _ => "all",
    };
    let sort_norm = match req.sort.trim().to_ascii_lowercase().as_str() {
        "created" | "create" => "created",
        _ => "updated",
    };
    let limit = req.limit.clamp(1, 500);
    let terms: Vec<String> = req
        .query
        .split_whitespace()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();

    let mut raw_items: Vec<UnifiedSessionItem> = Vec::new();

    if client_norm == "all" || client_norm == "agy" {
        let mut seen_agy: HashSet<String> = HashSet::new();
        if let Some(db) = agy_db_path.filter(|p| p.is_file()) {
            let db_items = query_agy_sqlite(db, &terms, brain_dirs, registry, home_dir);
            for item in db_items {
                seen_agy.insert(item.id.clone());
                raw_items.push(item);
            }
        }
        // Supplement with transcript scan when DB is absent, or strictly for brand-new (<120s) unflushed sessions.
        let has_db = agy_db_path.is_some_and(|p| p.is_file());
        let max_fallback_age_ms = if has_db { Some(120_000u64) } else { None };
        if !has_db || !terms.is_empty() {
            let fs_items = scan_agy_transcripts_fallback(
                brain_dirs,
                &seen_agy,
                &terms,
                registry,
                home_dir,
                max_fallback_age_ms,
            );
            raw_items.extend(fs_items);
        }
    }

    if client_norm == "all" || client_norm == "codex" {
        if let Some(cdb) = codex_db_path.filter(|p| p.is_file()) {
            let codex_items = query_codex_sqlite(cdb, &terms, registry, home_dir);
            raw_items.extend(codex_items);
        }
    }

    // Apply time-window filter (`days > 0`) BEFORE computing group facets so pill counts stay accurate.
    if req.days > 0 {
        let cutoff_ms =
            now_epoch_ms_u64().saturating_sub(u64::from(req.days).saturating_mul(86_400_000));
        raw_items.retain(|it| {
            let ts = if sort_norm == "created" {
                it.created_at_ms
            } else {
                it.updated_at_ms
            };
            ts >= cutoff_ms
        });
    }

    // Aggregate dynamic business-group pills (`group` + `code` + `count`)
    let mut group_counts: HashMap<String, (String, usize)> = HashMap::new();
    for it in &raw_items {
        let entry = group_counts
            .entry(it.group.clone())
            .or_insert_with(|| (it.code.clone(), 0));
        entry.1 += 1;
    }
    let mut groups: Vec<SessionGroupFacet> = group_counts
        .into_iter()
        .map(|(group, (code, count))| SessionGroupFacet { group, code, count })
        .collect();
    groups.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.group.cmp(&b.group)));

    // Apply optional `group` filter (matches either `group` or short `code`, case-insensitively).
    let group_filter = req.group.trim();
    if !group_filter.is_empty() && !group_filter.eq_ignore_ascii_case("all") {
        let gf_lower = group_filter.to_lowercase();
        raw_items.retain(|it| {
            it.group.to_lowercase() == gf_lower || it.code.to_lowercase() == gf_lower
        });
    }

    let agy_matches = raw_items.iter().filter(|i| i.client == "agy").count();
    let codex_matches = raw_items.iter().filter(|i| i.client == "codex").count();
    let total_matches = raw_items.len();

    if sort_norm == "created" {
        raw_items.sort_by(|a, b| {
            b.created_at_ms
                .cmp(&a.created_at_ms)
                .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
        });
    } else {
        raw_items.sort_by(|a, b| {
            b.updated_at_ms
                .cmp(&a.updated_at_ms)
                .then_with(|| b.created_at_ms.cmp(&a.created_at_ms))
        });
    }
    raw_items.truncate(limit);
    enrich_unified_sessions_live_state(
        &mut raw_items,
        agy_db_path,
        brain_dirs,
        codex_db_path,
        home_dir,
    );

    let elapsed_ms = (t0.elapsed().as_secs_f64() * 1000.0 * 100.0).round() / 100.0;
    SessionSearchResponse {
        ok: true,
        query: req.query.trim().to_string(),
        client_filter: client_norm.to_string(),
        group_filter: group_filter.to_string(),
        days_filter: req.days,
        sort: sort_norm.to_string(),
        total_matches,
        agy_matches,
        codex_matches,
        elapsed_ms,
        groups,
        sessions: raw_items,
    }
}

pub fn search_sessions(req: &SessionSearchQuery) -> SessionSearchResponse {
    let home = dirs::home_dir();
    let agy_db = std::env::var("AGY_HYPER_SUMMARIES_DB")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let h = home.as_ref()?;
            ["antigravity", "antigravity-ide", "antigravity-cli"]
                .into_iter()
                .map(|p| h.join(".gemini").join(p).join("conversation_summaries.db"))
                .find(|p| p.is_file())
        });

    let brain_dirs: Vec<PathBuf> = if let Ok(custom) = std::env::var("AGY_HYPER_BRAIN_DIR") {
        let t = custom.trim();
        if t.is_empty() {
            Vec::new()
        } else {
            vec![PathBuf::from(t)]
        }
    } else if let Some(h) = home.as_ref() {
        ["antigravity", "antigravity-ide", "antigravity-cli"]
            .into_iter()
            .map(|p| h.join(".gemini").join(p).join("brain"))
            .collect()
    } else {
        Vec::new()
    };

    let codex_db = std::env::var("AGY_HYPER_CODEX_DB")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".codex").join("state_5.sqlite")));

    let registry = load_session_registry();
    search_sessions_with_paths(
        req,
        agy_db.as_deref(),
        &brain_dirs,
        codex_db.as_deref(),
        registry.as_ref(),
        home.as_deref(),
    )
}

fn run_sqlite_tsv(db_path: &Path, sql: &str) -> Option<String> {
    match Command::new("sqlite3")
        .args(["-readonly", "-cmd", ".timeout 800", "-separator", "\t"])
        .arg(db_path)
        .arg(sql)
        .output()
    {
        Ok(out) if out.status.success() => {
            return Some(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        Ok(_) => return None,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }

    // Fallback for hosts (such as Windows) without the `sqlite3` CLI in PATH.
    let py_script = concat!(
        "import sqlite3, sys\n",
        "sys.stdout.reconfigure(encoding='utf-8')\n",
        "con = sqlite3.connect(sys.argv[1], timeout=0.8)\n",
        "cur = con.execute(sys.argv[2])\n",
        "for row in cur:\n",
        "    print('\\t'.join('' if v is None else str(v) for v in row))\n",
        "con.close()\n"
    );
    for bin in ["python3", "python"] {
        if let Ok(out) = Command::new(bin)
            .env("PYTHONUTF8", "1")
            .arg("-c")
            .arg(py_script)
            .arg(db_path)
            .arg(sql)
            .output()
        {
            if out.status.success() {
                return Some(String::from_utf8_lossy(&out.stdout).into_owned());
            }
        }
    }
    None
}

fn query_agy_sqlite(
    db_path: &Path,
    terms: &[String],
    brain_dirs: &[PathBuf],
    registry: Option<&SessionRegistry>,
    home_dir: Option<&Path>,
) -> Vec<UnifiedSessionItem> {
    let base_where = "s.nesting_depth = 0 AND coalesce(s.parent_conversation_id, '') = '' AND coalesce(p.parent_id, '') = '' AND coalesce(s.agent_name, '') = '' AND coalesce(s.killed, 0) = 0 AND coalesce(s.project_id, '') != 'default-cli-project' AND (s.step_count > 0 OR s.title != '' OR s.preview != '')";

    let (sql, has_fts_snippet) = if terms.is_empty() {
        (
            format!(
                "SELECT s.conversation_id, \
                        replace(replace(coalesce(s.title, ''), char(9), ' '), char(10), ' '), \
                        replace(replace(coalesce(s.preview, ''), char(9), ' '), char(10), ' '), \
                        replace(coalesce(s.workspace_uris, ''), char(9), ' '), \
                        coalesce(s.project_id, ''), \
                        coalesce(s.last_modified_time, ''), \
                        coalesce(s.last_user_input_time, ''), \
                        coalesce(s.not_fully_idle, 0), \
                        '' \
                 FROM conversation_summaries s \
                 LEFT JOIN conversation_parents p ON p.conversation_id = s.conversation_id \
                 WHERE {} \
                 ORDER BY s.last_modified_time DESC LIMIT 400;",
                base_where
            ),
            false,
        )
    } else {
        let mut clauses = Vec::with_capacity(terms.len());
        for t in terms {
            let esc = sql_escape_like_term(t);
            clauses.push(format!(
                "(lower(s.title) LIKE '%{e}%' ESCAPE '\\' \
                  OR lower(s.preview) LIKE '%{e}%' ESCAPE '\\' \
                  OR lower(s.workspace_uris) LIKE '%{e}%' ESCAPE '\\' \
                  OR EXISTS (SELECT 1 FROM conversations_fts_content f WHERE f.c0 = s.conversation_id AND (\
                      lower(coalesce(f.c10,'')) LIKE '%{e}%' ESCAPE '\\' \
                      OR lower(coalesce(f.c11,'')) LIKE '%{e}%' ESCAPE '\\' \
                      OR lower(coalesce(f.c12,'')) LIKE '%{e}%' ESCAPE '\\' \
                      OR lower(coalesce(f.c13,'')) LIKE '%{e}%' ESCAPE '\\' \
                      OR lower(coalesce(f.c14,'')) LIKE '%{e}%' ESCAPE '\\')))",
                e = esc
            ));
        }
        let first_esc = sql_escape_like_term(&terms[0]);
        let snippet_expr = format!(
            "replace(replace(coalesce((SELECT CASE \
                WHEN lower(coalesce(f.c13,'')) LIKE '%{e}%' ESCAPE '\\' THEN substr(f.c13, max(1, instr(lower(f.c13), '{raw}') - 40), 180) \
                WHEN lower(coalesce(f.c14,'')) LIKE '%{e}%' ESCAPE '\\' THEN substr(f.c14, max(1, instr(lower(f.c14), '{raw}') - 40), 180) \
                ELSE coalesce(f.c10, '') END \
              FROM conversations_fts_content f \
              WHERE f.c0 = s.conversation_id AND (\
                lower(coalesce(f.c13,'')) LIKE '%{e}%' ESCAPE '\\' \
                OR lower(coalesce(f.c14,'')) LIKE '%{e}%' ESCAPE '\\' \
                OR lower(coalesce(f.c10,'')) LIKE '%{e}%' ESCAPE '\\') LIMIT 1), ''), char(9), ' '), char(10), ' ')",
            e = first_esc,
            raw = terms[0].replace('\'', "''")
        );
        (
            format!(
                "SELECT s.conversation_id, \
                        replace(replace(coalesce(s.title, ''), char(9), ' '), char(10), ' '), \
                        replace(replace(coalesce(s.preview, ''), char(9), ' '), char(10), ' '), \
                        replace(coalesce(s.workspace_uris, ''), char(9), ' '), \
                        coalesce(s.project_id, ''), \
                        coalesce(s.last_modified_time, ''), \
                        coalesce(s.last_user_input_time, ''), \
                        coalesce(s.not_fully_idle, 0), \
                        {} \
                 FROM conversation_summaries s \
                 LEFT JOIN conversation_parents p ON p.conversation_id = s.conversation_id \
                 WHERE {} AND {} \
                 ORDER BY s.last_modified_time DESC LIMIT 400;",
                snippet_expr,
                base_where,
                clauses.join(" AND ")
            ),
            true,
        )
    };

    let Some(stdout) = run_sqlite_tsv(db_path, &sql) else {
        return Vec::new();
    };

    let mut items = Vec::new();
    for line in stdout.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 8 {
            continue;
        }
        let cid = cols[0].trim();
        if cid.is_empty() {
            continue;
        }
        let raw_title = cols[1].trim();
        let raw_preview = cols[2].trim();
        let workspaces = decode_workspace_uris(cols[3].trim());
        if is_ephemeral_bench_or_workflow_noise(raw_title, raw_preview, &workspaces) {
            continue;
        }
        let project_id = cols[4].trim();
        let updated_ms = parse_sqlite_datetime_ms(cols[5]);
        let input_ms = parse_sqlite_datetime_ms(cols[6]);
        let active = cols[7].trim() == "1";
        let fts_excerpt = if has_fts_snippet && cols.len() >= 9 {
            cols[8].trim()
        } else {
            ""
        };

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

        let identity = resolve_identity_from_title_prefix(registry, raw_title).unwrap_or_else(|| {
            let base_text = format!("{} {}", raw_title, raw_preview);
            let text_for_identity = if base_text.trim().is_empty() {
                fts_excerpt.to_string()
            } else {
                base_text
            };
            resolve_session_identity(
                registry,
                &workspaces,
                &text_for_identity,
                project_id,
                home_dir,
            )
        });
        let (group, code, tag) = apply_identity_labels(&identity, &ws_basename);

        let display_title = if !raw_title.is_empty() {
            raw_title.to_string()
        } else {
            sanitize_session_title(raw_preview, &ws_basename, cid)
        };
        let snippet_source = if !fts_excerpt.is_empty() {
            fts_excerpt
        } else {
            raw_preview
        };
        let snippet = extract_match_snippet(snippet_source, terms, 92);
        let created_ms = resolve_agy_created_ms(cid, brain_dirs, input_ms, updated_ms);

        items.push(UnifiedSessionItem {
            id: cid.to_string(),
            client: "agy".to_string(),
            title: display_title,
            snippet,
            workspace: ws_basename,
            group,
            code,
            tag,
            created_at_ms: created_ms,
            updated_at_ms: updated_ms.max(created_ms),
            active,
            live_state: if active {
                SessionLiveState::Running
            } else {
                SessionLiveState::Idle
            },
            open_url: None,
        });
    }
    items
}

fn scan_agy_transcripts_fallback(
    brain_dirs: &[PathBuf],
    already_seen: &HashSet<String>,
    terms: &[String],
    registry: Option<&SessionRegistry>,
    home_dir: Option<&Path>,
    max_age_ms: Option<u64>,
) -> Vec<UnifiedSessionItem> {
    let now_ms = now_epoch_ms_u64();
    let mut items = Vec::new();
    for bdir in brain_dirs {
        let Ok(entries) = fs::read_dir(bdir) else {
            continue;
        };
        for ent in entries.flatten() {
            let conv_dir = ent.path();
            let Some(cid) = conv_dir.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if already_seen.contains(cid) {
                continue;
            }
            let tpath = conv_dir
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl");
            let Ok(meta) = fs::metadata(&tpath) else {
                continue;
            };
            if meta.len() < 64 {
                continue;
            }
            if let Some(max_age) = max_age_ms {
                let mtime_ms = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                if now_ms.saturating_sub(mtime_ms) > max_age {
                    continue;
                }
            }
            let Ok(file) = File::open(&tpath) else {
                continue;
            };

            let mut is_sub = false;
            let mut first_user = String::new();
            let mut created_ms = 0u64;
            let mut matched_terms: Vec<bool> = vec![false; terms.len()];
            let mut match_line = String::new();

            for line in BufReader::new(file).lines().take(400).map_while(Result::ok) {
                let trimmed = line.trim();
                if !trimmed.starts_with('{') {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
                    continue;
                };
                if v.get("is_subagent").and_then(Value::as_bool) == Some(true)
                    || v.get("parent_conversation_id")
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.trim().is_empty())
                {
                    is_sub = true;
                    break;
                }
                let typ = v.get("type").and_then(Value::as_str).unwrap_or("");
                if created_ms == 0 {
                    if let Some(ct) = v.get("created_at").and_then(Value::as_str) {
                        if let Some(ms) = parse_iso8601_epoch_ms(ct) {
                            created_ms = ms.max(0) as u64;
                        }
                    }
                }
                let text = v
                    .get("content")
                    .and_then(Value::as_str)
                    .or_else(|| v.get("response").and_then(Value::as_str))
                    .unwrap_or("");
                if typ == "SYSTEM_MESSAGE"
                    && (text.contains("<swarm_worker>")
                        || text.contains("Your assignment (recorded on the board"))
                {
                    is_sub = true;
                    break;
                }
                if typ == "USER_INPUT" && first_user.is_empty() && !text.is_empty() {
                    if text.contains("call the finish tool with your result")
                        || text.contains("You are a deterministic session title editor.")
                    {
                        is_sub = true;
                        break;
                    }
                    first_user = clean_snippet_text(text);
                }
                if !terms.is_empty() && matches!(typ, "USER_INPUT" | "PLANNER_RESPONSE") {
                    let lower = text.to_lowercase();
                    for (idx, term) in terms.iter().enumerate() {
                        if lower.contains(term.as_str()) {
                            matched_terms[idx] = true;
                            if match_line.is_empty() {
                                match_line = text.to_string();
                            }
                        }
                    }
                }
            }

            if is_sub || first_user.is_empty() {
                continue;
            }
            if !terms.is_empty() && !matched_terms.iter().all(|&m| m) {
                continue;
            }

            let updated_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let final_created = if created_ms > 0 {
                created_ms
            } else {
                updated_ms
            };
            let identity = resolve_session_identity(registry, &[], &first_user, "", home_dir);
            let (group, code, tag) = apply_identity_labels(&identity, "");
            let title = sanitize_session_title(&first_user, "", cid);
            let snippet = extract_match_snippet(
                if !match_line.is_empty() {
                    &match_line
                } else {
                    &first_user
                },
                terms,
                92,
            );

            items.push(UnifiedSessionItem {
                id: cid.to_string(),
                client: "agy".to_string(),
                title,
                snippet,
                workspace: String::new(),
                group,
                code,
                tag,
                created_at_ms: final_created,
                updated_at_ms: updated_ms.max(final_created),
                active: false,
                live_state: SessionLiveState::Idle,
                open_url: None,
            });
        }
    }
    items
}

fn query_codex_sqlite(
    db_path: &Path,
    terms: &[String],
    registry: Option<&SessionRegistry>,
    home_dir: Option<&Path>,
) -> Vec<UnifiedSessionItem> {
    let mut where_parts = vec![
        "archived = 0".to_string(),
        "attached_thread_parent_id IS NULL".to_string(),
        "source IN ('vscode', 'cli')".to_string(),
    ];
    for t in terms {
        let esc = sql_escape_like_term(t);
        where_parts.push(format!(
            "(lower(coalesce(name,'')) LIKE '%{e}%' ESCAPE '\\' \
              OR lower(coalesce(title,'')) LIKE '%{e}%' ESCAPE '\\' \
              OR lower(coalesce(first_user_message,'')) LIKE '%{e}%' ESCAPE '\\' \
              OR lower(coalesce(preview,'')) LIKE '%{e}%' ESCAPE '\\' \
              OR lower(coalesce(cwd,'')) LIKE '%{e}%' ESCAPE '\\')",
            e = esc
        ));
    }

    let sql = format!(
        "SELECT id, \
                replace(replace(coalesce(nullif(trim(name), ''), nullif(trim(title), ''), ''), char(9), ' '), char(10), ' '), \
                replace(coalesce(cwd, ''), char(9), ' '), \
                CASE WHEN coalesce(created_at_ms, 0) > 0 THEN created_at_ms ELSE coalesce(created_at, 0) * 1000 END, \
                CASE WHEN coalesce(updated_at_ms, 0) > 0 THEN updated_at_ms ELSE coalesce(updated_at, 0) * 1000 END, \
                replace(replace(substr(coalesce(nullif(trim(preview), ''), nullif(trim(first_user_message), ''), ''), 1, 320), char(9), ' '), char(10), ' ') \
         FROM threads \
         WHERE {} \
         ORDER BY updated_at DESC LIMIT 450;",
        where_parts.join(" AND ")
    );

    let Some(stdout) = run_sqlite_tsv(db_path, &sql) else {
        return Vec::new();
    };

    let mut items = Vec::new();
    for line in stdout.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 6 {
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
        let raw_preview = cols[5].trim();

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
        let identity = resolve_identity_from_title_prefix(registry, raw_title).unwrap_or_else(|| {
            let combined_text = format!("{} {}", raw_title, raw_preview);
            resolve_session_identity(registry, &ws_vec, &combined_text, "", home_dir)
        });
        let (group, code, tag) = apply_identity_labels(&identity, ws_basename);

        let title = clean_codex_title(raw_title, id);
        let snippet = extract_match_snippet(raw_preview, terms, 92);

        items.push(UnifiedSessionItem {
            id: id.to_string(),
            client: "codex".to_string(),
            title,
            snippet,
            workspace: ws_basename.to_string(),
            group,
            code,
            tag,
            created_at_ms: created_ms,
            updated_at_ms: updated_ms.max(created_ms),
            active: false,
            live_state: SessionLiveState::Idle,
            open_url: Some(format!("codex://threads/{}", id)),
        });
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::namer::SessionRegistryRule;
    use tempfile::tempdir;

    fn seed_sqlite_for_test(db_path: &Path, ddl: &str) {
        if let Ok(st) = Command::new("sqlite3").arg(db_path).arg(ddl).status() {
            assert!(st.success(), "sqlite3 DDL seed failed");
            return;
        }
        let py_seed = "import sqlite3, sys; con = sqlite3.connect(sys.argv[1]); con.executescript(sys.argv[2]); con.commit(); con.close()";
        for bin in ["python3", "python"] {
            if let Ok(st) = Command::new(bin)
                .env("PYTHONUTF8", "1")
                .arg("-c")
                .arg(py_seed)
                .arg(db_path)
                .arg(ddl)
                .status()
            {
                if st.success() {
                    return;
                }
            }
        }
        panic!("neither sqlite3 nor python sqlite3 available to seed test DB");
    }

    #[test]
    fn cross_client_cjk_search_achieves_full_recall_and_resolves_codex_name_priority() {
        let dir = tempdir().unwrap();
        let agy_db = dir.path().join("conversation_summaries.db");
        let codex_db = dir.path().join("state_5.sqlite");
        let brain = dir.path().join("brain");
        fs::create_dir_all(&brain).unwrap();

        // Create temporary Antigravity summary + FTS content tables with mid-clause CJK terms
        let init_agy = "\
            CREATE TABLE conversation_summaries (\
                conversation_id TEXT PRIMARY KEY, title TEXT NOT NULL DEFAULT '', preview TEXT NOT NULL DEFAULT '',\
                step_count INTEGER NOT NULL DEFAULT 1, last_modified_time DATETIME NOT NULL, workspace_uris TEXT NOT NULL,\
                project_id TEXT NOT NULL DEFAULT '', agent_name TEXT NOT NULL DEFAULT '', parent_conversation_id TEXT NOT NULL DEFAULT '',\
                nesting_depth INTEGER NOT NULL DEFAULT 0, not_fully_idle NUMERIC NOT NULL DEFAULT 0, killed NUMERIC NOT NULL DEFAULT 0,\
                last_user_input_time DATETIME NOT NULL\
            );\
            CREATE TABLE conversation_parents (conversation_id TEXT PRIMARY KEY, parent_id TEXT NOT NULL);\
            CREATE TABLE conversations_fts_content (id INTEGER PRIMARY KEY, c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11, c12, c13, c14);\
            INSERT INTO conversation_summaries VALUES ('agy-1', 'Generic English Title', '/wc-local', 8, '2026-11-26 10:00:00+00:00', '[\"file:///work/pos-app\"]', 'p1', '', '', 0, 0, 0, '2026-11-26 09:00:00+00:00');\
            INSERT INTO conversations_fts_content (id, c0, c10, c13, c14) VALUES (1, 'agy-1', 'Generic English Title', '门店后厨小票打印机突然无法出单，同时核对昨日提现记录', '已定位蓝牙网关队列阻塞');\
            INSERT INTO conversation_summaries VALUES ('sub-ignored', 'Worker', 'task', 2, '2026-11-26 10:05:00+00:00', '[]', 'p1', 'research', 'agy-1', 1, 0, 0, '2026-11-26 10:01:00+00:00');\
        ";
        seed_sqlite_for_test(&agy_db, init_agy);

        // Create temporary Codex state_5.sqlite testing COALESCE(NULLIF(name,''), NULLIF(title,''), '')
        let init_codex = "\
            CREATE TABLE threads (\
                id TEXT PRIMARY KEY, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,\
                source TEXT NOT NULL, cwd TEXT NOT NULL, title TEXT NOT NULL, archived INTEGER NOT NULL DEFAULT 0,\
                first_user_message TEXT NOT NULL DEFAULT '', created_at_ms INTEGER, updated_at_ms INTEGER,\
                preview TEXT NOT NULL DEFAULT '', name TEXT, attached_thread_parent_id TEXT\
            );\
            INSERT INTO threads VALUES ('cdx-1', 1764140000, 1764150000, 'vscode', '/work/pos-app', 'Raw multi-line prompt that should be shadowed', 0, '检查热敏打印机驱动', 1764140000000, 1764150000000, '检查热敏打印机驱动与固件升级', '🧩 收银·热敏打印｜驱动修复', NULL);\
            INSERT INTO threads VALUES ('cdx-exec-ignored', 1764140000, 1764155000, 'exec', '/work/pos-app', 'Headless exec', 0, '打印机', 1764140000000, 1764155000000, '打印机', '', NULL);\
        ";
        seed_sqlite_for_test(&codex_db, init_codex);

        let reg = SessionRegistry {
            version: 1,
            default_code: "通用".into(),
            rules: vec![SessionRegistryRule {
                id: "pos".into(),
                code: "收银".into(),
                tag: "biz:pos".into(),
                group: "门店收银".into(),
                path_prefixes: vec!["/work/pos-app".into()],
                keywords: vec![],
            }],
        };

        let resp = search_sessions_with_paths(
            &SessionSearchQuery {
                query: "打印机".into(),
                client: "all".into(),
                ..Default::default()
            },
            Some(&agy_db),
            std::slice::from_ref(&brain),
            Some(&codex_db),
            Some(&reg),
            Some(Path::new("/tmp/fake-home")),
        );

        assert!(resp.ok);
        assert_eq!(resp.agy_matches, 1);
        assert_eq!(resp.codex_matches, 1);
        assert_eq!(resp.total_matches, 2);
        assert_eq!(resp.groups.len(), 1);
        assert_eq!(resp.groups[0].group, "门店收银");
        assert_eq!(resp.groups[0].count, 2);

        let codex_item = resp.sessions.iter().find(|s| s.client == "codex").unwrap();
        assert_eq!(codex_item.title, "🧩 收银·热敏打印｜驱动修复");
        assert_eq!(
            codex_item.open_url.as_deref(),
            Some("codex://threads/cdx-1")
        );

        let agy_item = resp.sessions.iter().find(|s| s.client == "agy").unwrap();
        assert!(agy_item.snippet.contains("打印机"));
    }

    #[test]
    fn filters_cli_namer_noise_and_stabilizes_home_group_via_title_prefix() {
        let dir = tempdir().unwrap();
        let fake_home = dir.path().join("home");
        fs::create_dir_all(&fake_home).unwrap();
        let agy_db = dir.path().join("conversation_summaries.db");
        let brain = dir.path().join("brain");
        fs::create_dir_all(&brain).unwrap();

        let home_uri = serde_json::to_string(&vec![format!("file://{}", fake_home.display())])
            .unwrap()
            .replace('\'', "''");
        let init_agy = format!(
            "\
            CREATE TABLE conversation_summaries (\
                conversation_id TEXT PRIMARY KEY, title TEXT NOT NULL DEFAULT '', preview TEXT NOT NULL DEFAULT '',\
                step_count INTEGER NOT NULL DEFAULT 1, last_modified_time DATETIME NOT NULL, workspace_uris TEXT NOT NULL,\
                project_id TEXT NOT NULL DEFAULT '', agent_name TEXT NOT NULL DEFAULT '', parent_conversation_id TEXT NOT NULL DEFAULT '',\
                nesting_depth INTEGER NOT NULL DEFAULT 0, not_fully_idle NUMERIC NOT NULL DEFAULT 0, killed NUMERIC NOT NULL DEFAULT 0,\
                last_user_input_time DATETIME NOT NULL\
            );\
            CREATE TABLE conversation_parents (conversation_id TEXT PRIMARY KEY, parent_id TEXT NOT NULL);\
            CREATE TABLE conversations_fts_content (id INTEGER PRIMARY KEY, c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11, c12, c13, c14);\
            INSERT INTO conversation_summaries VALUES ('cli-1', '', 'You are a deterministic session title editor.', 1, '2026-11-26 12:00:00+00:00', '[\"file:///private/var/folders/ab/cd/T/agy-hyper-namer-42-1\"]', 'default-cli-project', '', '', 0, 0, 0, '2026-11-26 12:00:00+00:00');\
            INSERT INTO conversation_summaries VALUES ('cli-2', 'Manual CLI Run', 'probe', 1, '2026-11-26 11:50:00+00:00', '[\"file:///work/app\"]', 'default-cli-project', '', '', 0, 0, 0, '2026-11-26 11:50:00+00:00');\
            INSERT INTO conversation_summaries VALUES ('namer-tmp', '', 'prompt', 1, '2026-11-26 11:40:00+00:00', '[\"file:///private/var/folders/ab/cd/T/agy-hyper-namer-99\"]', 'p1', '', '', 0, 0, 0, '2026-11-26 11:40:00+00:00');\
            INSERT INTO conversation_summaries VALUES ('home-1', '💬 Core·Session Architecture｜Cross-Client Governance', 'https://example.com/repo', 6, '2026-11-26 11:30:00+00:00', '{home_uri}', 'home-proj', '', '', 0, 0, 0, '2026-11-26 11:00:00+00:00');\
            INSERT INTO conversations_fts_content (id, c0, c10, c13, c14) VALUES (1, 'home-1', 'Session Architecture', 'Compare billing ledger and printer queue behavior across clients', 'Done');\
            "
        );
        seed_sqlite_for_test(&agy_db, &init_agy);

        let reg = SessionRegistry {
            version: 1,
            default_code: "System".into(),
            rules: vec![
                SessionRegistryRule {
                    id: "core".into(),
                    code: "Core".into(),
                    tag: "biz:core".into(),
                    group: "Core Platform".into(),
                    path_prefixes: vec!["/work/core".into()],
                    keywords: vec!["governance".into()],
                },
                SessionRegistryRule {
                    id: "billing".into(),
                    code: "Billing".into(),
                    tag: "biz:billing".into(),
                    group: "Billing Ops".into(),
                    path_prefixes: vec!["/work/billing".into()],
                    keywords: vec!["billing".into()],
                },
            ],
        };

        let all_resp = search_sessions_with_paths(
            &SessionSearchQuery::default(),
            Some(&agy_db),
            std::slice::from_ref(&brain),
            None,
            Some(&reg),
            Some(&fake_home),
        );
        assert_eq!(all_resp.agy_matches, 1);
        assert_eq!(all_resp.sessions[0].id, "home-1");
        assert_eq!(all_resp.sessions[0].group, "Core Platform");
        assert_eq!(all_resp.sessions[0].code, "Core");

        // Searching for `billing` hits `fts_excerpt`, yet `home-1` must keep `Core Platform` via `<Emoji> Core·` title prefix
        let q_billing = search_sessions_with_paths(
            &SessionSearchQuery {
                query: "billing".into(),
                ..Default::default()
            },
            Some(&agy_db),
            std::slice::from_ref(&brain),
            None,
            Some(&reg),
            Some(&fake_home),
        );
        assert_eq!(q_billing.agy_matches, 1);
        assert_eq!(q_billing.sessions[0].group, "Core Platform");
        assert_eq!(q_billing.sessions[0].tag.as_deref(), Some("biz:core"));
    }
}
