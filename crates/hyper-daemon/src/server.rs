use crate::protocol::{DaemonRequest, DaemonResponse};
use crate::state::{
    build_cockpit_snapshot, build_statusline_pulse, execute_rollback_unlocked,
    lookup_prewarm_coalesced, DaemonState,
};
use anyhow::{Context, Result};
use fs2::FileExt;
use hyper_core::audit::{
    assert_share_card_clean, build_audit_report, encode_base64, render_share_card_png,
    render_share_card_svg, AuditShareCardPayload, SHARE_PNG_PAD, SHARE_SVG_H, SHARE_SVG_W,
};
use hyper_core::config::HyperConfig;
use hyper_core::namer::{
    discover_unmanaged_recent_agy_sessions, process_session_naming_with_options,
    process_session_stop, NamingOutcome,
};
use hyper_core::radar::{
    build_session_radar, now_epoch_ms_u64 as radar_now_ms, FocusSessionRequest, RadarRuntimeLedger,
    SessionLiveState,
};
use hyper_core::session_index::{search_sessions, SessionSearchQuery};
use hyper_core::sessions::discover_all_active_subagents;
use hyper_core::transcript::inspect_transcript_tail;
use hyper_core::usage::UsageWindow;
use hyper_core::wal::inspect_transaction_diff;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[cfg(unix)]
use tokio::net::UnixListener;

pub fn socket_path() -> PathBuf {
    HyperConfig::run_dir().join("agy-hyper-v1.sock")
}

pub fn lock_path() -> PathBuf {
    HyperConfig::run_dir().join("agy-hyper-v1.lock")
}

pub fn pid_path() -> PathBuf {
    HyperConfig::run_dir().join("agy-hyper-v1.pid")
}

fn acquire_singleton_lock() -> Result<Option<fs::File>> {
    let run_dir = HyperConfig::run_dir();
    fs::create_dir_all(&run_dir).context("create daemon run directory")?;
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path())
        .context("open daemon lock file")?;
    if lock_file.try_lock_exclusive().is_err() {
        return Ok(None);
    }
    let _ = fs::write(pid_path(), std::process::id().to_string());
    Ok(Some(lock_file))
}

#[cfg(unix)]
pub async fn run_daemon_server() -> Result<()> {
    let Some(lock_file) = acquire_singleton_lock()? else {
        return Ok(());
    };
    let sock = socket_path();
    if sock.exists() {
        let _ = fs::remove_file(&sock);
    }

    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("bind unix socket {}", sock.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&sock, fs::Permissions::from_mode(0o600));
    }

    let state = Arc::new(Mutex::new(DaemonState::new()));
    let namer = Arc::new(NamerCoordinator::new());
    let radar = Arc::new(RadarCoordinator::new());
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let sig_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = sig_tx.send(true);
    });
    spawn_watchdog_loop(state.clone(), shutdown_rx.clone());
    spawn_namer_loop(state.clone(), namer.clone(), shutdown_rx.clone());
    spawn_radar_loop(state.clone(), namer.clone(), radar.clone(), shutdown_rx.clone());

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() { break; }
            }
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue; };
                let state = state.clone();
                let namer = namer.clone();
                let radar = radar.clone();
                let shutdown_tx = shutdown_tx.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut buf_reader = BufReader::new(reader);
                    let mut line = String::new();
                    while let Ok(n) = buf_reader.read_line(&mut line).await {
                        if n == 0 { break; }
                        let raw = std::mem::take(&mut line);
                        let st = state.clone();
                        let nm = namer.clone();
                        let rd = radar.clone();
                        let tx = shutdown_tx.clone();
                        let resp = tokio::task::spawn_blocking(move || handle_line(&st, &nm, &rd, &tx, &raw))
                            .await
                            .unwrap_or_else(|e| DaemonResponse::err(e.to_string()));
                        if let Ok(mut out) = serde_json::to_string(&resp) {
                            out.push('\n');
                            if writer.write_all(out.as_bytes()).await.is_err() { break; }
                        }
                    }
                });
            }
        }
    }

    if let Ok(mut guard) = state.lock() {
        guard.flush_stats_now();
    }
    let _ = fs::remove_file(&sock);
    let _ = fs::remove_file(pid_path());
    let _ = lock_file.unlock();
    Ok(())
}

