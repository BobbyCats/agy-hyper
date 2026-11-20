use crate::protocol::{
    AuditEvent, CockpitSnapshot, DrillOutput, EngineStatus, HandleStored, PrewarmedCheck,
};
use hyper_core::bench::{analyze_transcript, SessionBenchmark};
use hyper_core::config::{truncate_utf8_bytes, HyperConfig, Profile};
use hyper_core::context_pack::{
    build_context_pack_with_cache, AstOutlineCache, ContextPackRequest, ContextPackResult,
};
use hyper_core::jail::WorkspaceJail;
use hyper_core::runner::{discover_presets, run_check, CheckRequest};
use hyper_core::sessions::{discover_primary_transcript, discover_session_summaries};
use hyper_core::transcript::{inspect_transcript_tail, resolve_transcript_path};
use hyper_core::wal::{list_transactions, rollback_transaction, RollbackResult};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_HANDLES: usize = 64;
const MAX_HANDLE_TOTAL_BYTES: usize = 24 * 1024 * 1024;
const MAX_EVENTS: usize = 120;
const MAX_WATCHED_WORKSPACES: usize = 8;

struct StoredHandle {
    id: String,
    label: String,
    content: String,
    bytes: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct PersistedStats {
    total_calls: u64,
    total_saved_ms: u64,
    total_turns_saved: u64,
    blocked_security_events: u64,
    cumulative_tool_ms: u64,
    ast_cache_hits: u64,
    ast_cache_misses: u64,
    pack_cache_hits: u64,
    pack_cache_misses: u64,
    tool_counts: HashMap<String, u64>,
}

/// Shared mtime/size-keyed transcript benchmark cache so 2.5s cockpit polls never reparse 50MB logs.
#[derive(Debug, Default)]
pub struct BenchCache {
    entries: HashMap<PathBuf, (u128, u64, SessionBenchmark)>,
}

impl BenchCache {
    pub fn analyze_cached(&mut self, path: &Path) -> Option<SessionBenchmark> {
        let meta = fs::metadata(path).ok()?;
        let size = meta.len();
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        if let Some((cm, cs, bench)) = self.entries.get(path) {
            if *cm == mtime_ns && *cs == size {
                return Some(bench.clone());
            }
        }
        let fresh = analyze_transcript(path).ok()?;
        if self.entries.len() >= 16 {
            if let Some(evict) = self.entries.keys().next().cloned() {
                self.entries.remove(&evict);
            }
        }
        self.entries
            .insert(path.to_path_buf(), (mtime_ns, size, fresh.clone()));
        Some(fresh)
    }
}

pub struct DaemonState {
    started_at: Instant,
    config: HyperConfig,
    stats: PersistedStats,
    last_stats_flush: Instant,
    workspace_revs: Arc<Mutex<HashMap<String, u64>>>,
    watchers: HashMap<String, RecommendedWatcher>,
    prewarm_cache: Arc<Mutex<HashMap<(String, String), PrewarmedCheck>>>,
    prewarm_scheduled_rev: Arc<Mutex<HashMap<String, u64>>>,
    pub bench_cache: Arc<Mutex<BenchCache>>,
    ast_cache: AstOutlineCache,
    pack_cache: HashMap<String, (u64, u64, ContextPackResult)>,
    pack_seq: u64,
    pack_cache_hits: u64,
    pack_cache_misses: u64,
    handles: Arc<Mutex<HandleStore>>,
    events: VecDeque<AuditEvent>,
}

#[derive(Default)]
struct HandleStore {
    items: VecDeque<StoredHandle>,
    total_bytes: usize,
    seq: u64,
}

impl HandleStore {
    fn store(&mut self, label: String, content: String) -> HandleStored {
        self.seq += 1;
        let hash = blake3::hash(format!("{}:{}", self.seq, now_ms()).as_bytes());
        let id = format!("h_{}", &hash.to_hex()[..8]);
        let total_lines = content.lines().count();
        let bytes = content.len();

        while self.items.len() >= MAX_HANDLES
            || (self.total_bytes + bytes > MAX_HANDLE_TOTAL_BYTES && !self.items.is_empty())
        {
            if let Some(old) = self.items.pop_front() {
                self.total_bytes = self.total_bytes.saturating_sub(old.bytes);
            }
        }
        self.total_bytes += bytes;
        self.items.push_back(StoredHandle {
            id: id.clone(),
            label,
            content,
            bytes,
        });
        HandleStored {
            handle_id: id,
            total_lines,
            total_bytes: bytes,
        }
    }

