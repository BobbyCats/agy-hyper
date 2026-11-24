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
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let sig_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = sig_tx.send(true);
    });
    spawn_watchdog_loop(state.clone(), shutdown_rx.clone());

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() { break; }
            }
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue; };
                let state = state.clone();
                let shutdown_tx = shutdown_tx.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut buf_reader = BufReader::new(reader);
                    let mut line = String::new();
                    while let Ok(n) = buf_reader.read_line(&mut line).await {
                        if n == 0 { break; }
                        let raw = std::mem::take(&mut line);
                        let st = state.clone();
                        let tx = shutdown_tx.clone();
                        let resp = tokio::task::spawn_blocking(move || handle_line(&st, &tx, &raw))
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
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let token = Arc::new(token);

    let sig_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = sig_tx.send(true);
    });
    spawn_watchdog_loop(state.clone(), shutdown_rx.clone());

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() { break; }
            }
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue; };
                let state = state.clone();
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
                        let tx = shutdown_tx.clone();
                        let resp = tokio::task::spawn_blocking(move || handle_line(&st, &tx, &raw))
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