#[cfg(not(unix))]
pub async fn run_daemon_server() -> Result<()> {
    use tokio::net::TcpListener;
    let Some(lock_file) = acquire_singleton_lock()? else {
        return Ok(());
    };
    let run_dir = HyperConfig::run_dir();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let seed = format!(
        "{}:{}:{}",
        std::process::id(),
        addr,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let token = blake3::hash(seed.as_bytes()).to_hex().to_string();
    fs::write(run_dir.join("agy-hyper-v1.addr"), addr.to_string())?;
    fs::write(run_dir.join("agy-hyper-v1.token"), &token)?;
    let state = Arc::new(Mutex::new(DaemonState::new()));
    let namer = Arc::new(NamerCoordinator::new());
    let radar = Arc::new(RadarCoordinator::new());
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let token = Arc::new(token);

    let sig_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = sig_tx.send(true);
    });
    spawn_watchdog_loop(state.clone(), shutdown_rx.clone());
    spawn_namer_loop(state.clone(), namer.clone(), shutdown_rx.clone());
    spawn_radar_loop(state.clone(), namer.clone(), radar.clone(), shutdown_rx.clone());

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() { break; }
            }
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue; };
                let state = state.clone();
                let namer = namer.clone();
                let radar = radar.clone();
                let shutdown_tx = shutdown_tx.clone();
                let expected_token = token.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut buf_reader = BufReader::new(reader);
                    let mut auth_line = String::new();
                    if buf_reader.read_line(&mut auth_line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    if auth_line.trim() != expected_token.as_str() {
                        let _ = writer.write_all(b"{\"status\":\"err\",\"message\":\"unauthorized\"}\n").await;
                        return;
                    }
                    let mut line = String::new();
                    while let Ok(n) = buf_reader.read_line(&mut line).await {
                        if n == 0 { break; }
                        let raw = std::mem::take(&mut line);
                        let st = state.clone();
                        let nm = namer.clone();
                        let rd = radar.clone();
                        let tx = shutdown_tx.clone();
                        let resp = tokio::task::spawn_blocking(move || handle_line(&st, &nm, &rd, &tx, &raw))
                            .await
                            .unwrap_or_else(|e| DaemonResponse::err(e.to_string()));
                        if let Ok(mut out) = serde_json::to_string(&resp) {
                            out.push('\n');
                            if writer.write_all(out.as_bytes()).await.is_err() { break; }
                        }
                    }
                });
            }
        }
    }

    if let Ok(mut guard) = state.lock() {
        guard.flush_stats_now();
    }
    let _ = fs::remove_file(run_dir.join("agy-hyper-v1.addr"));
    let _ = fs::remove_file(run_dir.join("agy-hyper-v1.token"));
    let _ = fs::remove_file(pid_path());
    let _ = lock_file.unlock();
    Ok(())
}