    fn drill(
        &self,
        handle_id: &str,
        offset_lines: usize,
        limit_lines: usize,
        grep: Option<&str>,
        max_output_bytes: usize,
    ) -> Result<DrillOutput, String> {
        let item = self
            .items
            .iter()
            .find(|h| h.id == handle_id)
            .ok_or_else(|| format!("handle '{}' not found or evicted from LRU", handle_id))?;

        let all_lines: Vec<&str> = item.content.lines().collect();
        let filtered: Vec<(usize, &str)> = if let Some(pat) = grep.filter(|s| !s.trim().is_empty())
        {
            let needle = pat.to_ascii_lowercase();
            all_lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.to_ascii_lowercase().contains(&needle))
                .map(|(i, l)| (i + 1, *l))
                .collect()
        } else {
            all_lines
                .iter()
                .enumerate()
                .map(|(i, l)| (i + 1, *l))
                .collect()
        };

        let limit = limit_lines.clamp(1, 200);
        let slice: Vec<String> = filtered
            .iter()
            .skip(offset_lines)
            .take(limit)
            .map(|(ln, txt)| format!("{}: {}", ln, txt))
            .collect();
        let joined = slice.join("\n");
        let (capped, _) = truncate_utf8_bytes(&joined, max_output_bytes.min(7_800));

        Ok(DrillOutput {
            handle_id: item.id.clone(),
            label: item.label.clone(),
            total_lines: filtered.len(),
            returned_lines: slice.len(),
            offset_lines,
            content: capped.to_string(),
        })
    }
}

impl DaemonState {
    pub fn new() -> Self {
        let config = HyperConfig::load();
        let stats = load_stats(&stats_path());
        Self {
            started_at: Instant::now(),
            config,
            stats,
            last_stats_flush: Instant::now(),
            workspace_revs: Arc::new(Mutex::new(HashMap::new())),
            watchers: HashMap::new(),
            prewarm_cache: Arc::new(Mutex::new(HashMap::new())),
            prewarm_scheduled_rev: Arc::new(Mutex::new(HashMap::new())),
            bench_cache: Arc::new(Mutex::new(BenchCache::default())),
            ast_cache: AstOutlineCache::default(),
            pack_cache: HashMap::new(),
            pack_seq: 0,
            pack_cache_hits: 0,
            pack_cache_misses: 0,
            handles: Arc::new(Mutex::new(HandleStore::default())),
            events: VecDeque::with_capacity(MAX_EVENTS),
        }
    }

    pub fn flush_stats_now(&mut self) {
        save_stats(&stats_path(), &self.snapshot_persisted_stats());
        self.last_stats_flush = Instant::now();
    }

    fn maybe_flush_stats(&mut self) {
        if self.last_stats_flush.elapsed() >= Duration::from_secs(2) {
            self.flush_stats_now();
        }
    }

    pub fn engine_status(&self) -> EngineStatus {
        let home_str = dirs::home_dir().map(|h| h.display().to_string());
        let workspaces: Vec<(String, u64)> = self
            .workspace_revs
            .lock()
            .map(|m| {
                let mut v: Vec<(String, u64)> = m
                    .iter()
                    .map(|(k, r)| {
                        let display = match &home_str {
                            Some(h) if k == h => "~".to_string(),
                            Some(h) if k.starts_with(h) => format!("~{}", &k[h.len()..]),
                            _ => k.clone(),
                        };
                        (display, *r)
                    })
                    .collect();
                v.sort_by(|a, b| a.0.cmp(&b.0));
                v
            })
            .unwrap_or_default();

        let prewarm_cached = self.prewarm_cache.lock().map(|m| m.len()).unwrap_or(0);
        let handles_cached = self.handles.lock().map(|h| h.items.len()).unwrap_or(0);
        let avg_latency = if self.stats.total_calls > 0 {
            self.stats.cumulative_tool_ms as f64 / self.stats.total_calls as f64
        } else {
            0.0
        };

        EngineStatus {
            pid: std::process::id(),
            version: hyper_core::version().into(),
            uptime_secs: self.started_at.elapsed().as_secs(),
            rss_mb: current_rss_mb(),
            active_workspaces: workspaces,
            handles_cached,
            prewarm_cached,
            ast_cache_entries: self.ast_cache.len(),
            ast_cache_hits: self.stats.ast_cache_hits + self.ast_cache.hits,
            ast_cache_misses: self.stats.ast_cache_misses + self.ast_cache.misses,
            pack_cache_hits: self.stats.pack_cache_hits + self.pack_cache_hits,
            pack_cache_misses: self.stats.pack_cache_misses + self.pack_cache_misses,
            tool_counts: self.stats.tool_counts.clone(),
            total_calls: self.stats.total_calls,
            total_saved_ms: self.stats.total_saved_ms,
            total_turns_saved: self.stats.total_turns_saved,
            blocked_security_events: self.stats.blocked_security_events,
            avg_tool_latency_ms: (avg_latency * 10.0).round() / 10.0,
        }
    }

