use crate::protocol::{DaemonRequest, DaemonResponse};
use crate::state::{build_cockpit_snapshot, execute_rollback_unlocked, DaemonState};
use anyhow::{Context, Result};
use fs2::FileExt;
use hyper_core::config::HyperConfig;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
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
            let (engine, config, recent_events, bench_cache) = match state.lock() {
                Ok(g) => g.snapshot_parts(),
                Err(_) => return DaemonResponse::err("daemon state lock poisoned"),
            };
            let snap = build_cockpit_snapshot(
                engine,
                config,
                recent_events,
                Some(&bench_cache),
                conversation_id.as_deref(),
                transcript_path.as_deref(),
            );
            return DaemonResponse::ok(snap);
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
        DaemonRequest::LookupPrewarm { workspace, preset } => {
            match guard.lookup_prewarm(&workspace, &preset) {
                Some(hit) => DaemonResponse::ok(hit),
                None => DaemonResponse::ok(serde_json::Value::Null),
            }
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
        DaemonRequest::Shutdown => {
            guard.flush_stats_now();
            let _ = shutdown_tx.send(true);
            DaemonResponse::ok(serde_json::json!({ "shutting_down": true }))
        }
        DaemonRequest::Snapshot { .. } | DaemonRequest::RollbackTx { .. } => unreachable!(),
    }
}