fn handle_line(
    state: &Arc<Mutex<DaemonState>>,
    namer: &Arc<NamerCoordinator>,
    radar: &Arc<RadarCoordinator>,
    shutdown_tx: &tokio::sync::watch::Sender<bool>,
    raw: &str,
) -> DaemonResponse {
    let req: DaemonRequest = match serde_json::from_str(raw.trim()) {
        Ok(r) => r,
        Err(e) => return DaemonResponse::err(format!("invalid request: {}", e)),
    };

    // Execute disk-heavy Snapshot and Rollback OUTSIDE the global DaemonState lock so
    // concurrent MCP tools and lifecycle hooks are never blocked by 2.5s UI polling.
    match req {
        DaemonRequest::SessionRadar { recent_limit } => {
            let (sound_enabled, ui_language, glyph_style) = state
                .lock()
                .map(|g| {
                    let c = g.config_clone();
                    (c.enable_radar_sound, c.ui_language, c.radar_glyph_style)
                })
                .unwrap_or_else(|_| {
                    (
                        true,
                        hyper_core::config::default_ui_language(),
                        hyper_core::config::default_radar_glyph_style(),
                    )
                });
            let (ledger_snap, focus_snap) = radar.snapshot_runtime();
            let mut resp = build_session_radar(
                recent_limit,
                sound_enabled,
                Some(&ledger_snap),
                focus_snap,
            );
            resp.ui_language = ui_language;
            resp.glyph_style = glyph_style;
            return DaemonResponse::ok(resp);
        }
        DaemonRequest::RadarAck { conversation_id } => {
            radar.acknowledge(&conversation_id);
            return DaemonResponse::ok(serde_json::json!({ "ok": true }));
        }
        DaemonRequest::RadarFocus {
            client,
            conversation_id,
            split,
            consume_seq,
        } => {
            if let Some(seq) = consume_seq {
                radar.consume_focus(seq);
                return DaemonResponse::ok(serde_json::json!({ "ok": true, "consumed": seq }));
            }
            let focus = radar.queue_focus(&client, &conversation_id, split);
            return DaemonResponse::ok(serde_json::json!({ "ok": true, "focus": focus }));
        }
        DaemonRequest::SessionStop {
            conversation_id,
            transcript_path,
            workspace_paths,
        } => {
            let queued = namer.enqueue(StopNamingJob {
                conversation_id,
                transcript_path,
                workspace_paths,
                enqueued_at: std::time::Instant::now(),
                force: false,
            });
            return DaemonResponse::ok(serde_json::json!({ "queued": queued }));
        }
        DaemonRequest::NamerSync {
            conversation_id,
            force,
            limit,
        } => {
            if let Some(cid) = conversation_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let cfg = state
                    .lock()
                    .map(|g| g.config_clone())
                    .unwrap_or_default();
                let t0 = std::time::Instant::now();
                return match process_session_naming_with_options(&cfg, cid, None, &[], force) {
                    Ok(outcome) => {
                        if let NamingOutcome::Renamed { .. } = &outcome {
                            let short_id = &cid[..cid.len().min(8)];
                            if let Ok(mut guard) = state.lock() {
                                guard.record_event(
                                    "session_namer".into(),
                                    None,
                                    t0.elapsed().as_millis() as u64,
                                    0,
                                    0,
                                    format!("renamed #{}", short_id),
                                    false,
                                );
                            }
                        }
                        DaemonResponse::ok(serde_json::json!({
                            "ok": true,
                            "conversation_id": cid,
                            "outcome": outcome,
                        }))
                    }
                    Err(e) => DaemonResponse::err(e.to_string()),
                };
            }

            let candidates = discover_unmanaged_recent_agy_sessions(limit.clamp(1, 48));
            let mut queued_ids = Vec::new();
            for (cid, ws) in candidates {
                if namer.enqueue_immediate(cid.clone(), ws, force) {
                    queued_ids.push(cid);
                }
            }
            return DaemonResponse::ok(serde_json::json!({
                "ok": true,
                "queued": queued_ids.len(),
                "conversation_ids": queued_ids,
            }));
        }
        DaemonRequest::SearchSessions {
            query,
            client,
            group,
            days,
            sort,
            limit,
        } => {
            let resp = search_sessions(&SessionSearchQuery {
                query,
                client,
                group,
                days,
                sort,
                limit,
            });
            return DaemonResponse::ok(resp);
        }
        DaemonRequest::Snapshot {
            conversation_id,
            transcript_path,
        } => {
            let (engine, config, recent_events, bench_cache, usage_cache) = match state.lock() {
                Ok(g) => g.snapshot_parts(),
                Err(_) => return DaemonResponse::err("daemon state lock poisoned"),
            };
            let snap = build_cockpit_snapshot(
                engine,
                config,
                recent_events,
                Some(&bench_cache),
                Some(&usage_cache),
                conversation_id.as_deref(),
                transcript_path.as_deref(),
            );
            return DaemonResponse::ok(snap);
        }
        DaemonRequest::LookupPrewarm { workspace, preset } => {
            let (revs, cache, in_flight) = match state.lock() {
                Ok(g) => g.prewarm_lookup_parts(),
                Err(_) => return DaemonResponse::err("daemon state lock poisoned"),
            };
            return match lookup_prewarm_coalesced(
                &revs,
                &cache,
                &in_flight,
                &workspace,
                &preset,
                Duration::from_secs(25),
            ) {
                Some(hit) => DaemonResponse::ok(hit),
                None => DaemonResponse::ok(serde_json::Value::Null),
            };
        }
        DaemonRequest::StatuslinePulse {
            conversation_id,
            transcript_path,
            model,
        } => {
            let (config, bench_cache, usage_cache) = match state.lock() {
                Ok(g) => g.statusline_parts(),
                Err(_) => return DaemonResponse::err("daemon state lock poisoned"),
            };
            let pulse = build_statusline_pulse(
                &config,
                Some(&bench_cache),
                Some(&usage_cache),
                conversation_id.as_deref(),
                transcript_path.as_deref(),
                model.as_deref(),
            );
            return DaemonResponse::ok(pulse);
        }
        DaemonRequest::CacheTtlReport { force_sync } => {
            let (config, usage_cache) = match state.lock() {
                Ok(g) => g.usage_parts(),
                Err(_) => return DaemonResponse::err("daemon state lock poisoned"),
            };
            let estimates = match usage_cache.lock() {
                Ok(mut cache) => cache.all_ttl_estimates_cached(&config, force_sync),
                Err(_) => return DaemonResponse::err("usage ledger lock poisoned"),
            };
            return DaemonResponse::ok(estimates);
        }
        DaemonRequest::UsageSummary {
            window,
            currency,
            tz_offset_mins,
            force_sync,
        } => {
            let (config, usage_cache) = match state.lock() {
                Ok(g) => g.usage_parts(),
                Err(_) => return DaemonResponse::err("daemon state lock poisoned"),
            };
            let report = match usage_cache.lock() {
                Ok(mut cache) => cache.query_window(
                    UsageWindow::parse(&window),
                    &config,
                    currency.as_deref(),
                    tz_offset_mins.unwrap_or(0),
                    force_sync,
                ),
                Err(_) => return DaemonResponse::err("usage ledger lock poisoned"),
            };
            return DaemonResponse::ok(report);
        }
        DaemonRequest::RollbackTx { tx_id, force } => {
            return match execute_rollback_unlocked(&tx_id, force) {
                Ok(res) => {
                    if let Ok(mut g) = state.lock() {
                        g.record_rollback(&tx_id, &res);
                    }
                    DaemonResponse::ok(res)
                }
                Err(e) => DaemonResponse::err(e),
            };
        }
        DaemonRequest::InspectTxDiff { tx_id, max_lines } => {
            return match inspect_transaction_diff(
                &HyperConfig::wal_dir(),
                &tx_id,
                max_lines.unwrap_or(240),
            ) {
                Ok(diff) => DaemonResponse::ok(diff),
                Err(e) => DaemonResponse::err(e.to_string()),
            };
        }
        DaemonRequest::AuditReport {
            days,
            max_sessions,
            tz_offset_mins,
        } => {
            let cfg = match state.lock() {
                Ok(g) => g.usage_parts().0,
                Err(_) => return DaemonResponse::err("daemon state lock poisoned"),
            };
            let report = build_audit_report(days, max_sessions, &cfg, tz_offset_mins);
            return DaemonResponse::ok(report);
        }
        DaemonRequest::AuditShareCard {
            days,
            max_sessions,
            tz_offset_mins,
            theme,
            include_png,
        } => {
            let cfg = match state.lock() {
                Ok(g) => g.usage_parts().0,
                Err(_) => return DaemonResponse::err("daemon state lock poisoned"),
            };
            let report = build_audit_report(days, max_sessions, &cfg, tz_offset_mins);
            let dark = !theme.trim().eq_ignore_ascii_case("light");
            let svg = render_share_card_svg(&report, dark);
            if let Err(e) = assert_share_card_clean(&svg) {
                return DaemonResponse::err(e.to_string());
            }
            let png_base64 = if include_png {
                match render_share_card_png(&report, dark) {
                    Ok(bytes) => Some(encode_base64(&bytes)),
                    Err(e) => return DaemonResponse::err(e.to_string()),
                }
            } else {
                None
            };
            let payload = AuditShareCardPayload {
                svg,
                png_base64,
                svg_width: SHARE_SVG_W,
                svg_height: SHARE_SVG_H,
                png_width: (SHARE_SVG_W + 2 * SHARE_PNG_PAD) * 2,
                png_height: (SHARE_SVG_H + 2 * SHARE_PNG_PAD) * 2,
                theme: if dark { "dark" } else { "light" }.to_string(),
                verdict: report.verdict,
                redacted: true,
            };
            return DaemonResponse::ok(payload);
        }
        _ => {}
    }

    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(_) => return DaemonResponse::err("daemon state lock poisoned"),
    };

    match req {
        DaemonRequest::Ping => DaemonResponse::ok(guard.engine_status()),
        DaemonRequest::TelemetryEvent {
            kind,
            workspace,
            duration_ms,
            saved_ms,
            turns_saved,
            detail,
            blocked,
            ..
        } => {
            guard.record_event(kind, workspace, duration_ms, saved_ms, turns_saved, detail, blocked);
            DaemonResponse::ok(serde_json::json!({ "recorded": true }))
        }
        DaemonRequest::StoreHandle { label, content } => {
            DaemonResponse::ok(guard.store_handle(label, content))
        }
        DaemonRequest::DrillHandle {
            handle_id,
            offset_lines,
            limit_lines,
            grep,
        } => match guard.drill_handle(&handle_id, offset_lines, limit_lines, grep.as_deref()) {
            Ok(out) => DaemonResponse::ok(out),
            Err(e) => DaemonResponse::err(e),
        },
        DaemonRequest::WatchWorkspace { workspace } => {
            let rev = guard.watch_workspace(&workspace);
            DaemonResponse::ok(serde_json::json!({ "workspace_rev": rev }))
        }
        DaemonRequest::NotifyChange { workspace } => {
            let rev = guard.bump_workspace_rev(&workspace);
            DaemonResponse::ok(serde_json::json!({ "workspace_rev": rev }))
        }
        DaemonRequest::RecordCheckTarget { workspace, preset } => {
            guard.record_check_target(&workspace, &preset);
            DaemonResponse::ok(serde_json::json!({ "recorded": true }))
        }
        DaemonRequest::PackContext { workspace, request } => {
            match guard.pack_context(&workspace, &request) {
                Ok((pack, cache_hit)) => DaemonResponse::ok(serde_json::json!({
                    "pack": pack,
                    "cache_hit": cache_hit
                })),
                Err(e) => DaemonResponse::err(e),
            }
        }
        DaemonRequest::SetProfile { profile } => match guard.set_profile(&profile) {
            Ok(cfg) => DaemonResponse::ok(cfg),
            Err(e) => DaemonResponse::err(e),
        },
        DaemonRequest::SetToggle { key, value } => match guard.set_toggle(&key, value) {
            Ok(cfg) => DaemonResponse::ok(cfg),
            Err(e) => DaemonResponse::err(e),
        },
        DaemonRequest::SetCurrency { currency } => match guard.set_currency(&currency) {
            Ok(cfg) => DaemonResponse::ok(cfg),
            Err(e) => DaemonResponse::err(e),
        },
        DaemonRequest::SetLanguage { language } => match guard.set_ui_language(&language) {
            Ok(cfg) => DaemonResponse::ok(cfg),
            Err(e) => DaemonResponse::err(e),
        },
        DaemonRequest::SetGlyphStyle { style } => match guard.set_radar_glyph_style(&style) {
            Ok(cfg) => DaemonResponse::ok(cfg),
            Err(e) => DaemonResponse::err(e),
        },
        DaemonRequest::UpdateFxRates { rates, source } => {
            match guard.update_fx_rates(rates, source.as_deref()) {
                Ok(cfg) => DaemonResponse::ok(cfg),
                Err(e) => DaemonResponse::err(e),
            }
        }
        DaemonRequest::Shutdown => {
            guard.flush_stats_now();
            let _ = shutdown_tx.send(true);
            DaemonResponse::ok(serde_json::json!({ "shutting_down": true }))
        }
        DaemonRequest::Snapshot { .. }
        | DaemonRequest::SessionRadar { .. }
        | DaemonRequest::RadarAck { .. }
        | DaemonRequest::RadarFocus { .. }
        | DaemonRequest::SessionStop { .. }
        | DaemonRequest::NamerSync { .. }
        | DaemonRequest::SearchSessions { .. }
        | DaemonRequest::LookupPrewarm { .. }
        | DaemonRequest::StatuslinePulse { .. }
        | DaemonRequest::CacheTtlReport { .. }
        | DaemonRequest::UsageSummary { .. }
        | DaemonRequest::RollbackTx { .. }
        | DaemonRequest::InspectTxDiff { .. }
        | DaemonRequest::AuditReport { .. }
        | DaemonRequest::AuditShareCard { .. } => unreachable!(),
    }
}