    pub fn record_event(
        &mut self,
        kind: String,
        workspace: Option<String>,
        duration_ms: u64,
        saved_ms: u64,
        turns_saved: u32,
        detail: String,
        blocked: bool,
    ) {
        if kind != "wal_capture" {
            self.stats.total_calls += 1;
            self.stats.cumulative_tool_ms += duration_ms;
            self.stats.total_saved_ms += saved_ms;
            self.stats.total_turns_saved += turns_saved as u64;
            *self.stats.tool_counts.entry(kind.clone()).or_insert(0) += 1;
        }
        if blocked {
            self.stats.blocked_security_events += 1;
        }
        if self.events.len() >= MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(AuditEvent {
            ts_ms: now_ms(),
            kind,
            workspace,
            duration_ms,
            saved_ms,
            detail,
            blocked,
        });
        self.maybe_flush_stats();
    }

    pub fn pack_context(
        &mut self,
        workspace: &str,
        req: &ContextPackRequest,
    ) -> Result<(ContextPackResult, bool), String> {
        let rev = self.watch_workspace(workspace);
        let jail = WorkspaceJail::new(
            workspace,
            self.config.allow_outside_workspace,
            self.config.block_secret_files,
        )
        .map_err(|e| e.to_string())?;
        let canonical = jail.root().display().to_string();

        let mut norm_req = req.clone();
        norm_req.query = norm_req
            .query
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        norm_req.focus_paths.sort();
        norm_req.focus_paths.dedup();

        let key_json = serde_json::to_string(&(&canonical, &norm_req)).unwrap_or_default();
        let cache_key = blake3::hash(key_json.as_bytes()).to_hex().to_string();

        self.pack_seq += 1;
        let seq = self.pack_seq;
        if let Some((cached_rev, last_used, cached_pack)) = self.pack_cache.get_mut(&cache_key) {
            if *cached_rev == rev {
                *last_used = seq;
                let res = cached_pack.clone();
                self.pack_cache_hits += 1;
                self.maybe_flush_stats();
                return Ok((res, true));
            }
        }

        self.pack_cache_misses += 1;
        let block_secrets = self.config.block_secret_files;
        let pack = build_context_pack_with_cache(
            &jail,
            &norm_req,
            block_secrets,
            Some(&mut self.ast_cache),
        )
        .map_err(|e| e.to_string())?;

        if self.pack_cache.len() >= 64 {
            if let Some(lru_key) = self
                .pack_cache
                .iter()
                .min_by_key(|(_, (_, lu, _))| *lu)
                .map(|(k, _)| k.clone())
            {
                self.pack_cache.remove(&lru_key);
            }
        }
        self.pack_cache.insert(cache_key, (rev, seq, pack.clone()));
        self.maybe_flush_stats();
        Ok((pack, false))
    }

    fn snapshot_persisted_stats(&self) -> PersistedStats {
        let mut s = self.stats.clone();
        s.ast_cache_hits += self.ast_cache.hits;
        s.ast_cache_misses += self.ast_cache.misses;
        s.pack_cache_hits += self.pack_cache_hits;
        s.pack_cache_misses += self.pack_cache_misses;
        s
    }

    pub fn watch_workspace(&mut self, workspace: &str) -> u64 {
        let canonical = fs::canonicalize(workspace)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| workspace.to_string());

        let rev = {
            let mut map = self.workspace_revs.lock().unwrap();
            *map.entry(canonical.clone()).or_insert(1)
        };

        // Never recursively attach OS file watchers to `$HOME` or filesystem root `/`
        // (prevents Linux inotify exhaustion and constant cache thrashing from user caches).
        let is_too_broad = Path::new(&canonical).parent().is_none()
            || dirs::home_dir()
                .and_then(|h| fs::canonicalize(h).ok())
                .is_some_and(|h| Path::new(&canonical) == h);
        if is_too_broad || self.watchers.contains_key(&canonical) {
            return rev;
        }

