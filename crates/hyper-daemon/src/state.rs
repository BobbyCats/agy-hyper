use crate::protocol::{
    AuditEvent, CockpitSnapshot, DrillOutput, EngineStatus, HandleStored, PrewarmedCheck,
    SessionTreeSummary, StatuslinePulseResponse, SubagentSnapshot,
};
use hyper_core::bench::{analyze_transcript, SessionBenchmark};
use hyper_core::config::{truncate_utf8_bytes, HyperConfig, Profile};
use hyper_core::context_pack::{
    build_context_pack_with_cache, AstOutlineCache, ContextPackRequest, ContextPackResult,
};
use hyper_core::jail::WorkspaceJail;
use hyper_core::runner::{discover_presets, run_check, CheckRequest};
use hyper_core::sessions::{
    collect_descendant_sessions, discover_primary_transcript, discover_session_summaries,
    resolve_root_conversation_id,
};
use hyper_core::transcript::{inspect_transcript_tail, resolve_transcript_path, HealthLevel};
use hyper_core::usage::{
    build_session_usage, default_brain_dirs, model_ttl_family, now_epoch_ms, ModelTtlEstimate,
    TokenBucket, UsageLedger, UsageWindow, UsageWindowReport,
};
use hyper_core::wal::{list_transactions, rollback_transaction, RollbackResult};
use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
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
        if self.entries.len() >= 64 {
            if let Some(evict) = self.entries.keys().next().cloned() {
                self.entries.remove(&evict);
            }
        }
        self.entries
            .insert(path.to_path_buf(), (mtime_ns, size, fresh.clone()));
        Some(fresh)
    }
}

#[derive(Debug, Default)]
pub struct UsageLedgerCache {
    loaded: bool,
    ledger: UsageLedger,
    last_sync: Option<Instant>,
}

impl UsageLedgerCache {
    fn ensure_loaded(&mut self) {
        if !self.loaded {
            self.ledger = UsageLedger::load_from(&HyperConfig::ledger_path());
            self.loaded = true;
        }
    }