const NAMER_MAX_CONCURRENCY: usize = 2;
const NAMER_DEBOUNCE_MS: u64 = 450;
const NAMER_AUTO_SWEEP_COOLDOWN_SECS: u64 = 45;

#[derive(Debug, Clone)]
struct StopNamingJob {
    conversation_id: String,
    transcript_path: Option<String>,
    workspace_paths: Vec<String>,
    enqueued_at: std::time::Instant,
    force: bool,
}

struct NamerCoordinator {
    pending: Mutex<HashMap<String, StopNamingJob>>,
    in_flight: Mutex<HashSet<String>>,
    auto_attempted_at: Mutex<HashMap<String, std::time::Instant>>,
    notify: tokio::sync::Notify,
    sem: Arc<tokio::sync::Semaphore>,
}

impl NamerCoordinator {
    fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashSet::new()),
            auto_attempted_at: Mutex::new(HashMap::new()),
            notify: tokio::sync::Notify::new(),
            sem: Arc::new(tokio::sync::Semaphore::new(NAMER_MAX_CONCURRENCY)),
        }
    }

    /// Per-conversation coalescing enqueue (`<0.05ms`): latest `Stop` event overwrites any queued
    /// job for the same `conversation_id`.
    fn enqueue(&self, job: StopNamingJob) -> bool {
        let cid = job.conversation_id.trim().to_string();
        if cid.is_empty() {
            return false;
        }
        if let Ok(mut map) = self.pending.lock() {
            if map.len() >= 128 && !map.contains_key(&cid) {
                return false;
            }
            map.insert(cid, job);
        }
        self.notify.notify_one();
        true
    }

    fn enqueue_immediate(&self, cid: String, workspace_paths: Vec<String>, force: bool) -> bool {
        let clean = cid.trim().to_string();
        if clean.is_empty() {
            return false;
        }
        if let Ok(mut attempted) = self.auto_attempted_at.lock() {
            attempted.remove(&clean);
        }
        self.enqueue(StopNamingJob {
            conversation_id: clean,
            transcript_path: None,
            workspace_paths,
            enqueued_at: std::time::Instant::now()
                .checked_sub(Duration::from_millis(NAMER_DEBOUNCE_MS + 10))
                .unwrap_or_else(std::time::Instant::now),
            force,
        })
    }

    /// Coalesced background sweep for newly opened / active `/swarm` windows still carrying
    /// Antigravity's default unformatted title. Rate-limited per `conversation_id` (`45s`).
    fn enqueue_auto_sweep(&self, candidates: Vec<(String, Vec<String>)>) {
        if candidates.is_empty() {
            return;
        }
        let now = std::time::Instant::now();
        let cooldown = Duration::from_secs(NAMER_AUTO_SWEEP_COOLDOWN_SECS);
        let mut to_queue = Vec::new();
        if let Ok(mut attempted) = self.auto_attempted_at.lock() {
            attempted.retain(|_, ts| now.duration_since(*ts) < Duration::from_secs(600));
            let in_f = self.in_flight.lock().ok();
            let pend = self.pending.lock().ok();
            for (cid, ws) in candidates {
                if in_f.as_ref().is_some_and(|s| s.contains(&cid))
                    || pend.as_ref().is_some_and(|m| m.contains_key(&cid))
                {
                    continue;
                }
                if attempted
                    .get(&cid)
                    .is_some_and(|prev| now.duration_since(*prev) < cooldown)
                {
                    continue;
                }
                attempted.insert(cid.clone(), now);
                to_queue.push((cid, ws));
            }
        }
        for (cid, ws) in to_queue {
            let _ = self.enqueue(StopNamingJob {
                conversation_id: cid,
                transcript_path: None,
                workspace_paths: ws,
                enqueued_at: now,
                force: false,
            });
        }
    }

    fn take_ready_job(&self) -> (Option<StopNamingJob>, Option<Duration>) {
        let Ok(mut pending) = self.pending.lock() else {
            return (None, None);
        };
        let Ok(mut in_flight) = self.in_flight.lock() else {
            return (None, None);
        };
        if pending.is_empty() {
            return (None, None);
        }

        let debounce = Duration::from_millis(NAMER_DEBOUNCE_MS);
        let mut next_wait: Option<Duration> = None;
        let mut chosen_cid: Option<String> = None;

        for (cid, job) in pending.iter() {
            if in_flight.contains(cid) {
                continue;
            }
            let elapsed = job.enqueued_at.elapsed();
            if elapsed >= debounce {
                chosen_cid = Some(cid.clone());
                break;
            } else {
                let rem = debounce - elapsed;
                next_wait = Some(next_wait.map_or(rem, |cur| cur.min(rem)));
            }
        }

        if let Some(cid) = chosen_cid {
            if let Some(job) = pending.remove(&cid) {
                in_flight.insert(cid);
                return (Some(job), next_wait);
            }
        }
        (None, next_wait)
    }

    fn finish_in_flight(&self, cid: &str) {
        if let Ok(mut in_flight) = self.in_flight.lock() {
            in_flight.remove(cid);
        }
        self.notify.notify_one();
    }
}