        if self.watchers.len() >= MAX_WATCHED_WORKSPACES {
            if let Some(evict_key) = self.watchers.keys().next().cloned() {
                self.watchers.remove(&evict_key);
            }
        }
        let revs_arc = self.workspace_revs.clone();
        let ws_key = canonical.clone();
        if let Ok(mut watcher) = notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(ev) = res {
                let ignore_noise = ev.paths.iter().all(|p| {
                    let norm = p.to_string_lossy().replace('\\', "/");
                    norm.contains("/.git/")
                        || norm.contains("/target/")
                        || norm.contains("/node_modules/")
                        || norm.contains("/.agy-hyper.")
                        || norm.contains("/.DS_Store")
                        || norm.contains("/dist/")
                        || norm.contains("/.next/")
                        || norm.contains("/__pycache__/")
                        || norm.contains("/.cache/")
                });
                if !ignore_noise {
                    if let Ok(mut guard) = revs_arc.lock() {
                        let e = guard.entry(ws_key.clone()).or_insert(1);
                        *e += 1;
                    }
                }
            }
        }) {
            if watcher
                .watch(Path::new(&canonical), RecursiveMode::Recursive)
                .is_ok()
            {
                self.watchers.insert(canonical, watcher);
            }
        }
        rev
    }

    pub fn bump_workspace_rev(&mut self, workspace: &str) -> u64 {
        let canonical = fs::canonicalize(workspace)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| workspace.to_string());
        let rev = {
            let mut map = self.workspace_revs.lock().unwrap();
            let entry = map.entry(canonical.clone()).or_insert(1);
            *entry += 1;
            *entry
        };
        self.schedule_tier2_prewarm(&canonical, rev);
        rev
    }

    pub fn schedule_tier2_prewarm(&self, canonical_ws: &str, target_rev: u64) {
        if !self.config.prewarm_tier2 {
            return;
        }
        let ws_path = PathBuf::from(canonical_ws);
        let Some(preset) = discover_presets(&ws_path)
            .into_iter()
            .find(|p| p.read_only_safe)
        else {
            return;
        };

        {
            let mut sched = self.prewarm_scheduled_rev.lock().unwrap();
            if sched.get(canonical_ws).copied().unwrap_or(0) >= target_rev {
                return;
            }
            sched.insert(canonical_ws.to_string(), target_rev);
        }

        let revs = self.workspace_revs.clone();
        let cache = self.prewarm_cache.clone();
        let handles = self.handles.clone();
        let cfg = self.config.clone();
        let ws_key = canonical_ws.to_string();
        let preset_id = preset.id.clone();

        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            let cur_rev = revs
                .lock()
                .ok()
                .and_then(|m| m.get(&ws_key).copied())
                .unwrap_or(0);
            if cur_rev != target_rev {
                return;
            }
            let Ok(jail) =
                WorkspaceJail::new(&ws_key, cfg.allow_outside_workspace, cfg.block_secret_files)
            else {
                return;
            };
            let req = CheckRequest {
                preset: Some(preset_id.clone()),
                argv: None,
                timeout_secs: 45,
                cwd: None,
            };
            let Ok(res) = run_check(&jail, &cfg, &req, None) else {
                return;
            };
            let rev_after = revs
                .lock()
                .ok()
                .and_then(|m| m.get(&ws_key).copied())
                .unwrap_or(0);
            if rev_after != target_rev {
                return;
            }
            let spill_handle = if res.digest.truncated && !res.full_captured_log.is_empty() {
                handles
                    .lock()
                    .ok()
                    .map(|mut h| h.store(res.command_display.clone(), res.full_captured_log).handle_id)
            } else {
                None
            };
            let entry = PrewarmedCheck {
                workspace: ws_key.clone(),
                preset: preset_id.clone(),
                workspace_rev: target_rev,
                command_display: res.command_display,
                exit_code: res.exit_code,
                duration_ms: res.duration_ms,
                total_output_bytes: res.total_output_bytes,
                summary_lines: res.digest.summary_lines,
                toc_hint: res.digest.toc_hint,
                truncated: res.digest.truncated,
                spill_handle,
                completed_ts_ms: now_ms(),
            };
            if let Ok(mut map) = cache.lock() {
                if map.len() >= 16 {
                    if let Some(old_k) = map.keys().next().cloned() {
                        map.remove(&old_k);
                    }
                }
                map.insert((ws_key, preset_id), entry);
            }
        });
    }

    pub fn lookup_prewarm(&self, workspace: &str, preset: &str) -> Option<PrewarmedCheck> {
        let canonical = fs::canonicalize(workspace)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| workspace.to_string());
        let cur_rev = self
            .workspace_revs
            .lock()
            .ok()
            .and_then(|m| m.get(&canonical).copied())?;
        let cache = self.prewarm_cache.lock().ok()?;
        let item = cache.get(&(canonical, preset.to_string()))?;
        if item.workspace_rev == cur_rev && now_ms().saturating_sub(item.completed_ts_ms) <= 300_000
        {
            Some(item.clone())
        } else {
            None
        }
    }

    pub fn store_handle(&mut self, label: String, content: String) -> HandleStored {
        self.handles.lock().unwrap().store(label, content)
    }

    pub fn drill_handle(
        &self,
        handle_id: &str,
        offset_lines: usize,
        limit_lines: usize,
        grep: Option<&str>,
    ) -> Result<DrillOutput, String> {
        self.handles
            .lock()
            .unwrap()
            .drill(handle_id, offset_lines, limit_lines, grep, self.config.max_output_bytes)
    }

    pub fn set_profile(&mut self, profile_str: &str) -> Result<HyperConfig, String> {
        let p = Profile::from_str_loose(profile_str)
            .ok_or_else(|| format!("unknown profile '{}' (expected safe|balanced|hyper)", profile_str))?;
        self.config.apply_profile(p);
        self.config.save().map_err(|e| e.to_string())?;
        Ok(self.config.clone())
    }

    pub fn set_toggle(&mut self, key: &str, value: bool) -> Result<HyperConfig, String> {
        self.config.set_toggle(key, value).map_err(|e| e.to_string())?;
        self.config.save().map_err(|e| e.to_string())?;
        Ok(self.config.clone())
    }

    /// Capture lightweight in-memory state in `<10us` so heavy disk/transcript I/O runs lock-free.
    pub fn snapshot_parts(&self) -> (EngineStatus, HyperConfig, Vec<AuditEvent>, Arc<Mutex<BenchCache>>) {
        (
            self.engine_status(),
            self.config.clone(),
            self.events.iter().rev().take(40).cloned().collect(),
            self.bench_cache.clone(),
        )
    }

    pub fn record_rollback(&mut self, tx_id: &str, res: &RollbackResult) {
        self.record_event(
            "rollback".into(),
            None,
            2,
            0,
            0,
            format!("rolled back {} ({} restored)", tx_id, res.restored_files.len()),
            false,
        );
        self.flush_stats_now();
    }
}