    fn sync_if_needed(&mut self, force_sync: bool, min_interval_secs: u64) -> (i64, u64) {
        self.ensure_loaded();
        let now_ms = now_epoch_ms();
        let should_sync = force_sync
            || self
                .last_sync
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(min_interval_secs));
        let mut scan_ms = 0u64;
        if should_sync {
            let ledger_path = HyperConfig::ledger_path();
            let outcome = self.ledger.sync_from_dirs(&default_brain_dirs(), now_ms);
            scan_ms = outcome.scan_ms;
            if outcome.dirty {
                let _ = self.ledger.save_to(&ledger_path);
            }
            self.last_sync = Some(Instant::now());
        }
        (now_ms, scan_ms)
    }

    pub fn query_window(
        &mut self,
        window: UsageWindow,
        cfg: &HyperConfig,
        currency: Option<&str>,
        tz_offset_mins: i32,
        force_sync: bool,
    ) -> UsageWindowReport {
        let (now_ms, scan_ms) = self.sync_if_needed(force_sync, 10);
        self.ledger
            .query_window(window, cfg, currency, tz_offset_mins, now_ms, scan_ms)
    }

    pub fn estimate_ttl_cached(
        &mut self,
        model: Option<&str>,
        cfg: &HyperConfig,
        allow_sync: bool,
    ) -> ModelTtlEstimate {
        if allow_sync {
            let _ = self.sync_if_needed(false, 60);
        } else {
            self.ensure_loaded();
        }
        self.ledger
            .estimate_model_ttl(model, cfg.cache_warm_window_secs)
    }

    pub fn all_ttl_estimates_cached(
        &mut self,
        cfg: &HyperConfig,
        force_sync: bool,
    ) -> Vec<ModelTtlEstimate> {
        let _ = self.sync_if_needed(force_sync, 60);
        self.ledger
            .all_model_ttl_estimates(cfg.cache_warm_window_secs)
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
    last_check_preset: Arc<Mutex<HashMap<String, String>>>,
    pub bench_cache: Arc<Mutex<BenchCache>>,
    pub usage_cache: Arc<Mutex<UsageLedgerCache>>,
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

pub type SnapshotParts = (
    EngineStatus,
    HyperConfig,
    Vec<AuditEvent>,
    Arc<Mutex<BenchCache>>,
    Arc<Mutex<UsageLedgerCache>>,
);

pub type StatuslineParts = (
    HyperConfig,
    Arc<Mutex<BenchCache>>,
    Arc<Mutex<UsageLedgerCache>>,
);

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
            last_check_preset: Arc::new(Mutex::new(HashMap::new())),
            bench_cache: Arc::new(Mutex::new(BenchCache::default())),
            usage_cache: Arc::new(Mutex::new(UsageLedgerCache::default())),
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
                if !is_content_mutation_event(&ev.kind) {
                    return;
                }
                let ignore_noise = ev.paths.is_empty()
                    || ev.paths.iter().all(|p| is_noisy_watch_path(p));
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

    pub fn record_check_target(&mut self, workspace: &str, preset: &str) {
        let canonical = fs::canonicalize(workspace)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| workspace.to_string());
        if let Ok(mut map) = self.last_check_preset.lock() {
            if map.len() >= 32 && !map.contains_key(&canonical) {
                if let Some(old_k) = map.keys().next().cloned() {
                    map.remove(&old_k);
                }
            }
            map.insert(canonical, preset.to_string());
        }
    }

    pub fn schedule_tier2_prewarm(&self, canonical_ws: &str, target_rev: u64) {
        if !self.config.prewarm_tier2 {
            return;
        }
        let ws_path = PathBuf::from(canonical_ws);
        let presets = discover_presets(&ws_path);
        let preferred_id = self
            .last_check_preset
            .lock()
            .ok()
            .and_then(|m| m.get(canonical_ws).cloned());
        let Some(preset) = preferred_id
            .as_deref()
            .and_then(|id| presets.iter().find(|p| p.id == id && p.read_only_safe))
            .or_else(|| presets.iter().find(|p| p.read_only_safe))
            .cloned()
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
                idle_timeout_secs: None,
                fail_fast: None,
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

    pub fn set_currency(&mut self, currency: &str) -> Result<HyperConfig, String> {
        self.config
            .set_currency(currency)
            .map_err(|e| e.to_string())?;
        self.config.save().map_err(|e| e.to_string())?;
        Ok(self.config.clone())
    }

    pub fn update_fx_rates(
        &mut self,
        rates: BTreeMap<String, f64>,
        source: Option<&str>,
    ) -> Result<HyperConfig, String> {
        self.config
            .update_fx_rates(rates, source.unwrap_or("live"), now_ms())
            .map_err(|e| e.to_string())?;
        self.config.save().map_err(|e| e.to_string())?;
        Ok(self.config.clone())
    }

    pub fn usage_parts(&self) -> (HyperConfig, Arc<Mutex<UsageLedgerCache>>) {
        (self.config.clone(), self.usage_cache.clone())
    }

    pub fn statusline_parts(&self) -> StatuslineParts {
        (
            self.config.clone(),
            self.bench_cache.clone(),
            self.usage_cache.clone(),
        )
    }

    /// Capture lightweight in-memory state in `<10us` so heavy disk/transcript I/O runs lock-free.
    pub fn snapshot_parts(&self) -> SnapshotParts {
        (
            self.engine_status(),
            self.config.clone(),
            self.events.iter().rev().take(40).cloned().collect(),
            self.bench_cache.clone(),
            self.usage_cache.clone(),
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

const STALLED_SUBAGENT_IDLE_SECS: u64 = 90;

/// Ultra-lightweight lock-free pulse for Antigravity CLI `statusLine` & `title` (`<2ms`).
/// Never scans 60 sessions, never invokes sqlite3, and never scans brain directories on hot ticks.
pub fn build_statusline_pulse(
    config: &HyperConfig,
    bench_cache: Option<&Mutex<BenchCache>>,
    usage_cache: Option<&Mutex<UsageLedgerCache>>,
    conversation_id: Option<&str>,
    transcript_path: Option<&str>,
    model_hint: Option<&str>,
) -> Option<StatuslinePulseResponse> {
    let tpath = resolve_transcript_path(transcript_path, conversation_id)
        .or_else(discover_primary_transcript)?;
    let mut pulse = inspect_transcript_tail(&tpath, config)?;
    let effective_model = pulse
        .model
        .as_deref()
        .or_else(|| model_hint.filter(|s| !s.trim().is_empty()));

    if let Some(uc) = usage_cache {
        if let Ok(mut guard) = uc.lock() {
            let est = guard.estimate_ttl_cached(effective_model, config, false);
            pulse.apply_cache_ttl(est.effective_ttl_secs, est.source);
        }
    }

    let bench = if let Some(cache) = bench_cache {
        cache.lock().ok().and_then(|mut c| c.analyze_cached(&tpath))
    } else {
        analyze_transcript(&tpath).ok()
    };

    Some(StatuslinePulseResponse {
        model: pulse
            .model
            .clone()
            .or_else(|| model_hint.map(String::from)),
        effective_cache_ttl_secs: pulse.effective_cache_ttl_secs,
        cache_ttl_source: pulse.cache_ttl_source,
        cache_warm_remaining_secs: pulse.cache_warm_remaining_secs,
        cache_is_cold: pulse.cache_is_cold,
        total_tokens: pulse.total_tokens,
        effective_red_tokens: pulse.effective_red_tokens,
        cache_hit_ratio: bench
            .as_ref()
            .map(|b| b.cache_hit_ratio)
            .unwrap_or(pulse.cache_hit_ratio),
        health: pulse.health,
        consecutive_error_streak: pulse.consecutive_error_streak,
        median_turn_ms: bench.as_ref().map(|b| b.median_turn_ms).unwrap_or(0),
        median_llm_ms: bench.as_ref().map(|b| b.median_llm_ms).unwrap_or(0),
        effective_tps: bench.as_ref().map(|b| b.effective_tps).unwrap_or(0.0),
        hyper_adoption_ratio: bench.as_ref().map(|b| b.hyper_adoption_ratio).unwrap_or(0.0),
        total_turns: bench.as_ref().map(|b| b.total_turns).unwrap_or(0),
    })
}

/// Shared lock-free cockpit snapshot builder used by both the daemon and CLI fallback.
pub fn build_cockpit_snapshot(
    engine: EngineStatus,
    config: HyperConfig,
    recent_events: Vec<AuditEvent>,
    bench_cache: Option<&Mutex<BenchCache>>,
    usage_cache: Option<&Mutex<UsageLedgerCache>>,
    conversation_id: Option<&str>,
    transcript_path: Option<&str>,
) -> CockpitSnapshot {
    let all_sessions = discover_session_summaries(60);
    let tpath = resolve_transcript_path(transcript_path, conversation_id)
        .or_else(|| {
            all_sessions
                .iter()
                .find(|s| !s.is_subagent)
                .or_else(|| all_sessions.first())
                .map(|s| PathBuf::from(&s.transcript_path))
        })
        .or_else(discover_primary_transcript);

    let model_ttl_estimates: Vec<ModelTtlEstimate> = if let Some(uc) = usage_cache {
        uc.lock()
            .ok()
            .map(|mut c| c.all_ttl_estimates_cached(&config, false))
            .unwrap_or_default()
    } else {
        UsageLedger::load_from(&HyperConfig::ledger_path())
            .all_model_ttl_estimates(config.cache_warm_window_secs)
    };
    let resolve_ttl = |model: Option<&str>| -> (u64, String) {
        let fam = model_ttl_family(model);
        model_ttl_estimates
            .iter()
            .find(|e| e.model_family == fam)
            .map(|e| (e.effective_ttl_secs, e.source.clone()))
            .unwrap_or_else(|| (config.cache_warm_window_secs, "default".to_string()))
    };

    let analyze_path = |p: &Path| -> Option<SessionBenchmark> {
        if let Some(cache) = bench_cache {
            cache.lock().ok().and_then(|mut c| c.analyze_cached(p))
        } else {
            analyze_transcript(p).ok()
        }
    };

    let pulse = tpath.as_deref().and_then(|p| {
        let mut tp = inspect_transcript_tail(p, &config)?;
        let (ttl, src) = resolve_ttl(tp.model.as_deref());
        tp.apply_cache_ttl(ttl, src);
        Some(tp)
    });
    let benchmark = tpath.as_deref().and_then(analyze_path);

    let selected_id = pulse
        .as_ref()
        .map(|p| p.conversation_id.clone())
        .or_else(|| conversation_id.filter(|s| !s.trim().is_empty()).map(String::from));

    let mut tree_model_usage: BTreeMap<String, TokenBucket> = BTreeMap::new();
    let session_tree = selected_id.as_deref().map(|sel_id| {
        let root_id = resolve_root_conversation_id(sel_id, &all_sessions);
        let selected_meta = all_sessions.iter().find(|s| s.conversation_id == sel_id);
        let selected_is_subagent = root_id != sel_id
            || selected_meta.is_some_and(|s| s.is_subagent);
        let parent_conversation_id = selected_meta
            .and_then(|s| s.parent_conversation_id.clone())
            .or_else(|| (root_id != sel_id).then(|| root_id.clone()));

        let root_bench = if root_id == sel_id {
            benchmark.clone()
        } else {
            all_sessions
                .iter()
                .find(|s| s.conversation_id == root_id)
                .and_then(|s| analyze_path(Path::new(&s.transcript_path)))
        };

        if let Some(rb) = &root_bench {
            for (model, bucket) in &rb.model_usage {
                tree_model_usage
                    .entry(model.clone())
                    .or_default()
                    .add_assign(bucket);
            }
        }

        let mut combined_turns = root_bench.as_ref().map(|b| b.total_turns).unwrap_or(0);
        let mut combined_input = root_bench.as_ref().map(|b| b.total_input_tokens).unwrap_or(0);
        let mut combined_cache = root_bench
            .as_ref()
            .map(|b| b.total_cache_read_tokens)
            .unwrap_or(0);
        let mut combined_hyper = root_bench.as_ref().map(|b| b.hyper_tool_calls).unwrap_or(0);
        let mut combined_tools = root_bench.as_ref().map(|b| b.total_tool_calls).unwrap_or(0);

        let descendants = collect_descendant_sessions(&root_id, &all_sessions);
        let mut subagents = Vec::with_capacity(descendants.len());
        let mut active_subagents = 0usize;
        let mut redline_subagents = 0usize;
        let mut stalled_subagents = 0usize;
        let snap_now_ms = now_ms();

        for child in descendants {
            let cp = Path::new(&child.transcript_path);
            let child_pulse = inspect_transcript_tail(cp, &config).map(|mut tp| {
                let (ttl, src) = resolve_ttl(tp.model.as_deref());
                tp.apply_cache_ttl(ttl, src);
                tp
            });
            let child_bench = analyze_path(cp);
            if let Some(cb) = &child_bench {
                for (model, bucket) in &cb.model_usage {
                    tree_model_usage
                        .entry(model.clone())
                        .or_default()
                        .add_assign(bucket);
                }
            }

            let health = child_pulse
                .as_ref()
                .map(|p| p.health)
                .unwrap_or(HealthLevel::Green);
            let idle_secs = snap_now_ms.saturating_sub(child.mtime_ms) / 1000;
            let stalled = is_stalled_subagent(child.active, idle_secs);
            if child.active {
                active_subagents += 1;
            }
            if is_active_redline(child.active, health) {
                redline_subagents += 1;
            }
            if stalled {
                stalled_subagents += 1;
            }

            let c_in = child_bench.as_ref().map(|b| b.total_input_tokens).unwrap_or(0);
            let c_cache = child_bench
                .as_ref()
                .map(|b| b.total_cache_read_tokens)
                .unwrap_or(0);
            let c_turns = child_bench.as_ref().map(|b| b.total_turns).unwrap_or(0);
            let c_hyper = child_bench.as_ref().map(|b| b.hyper_tool_calls).unwrap_or(0);
            let c_tools = child_bench.as_ref().map(|b| b.total_tool_calls).unwrap_or(0);

            combined_turns += c_turns;
            combined_input += c_in;
            combined_cache += c_cache;
            combined_hyper += c_hyper;
            combined_tools += c_tools;

            subagents.push(SubagentSnapshot {
                conversation_id: child.conversation_id,
                parent_conversation_id: child.parent_conversation_id,
                agent_name: child.agent_name,
                title: child.title,
                nesting_depth: child.nesting_depth,
                active: child.active,
                mtime_ms: child.mtime_ms,
                idle_secs,
                stalled,
                model: child_pulse.as_ref().and_then(|p| p.model.clone()),
                health,
                health_reason: child_pulse
                    .as_ref()
                    .map(|p| p.health_reason.clone())
                    .unwrap_or_else(|| "healthy".into()),
                total_tokens: child_pulse.as_ref().map(|p| p.total_tokens).unwrap_or(0),
                cache_hit_ratio: child_bench
                    .as_ref()
                    .map(|b| b.cache_hit_ratio)
                    .or_else(|| child_pulse.as_ref().map(|p| p.cache_hit_ratio))
                    .unwrap_or(0.0),
                max_step: child_pulse.as_ref().map(|p| p.max_step).unwrap_or(0),
                total_turns: c_turns,
                cumulative_tokens: c_in + c_cache,
                hyper_tool_calls: c_hyper,
                total_tool_calls: c_tools,
            });
        }

        let combined_tokens = combined_input + combined_cache;
        let combined_cache_hit_ratio = if combined_tokens > 0 {
            combined_cache as f64 / combined_tokens as f64
        } else {
            0.0
        };

        SessionTreeSummary {
            root_conversation_id: root_id,
            selected_is_subagent,
            parent_conversation_id,
            subagent_count: subagents.len(),
            active_subagents,
            redline_subagents,
            stalled_subagents,
            combined_turns,
            combined_tokens,
            combined_hyper_calls: combined_hyper,
            combined_total_tools: combined_tools,
            combined_cache_hit_ratio,
            subagents,
        }
    });

    let session_usage = benchmark.as_ref().map(|b| {
        let sub_count = session_tree
            .as_ref()
            .map(|t| t.subagent_count)
            .unwrap_or(0);
        let tree_models = if tree_model_usage.is_empty() {
            &b.model_usage
        } else {
            &tree_model_usage
        };
        build_session_usage(&config, &b.model_usage, tree_models, sub_count)
    });

    let recent_sessions = all_sessions
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
        session_tree,
        session_usage,
        model_ttl_estimates,
        transactions,
        recent_events,
    }
}

pub(crate) fn is_active_redline(active: bool, health: HealthLevel) -> bool {
    active && health == HealthLevel::Red
}

pub(crate) fn is_stalled_subagent(active: bool, idle_secs: u64) -> bool {
    active && idle_secs >= STALLED_SUBAGENT_IDLE_SECS
}

pub(crate) fn is_content_mutation_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_) | ModifyKind::Any)
    )
}