fn spawn_namer_loop(
    state: Arc<Mutex<DaemonState>>,
    namer: Arc<NamerCoordinator>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        loop {
            let (ready_job, next_wait) = namer.take_ready_job();
            if let Some(job) = ready_job {
                let sem = namer.sem.clone();
                let Ok(permit) = sem.acquire_owned().await else {
                    namer.finish_in_flight(&job.conversation_id);
                    break;
                };
                let st = state.clone();
                let nm = namer.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let cid = job.conversation_id.clone();
                    let cfg = st
                        .lock()
                        .map(|g| g.config_clone())
                        .unwrap_or_default();
                    let t0 = std::time::Instant::now();
                    let job_moved = job;
                    let res = tokio::task::spawn_blocking(move || {
                        if job_moved.force {
                            process_session_naming_with_options(
                                &cfg,
                                &job_moved.conversation_id,
                                job_moved.transcript_path.as_deref(),
                                &job_moved.workspace_paths,
                                true,
                            )
                        } else {
                            process_session_stop(
                                &cfg,
                                &job_moved.conversation_id,
                                job_moved.transcript_path.as_deref(),
                                &job_moved.workspace_paths,
                            )
                        }
                    })
                    .await;

                    match res {
                        Ok(Ok(NamingOutcome::Renamed { .. })) => {
                            let short_id = &cid[..cid.len().min(8)];
                            if let Ok(mut guard) = st.lock() {
                                guard.record_event(
                                    "session_namer".into(),
                                    None,
                                    t0.elapsed().as_millis() as u64,
                                    0,
                                    0,
                                    format!("renamed #{}", short_id),
                                    false,
                                );
                            }
                        }
                        Ok(Err(err)) => {
                            let short_id = &cid[..cid.len().min(8)];
                            let msg: String = err.to_string().chars().take(72).collect();
                            if let Ok(mut guard) = st.lock() {
                                guard.record_event(
                                    "session_namer_err".into(),
                                    None,
                                    t0.elapsed().as_millis() as u64,
                                    0,
                                    0,
                                    format!("#{} {}", short_id, msg),
                                    false,
                                );
                            }
                        }
                        _ => {}
                    }
                    nm.finish_in_flight(&cid);
                });
                continue;
            }

            if let Some(wait_dur) = next_wait {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() { break; }
                    }
                    _ = namer.notify.notified() => {}
                    _ = tokio::time::sleep(wait_dur) => {}
                }
            } else {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() { break; }
                    }
                    _ = namer.notify.notified() => {}
                }
            }
        }
    });
}