/// Shared lock-free cockpit snapshot builder used by both the daemon and CLI fallback.
pub fn build_cockpit_snapshot(
    engine: EngineStatus,
    config: HyperConfig,
    recent_events: Vec<AuditEvent>,
    bench_cache: Option<&Mutex<BenchCache>>,
    conversation_id: Option<&str>,
    transcript_path: Option<&str>,
) -> CockpitSnapshot {
    let tpath = resolve_transcript_path(transcript_path, conversation_id)
        .or_else(discover_primary_transcript);

    let pulse = tpath
        .as_deref()
        .and_then(|p| inspect_transcript_tail(p, &config));
    let benchmark = tpath.as_deref().and_then(|p| {
        if let Some(cache) = bench_cache {
            cache.lock().ok().and_then(|mut c| c.analyze_cached(p))
        } else {
            analyze_transcript(p).ok()
        }
    });
    let recent_sessions = discover_session_summaries(12)
        .into_iter()
        .filter(|s| !s.is_subagent)
        .take(8)
        .collect();
    let transactions = list_transactions(&HyperConfig::wal_dir(), 25);

    CockpitSnapshot {
        engine,
        config,
        pulse,
        benchmark,
        recent_sessions,
        transactions,
        recent_events,
    }
}

pub fn execute_rollback_unlocked(tx_id: &str, force: bool) -> Result<RollbackResult, String> {
    rollback_transaction(&HyperConfig::wal_dir(), tx_id, force).map_err(|e| e.to_string())
}

fn stats_path() -> PathBuf {
    HyperConfig::data_dir().join("stats.json")
}

fn load_stats(path: &Path) -> PersistedStats {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_stats(path: &Path, stats: &PersistedStats) {
    if let Some(p) = path.parent() {
        let _ = fs::create_dir_all(p);
    }
    if let Ok(bytes) = serde_json::to_vec(stats) {
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, bytes).is_ok() {
            let _ = fs::rename(&tmp, path);
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn current_rss_mb() -> f64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(statm) = fs::read_to_string("/proc/self/statm") {
            if let Some(pages) = statm.split_whitespace().nth(1).and_then(|s| s.parse::<u64>().ok()) {
                let page_bytes = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as f64;
                if page_bytes > 0.0 {
                    return (((pages as f64) * page_bytes) / (1024.0 * 1024.0) * 10.0).round() / 10.0;
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0 {
            return ((usage.ru_maxrss as f64) / (1024.0 * 1024.0) * 10.0).round() / 10.0;
        }
    }
    0.0
}