pub(crate) fn is_noisy_watch_path(path: &Path) -> bool {
    let norm = path.to_string_lossy().replace('\\', "/");
    const IGNORED_SEGMENTS: &[&str] = &[
        "/.git/",
        "/target/",
        "/node_modules/",
        "/dist/",
        "/build/",
        "/.next/",
        "/.nuxt/",
        "/.vite/",
        "/.turbo/",
        "/.parcel-cache/",
        "/__pycache__/",
        "/.pytest_cache/",
        "/.mypy_cache/",
        "/.ruff_cache/",
        "/.cache/",
        "/coverage/",
        "/playwright-report/",
        "/test-results/",
        "/.playwright/",
        "/.venv/",
        "/venv/",
        "/tmp/",
        "/output/",
        "/out/",
    ];
    if IGNORED_SEGMENTS.iter().any(|seg| norm.contains(seg)) {
        return true;
    }
    let fname = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if fname.starts_with(".agy-hyper.") || fname == ".DS_Store" {
        return true;
    }
    const IGNORED_SUFFIXES: &[&str] = &[
        ".swp",
        ".swo",
        ".tmp",
        ".log",
        ".db-wal",
        ".db-shm",
        ".sqlite-wal",
        ".sqlite-shm",
        ".pid",
        ".lock",
    ];
    IGNORED_SUFFIXES.iter().any(|suf| fname.ends_with(suf))
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

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, DataChange, MetadataKind};

    #[test]
    fn watch_filters_ignore_access_metadata_and_build_artifacts() {
        assert!(!is_content_mutation_event(&EventKind::Access(AccessKind::Any)));
        assert!(!is_content_mutation_event(&EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any))));
        assert!(is_content_mutation_event(&EventKind::Modify(ModifyKind::Data(DataChange::Content))));
        assert!(is_content_mutation_event(&EventKind::Create(CreateKind::File)));

        assert!(is_noisy_watch_path(Path::new("/repo/.vite/deps/chunk.js")));
        assert!(is_noisy_watch_path(Path::new("/repo/playwright-report/index.html")));
        assert!(is_noisy_watch_path(Path::new("/repo/test-results/trace.zip")));
        assert!(is_noisy_watch_path(Path::new("/repo/data/app.db-wal")));
        assert!(is_noisy_watch_path(Path::new("/repo/logs/server.log")));
        assert!(!is_noisy_watch_path(Path::new("/repo/src/main.rs")));
        assert!(!is_noisy_watch_path(Path::new("/repo/apps/web/src/App.tsx")));
    }

    #[test]
    fn completed_redline_subagents_do_not_trigger_active_redline_alert() {
        assert!(is_active_redline(true, HealthLevel::Red));
        assert!(!is_active_redline(false, HealthLevel::Red));
        assert!(!is_active_redline(true, HealthLevel::Yellow));
        assert!(!is_active_redline(true, HealthLevel::Green));
        assert!(!is_active_redline(false, HealthLevel::Green));
    }

    #[test]
    fn stalled_subagent_requires_active_and_90s_silence() {
        assert!(!is_stalled_subagent(true, 89));
        assert!(is_stalled_subagent(true, 90));
        assert!(is_stalled_subagent(true, 300));
        assert!(!is_stalled_subagent(false, 600));
    }
}