const WATCHDOG_POLL_SECS: u64 = 15;
const WATCHDOG_STALLED_SECS: u64 = 120;
const WATCHDOG_MAX_ACTIVE_AGE_SECS: u64 = 1800;
const WATCHDOG_ERROR_STREAK_MIN: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchdogIncidentKind {
    Stalled,
    ErrorLoop,
}

#[derive(Debug, Default)]
struct SubagentWatchdogTracker {
    notified: HashMap<String, WatchdogIncidentKind>,
}

impl SubagentWatchdogTracker {
    /// Strict 1-shot deduplication per `(conversation_id, incident_kind)` while active.
    /// Allows at most one escalation from `Stalled` -> `ErrorLoop`, and prunes finished IDs.
    fn should_notify(&mut self, conv_id: &str, kind: WatchdogIncidentKind) -> bool {
        match self.notified.get(conv_id).copied() {
            Some(prev) if prev == kind => false,
            Some(WatchdogIncidentKind::ErrorLoop) => false,
            _ => {
                self.notified.insert(conv_id.to_string(), kind);
                true
            }
        }
    }

    fn retain_active(&mut self, active_ids: &HashSet<String>) {
        self.notified.retain(|id, _| active_ids.contains(id));
    }
}

fn sanitize_notification_text(raw: &str, max_chars: usize) -> String {
    raw.chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            '"' => '\'',
            '\\' => '/',
            c if c.is_control() => ' ',
            _ => c,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

fn format_watchdog_notification(
    role: &str,
    conv_id: &str,
    kind: WatchdogIncidentKind,
    streak: u32,
    err_summary: Option<&str>,
    idle_secs: u64,
) -> (String, String) {
    let clean_role = if role.trim().is_empty() {
        "subagent"
    } else {
        role.trim()
    };
    let short_id = &conv_id[..conv_id.len().min(8)];
    let title = "AGY Hyper Watchdog".to_string();
    let body = match kind {
        WatchdogIncidentKind::ErrorLoop => {
            let err_note = err_summary
                .filter(|s| !s.trim().is_empty())
                .map(|s| format!(" ({})", sanitize_notification_text(s, 64)))
                .unwrap_or_default();
            format!(
                "{} #{} hit {} consecutive errors{}. Switch to Antigravity to inspect or rescue.",
                clean_role, short_id, streak, err_note
            )
        }
        WatchdogIncidentKind::Stalled => {
            format!(
                "{} #{} has been silent for {}s. Switch to Antigravity to check if it hung.",
                clean_role, short_id, idle_secs
            )
        }
    };
    (title, sanitize_notification_text(&body, 180))
}

fn dispatch_desktop_notification(title: &str, body: &str) {
    if std::env::var_os("AGY_HYPER_NO_NOTIFY").is_some() {
        return;
    }
    let clean_title = sanitize_notification_text(title, 64);
    let clean_body = sanitize_notification_text(body, 180);
    if clean_title.is_empty() || clean_body.is_empty() {
        return;
    }

    #[cfg(target_os = "macos")]
    {
        // Pass title & body via positional argv so AppleScript string injection is impossible.
        let script = "on run argv\n  display notification (item 2 of argv) with title (item 1 of argv)\nend run";
        let _ = std::process::Command::new("osascript")
            .args(["-e", script, "--", &clean_title, &clean_body])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("notify-send")
            .args(["-u", "normal", "-a", "AGY Hyper", &clean_title, &clean_body])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (&clean_title, &clean_body);
    }
}

fn run_watchdog_tick(state: &Arc<Mutex<DaemonState>>, tracker: &mut SubagentWatchdogTracker) {
    let cfg = match state.lock() {
        Ok(guard) => guard.config_clone(),
        Err(_) => return,
    };
    if !cfg.enable_watchdog_notify {
        return;
    }

    let active = discover_all_active_subagents(WATCHDOG_MAX_ACTIVE_AGE_SECS);
    let active_ids: HashSet<String> = active.iter().map(|s| s.conversation_id.clone()).collect();
    tracker.retain_active(&active_ids);
    if active.is_empty() {
        return;
    }

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    for child in active {
        let idle_secs = now_ms.saturating_sub(child.mtime_ms) / 1000;
        let pulse = inspect_transcript_tail(&child.transcript_path, &cfg);
        let streak = pulse
            .as_ref()
            .map(|p| p.consecutive_error_streak)
            .unwrap_or(0);
        let err_summary = pulse.and_then(|p| p.repeat_error_signature);

        let incident = if streak >= WATCHDOG_ERROR_STREAK_MIN {
            Some(WatchdogIncidentKind::ErrorLoop)
        } else if idle_secs >= WATCHDOG_STALLED_SECS {
            Some(WatchdogIncidentKind::Stalled)
        } else {
            None
        };

        let Some(kind) = incident else {
            continue;
        };
        if !tracker.should_notify(&child.conversation_id, kind) {
            continue;
        }

        let (title, body) = format_watchdog_notification(
            &child.agent_name,
            &child.conversation_id,
            kind,
            streak,
            err_summary.as_deref(),
            idle_secs,
        );
        dispatch_desktop_notification(&title, &body);
        if let Ok(mut guard) = state.lock() {
            guard.record_event(
                "watchdog_notify".into(),
                Some(child.conversation_id),
                0,
                0,
                0,
                body,
                false,
            );
        }
    }
}

fn spawn_watchdog_loop(
    state: Arc<Mutex<DaemonState>>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(WATCHDOG_POLL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // skip immediate tick at daemon boot
        let tracker = Arc::new(Mutex::new(SubagentWatchdogTracker::default()));

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() { break; }
                }
                _ = ticker.tick() => {
                    let st = state.clone();
                    let tr = tracker.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Ok(mut guard) = tr.lock() {
                            run_watchdog_tick(&st, &mut guard);
                        }
                    }).await;
                }
            }
        }
    });
}

const RADAR_POLL_SECS: u64 = 3;
const RADAR_MIN_RUN_DURATION_FOR_CHIME_MS: u64 = 8_000;
const FOCUS_REQUEST_TTL_MS: u64 = 15_000;

#[derive(Debug, Default)]
struct RadarRuntimeState {
    seeded: bool,
    last_states: HashMap<String, SessionLiveState>,
    observed_active_since_ms: HashMap<String, u64>,
    notified_gate_keys: HashSet<String>,
    ledger: RadarRuntimeLedger,
    focus_seq: u64,
    pending_focus: Option<FocusSessionRequest>,
}

struct RadarCoordinator {
    inner: Mutex<RadarRuntimeState>,
}

impl RadarCoordinator {
    fn new() -> Self {
        Self {
            inner: Mutex::new(RadarRuntimeState::default()),
        }
    }

    fn snapshot_runtime(&self) -> (RadarRuntimeLedger, Option<FocusSessionRequest>) {
        let now_ms = radar_now_ms();
        let Ok(mut g) = self.inner.lock() else {
            return (RadarRuntimeLedger::default(), None);
        };
        if let Some(ref f) = g.pending_focus {
            if now_ms.saturating_sub(f.requested_at_ms) > FOCUS_REQUEST_TTL_MS {
                g.pending_focus = None;
            }
        }
        (g.ledger.clone(), g.pending_focus.clone())
    }

    fn acknowledge(&self, conversation_id: &str) {
        let cid = conversation_id.trim();
        if cid.is_empty() {
            return;
        }
        if let Ok(mut g) = self.inner.lock() {
            g.ledger
                .acknowledged_at_ms
                .insert(cid.to_string(), radar_now_ms());
        }
    }

    fn queue_focus(
        &self,
        client: &str,
        conversation_id: &str,
        split: Option<String>,
    ) -> Option<FocusSessionRequest> {
        let cid = conversation_id.trim();
        if cid.is_empty() {
            return None;
        }
        let now_ms = radar_now_ms();
        let Ok(mut g) = self.inner.lock() else {
            return None;
        };
        g.ledger
            .acknowledged_at_ms
            .insert(cid.to_string(), now_ms);
        g.focus_seq = g.focus_seq.saturating_add(1);
        let req = FocusSessionRequest {
            seq: g.focus_seq,
            client: if client.trim().is_empty() {
                "agy".to_string()
            } else {
                client.trim().to_ascii_lowercase()
            },
            conversation_id: cid.to_string(),
            split: split.filter(|s| !s.trim().is_empty()),
            requested_at_ms: now_ms,
        };
        g.pending_focus = Some(req.clone());
        Some(req)
    }

    fn consume_focus(&self, seq: u64) {
        if let Ok(mut g) = self.inner.lock() {
            if g.pending_focus.as_ref().is_some_and(|f| f.seq <= seq) {
                g.pending_focus = None;
            }
        }
    }
}

fn dispatch_earcon_sound(sound_name: &str) {
    if std::env::var_os("AGY_HYPER_NO_NOTIFY").is_some() {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let sound_path = format!("/System/Library/Sounds/{}.aiff", sound_name);
        if std::path::Path::new(&sound_path).is_file() {
            let _ = std::process::Command::new("/usr/bin/afplay")
                .args(["-v", "0.58", &sound_path])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = sound_name;
    }
}

fn run_radar_transition_tick(
    state: &Arc<Mutex<DaemonState>>,
    namer: &Arc<NamerCoordinator>,
    radar: &Arc<RadarCoordinator>,
) {
    let cfg = match state.lock() {
        Ok(g) => g.config_clone(),
        Err(_) => return,
    };
    if cfg.session_naming.enabled {
        namer.enqueue_auto_sweep(discover_unmanaged_recent_agy_sessions(12));
    }
    let (ledger_snap, _) = radar.snapshot_runtime();
    let snap = build_session_radar(8, cfg.enable_radar_sound, Some(&ledger_snap), None);
    let now_ms = radar_now_ms();

    let mut pending_alerts: Vec<(&'static str, String, String)> = Vec::new();
    {
        let Ok(mut rt) = radar.inner.lock() else {
            return;
        };
        let mut current_states: HashMap<String, (SessionLiveState, String, String)> = HashMap::new();
        for it in snap.attention_items.iter().chain(snap.recent_items.iter()) {
            let key = format!("{}:{}", it.client, it.id);
            current_states.insert(
                key,
                (
                    it.live_state,
                    it.title.clone(),
                    it.group.clone(),
                ),
            );
        }

        if !rt.seeded {
            rt.seeded = true;
            for (key, (st, _, _)) in &current_states {
                rt.last_states.insert(key.clone(), *st);
                if st.is_active_work() {
                    rt.observed_active_since_ms.insert(key.clone(), now_ms);
                }
            }
            return;
        }

        for (key, (cur_st, title, group)) in &current_states {
            let prev_st = rt
                .last_states
                .get(key)
                .copied()
                .unwrap_or(SessionLiveState::Idle);
            let raw_id = key.split_once(':').map(|(_, id)| id).unwrap_or(key.as_str());

            if cur_st.is_active_work() {
                rt.observed_active_since_ms
                    .entry(key.clone())
                    .or_insert(now_ms);
            }

            match *cur_st {
                SessionLiveState::Waiting if prev_st != SessionLiveState::Waiting => {
                    let gate_key = format!("wait:{}", key);
                    if rt.notified_gate_keys.insert(gate_key) {
                        pending_alerts.push((
                            "Ping",
                            format!("AGY Hyper · Waiting ({})", group),
                            title.clone(),
                        ));
                    }
                }
                SessionLiveState::Stalled
                    if prev_st != SessionLiveState::Stalled
                        && rt.observed_active_since_ms.contains_key(key) =>
                {
                    let gate_key = format!("stall:{}", key);
                    if rt.notified_gate_keys.insert(gate_key) {
                        pending_alerts.push((
                            "Basso",
                            format!("AGY Hyper · Stalled ({})", group),
                            title.clone(),
                        ));
                    }
                }
                SessionLiveState::Idle | SessionLiveState::Unread => {
                    rt.notified_gate_keys.remove(&format!("wait:{}", key));
                    rt.notified_gate_keys.remove(&format!("stall:{}", key));
                    if let Some(started_ms) = rt.observed_active_since_ms.remove(key) {
                        rt.ledger
                            .completed_at_ms
                            .insert(raw_id.to_string(), now_ms);
                        let ran_ms = now_ms.saturating_sub(started_ms);
                        // Only chime if task ran >=8s AND became Unread (suppressed when user is already viewing the chat)
                        if ran_ms >= RADAR_MIN_RUN_DURATION_FOR_CHIME_MS
                            && *cur_st == SessionLiveState::Unread
                        {
                            pending_alerts.push((
                                "Glass",
                                format!("AGY Hyper · Task Complete ({})", group),
                                title.clone(),
                            ));
                        }
                    }
                }
                _ => {}
            }
            rt.last_states.insert(key.clone(), *cur_st);
        }

        // Also check if any previously observed active session dropped off top list -> mark completed
        let finished_keys: Vec<(String, u64)> = rt
            .observed_active_since_ms
            .iter()
            .filter(|(k, _)| !current_states.contains_key(*k))
            .map(|(k, ts)| (k.clone(), *ts))
            .collect();
        for (k, _started) in finished_keys {
            rt.observed_active_since_ms.remove(&k);
            if let Some((_, raw_id)) = k.split_once(':') {
                rt.ledger.completed_at_ms.insert(raw_id.to_string(), now_ms);
            }
        }
    }

    for (sound, alert_title, alert_body) in pending_alerts {
        if cfg.enable_radar_sound {
            dispatch_earcon_sound(sound);
        }
        if cfg.enable_watchdog_notify {
            dispatch_desktop_notification(&alert_title, &alert_body);
        }
    }
}

fn spawn_radar_loop(
    state: Arc<Mutex<DaemonState>>,
    namer: Arc<NamerCoordinator>,
    radar: Arc<RadarCoordinator>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(RADAR_POLL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() { break; }
                }
                _ = ticker.tick() => {
                    let st = state.clone();
                    let nm = namer.clone();
                    let rd = radar.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        run_radar_transition_tick(&st, &nm, &rd);
                    }).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_tracker_deduplicates_oneshot_and_allows_escalation() {
        let mut tr = SubagentWatchdogTracker::default();
        assert!(tr.should_notify("sub-1", WatchdogIncidentKind::Stalled));
        assert!(!tr.should_notify("sub-1", WatchdogIncidentKind::Stalled));
        assert!(tr.should_notify("sub-1", WatchdogIncidentKind::ErrorLoop));
        assert!(!tr.should_notify("sub-1", WatchdogIncidentKind::ErrorLoop));
        assert!(!tr.should_notify("sub-1", WatchdogIncidentKind::Stalled));

        let active: HashSet<String> = HashSet::new();
        tr.retain_active(&active);
        assert!(tr.should_notify("sub-1", WatchdogIncidentKind::Stalled));
    }

    #[test]
    fn watchdog_notification_sanitizes_control_chars_and_quotes() {
        let (title, body) = format_watchdog_notification(
            "browser",
            "aaaaaaaa-1111-2222-3333-444444444444",
            WatchdogIncidentKind::ErrorLoop,
            6,
            Some("API error \"bad\"\n\\EOF"),
            12,
        );
        assert_eq!(title, "AGY Hyper Watchdog");
        assert!(body.contains("browser #aaaaaaaa hit 6 consecutive errors"));
        assert!(!body.contains('"'));
        assert!(!body.contains('\n'));
        assert!(!body.contains('\\'));
    }

    #[test]
    fn parses_audit_share_card_request_defaults() {
        let req: DaemonRequest =
            serde_json::from_str(r#"{"op":"audit_share_card"}"#).expect("valid rpc request");
        match req {
            DaemonRequest::AuditShareCard {
                days,
                max_sessions,
                tz_offset_mins,
                theme,
                include_png,
            } => {
                assert_eq!(days, 14);
                assert_eq!(max_sessions, 80);
                assert_eq!(tz_offset_mins, 0);
                assert_eq!(theme, "dark");
                assert!(!include_png);
            }
            _ => panic!("expected AuditShareCard variant"),
        }
    }
}
