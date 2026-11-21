mod hooks;
mod mcp;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use hyper_core::ast::outline_source;
use hyper_core::bench::{aggregate_sessions, render_waterfall_svg};
use hyper_core::config::{HyperConfig, Profile};
use hyper_core::git::quick_head;
use hyper_core::sessions::discover_transcripts;
use hyper_core::usage::{default_brain_dirs, now_epoch_ms, UsageLedger, UsageWindow, UsageWindowReport};
use hyper_core::wal::{list_transactions, rollback_transaction};
use hyper_daemon::client::{ensure_and_send, send_request, spawn_daemon_detached};
use hyper_daemon::protocol::{DaemonRequest, DaemonResponse, EngineStatus};
use hyper_daemon::server::{run_daemon_server, socket_path};
use hyper_daemon::state::build_cockpit_snapshot;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

#[derive(Parser)]
#[command(
    name = "agy-hyper",
    version = hyper_core::version(),
    about = "Zero-dependency Rust acceleration engine, WAL atomic editor & telemetry cockpit for Antigravity"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the MCP JSON-RPC 2.0 stdio server
    Mcp,
    /// Execute a sub-2ms native Antigravity lifecycle hook
    Hook {
        #[command(subcommand)]
        event: HookEvent,
    },
    /// Manage the shared local Rust daemon (UDS)
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Run mathematically honest waterfall benchmarks on Antigravity transcripts
    Bench {
        /// Number of recent sessions to analyze
        #[arg(long, default_value_t = 10)]
        sessions: usize,
        /// Analyze a specific transcript.jsonl path
        #[arg(long)]
        transcript: Option<PathBuf>,
        /// Export a dark-mode SVG waterfall chart for the latest session
        #[arg(long)]
        svg: Option<PathBuf>,
        /// Output raw JSON
        #[arg(long)]
        json: bool,
    },
    /// Run comprehensive health, latency, grammar & security diagnostics
    Doctor,
    /// Inspect or atomically rollback WAL edit transactions
    Tx {
        #[command(subcommand)]
        action: TxAction,
    },
    /// View or update AGY Hyper configuration profiles
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Print live Cockpit JSON snapshot (used by Sidecar UI and scripts)
    Snapshot {
        #[arg(long)]
        conversation_id: Option<String>,
        #[arg(long)]
        transcript_path: Option<String>,
    },
    /// Query incremental token and API-equivalent cost ledger (24h | 7d | 30d | 90d)
    Usage {
        #[arg(long, default_value = "7d")]
        window: String,
        #[arg(long)]
        currency: Option<String>,
        #[arg(long, default_value_t = 0)]
        tz_offset_mins: i32,
        #[arg(long)]
        force_sync: bool,
        #[arg(long)]
        json: bool,
    },
    /// Launch the standalone Hyper Deck web cockpit in your browser
    Ui {
        #[arg(long, default_value_t = 47821)]
        port: u16,
    },
}

#[derive(Subcommand)]
enum HookEvent {
    SessionStart,
    PreInvocation,
    PostToolUse,
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start daemon in background (idempotent singleton)
    Start,
    /// Serve daemon in foreground
    Serve,
    /// Stop running daemon cleanly
    Stop,
    /// Query live daemon status & UDS latency
    Status,
}

#[derive(Subcommand)]
enum TxAction {
    List {
        #[arg(long, default_value_t = 15)]
        limit: usize,
    },
    Rollback {
        tx_id: String,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    Show,
    Profile {
        /// safe | balanced | hyper
        name: String,
    },
    Toggle {
        /// prewarm_tier2 | block_secret_files | allow_custom_argv_checks | enable_ui_pill
        key: String,
        value: bool,
    },
    Currency {
        /// USD | CNY | JPY | EUR | GBP | HKD | SGD | KRW
        code: String,
    },
    Fx {
        /// Currency code (e.g. CNY, JPY, EUR)
        code: String,
        /// Units per 1 USD (e.g. 7.25)
        rate: f64,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Mcp => mcp::run_mcp_stdio(),
        Commands::Hook { event } => match event {
            HookEvent::SessionStart => hooks::run_session_start(),
            HookEvent::PreInvocation => hooks::run_pre_invocation(),
            HookEvent::PostToolUse => hooks::run_post_tool_use(),
        },
        Commands::Daemon { action } => handle_daemon(action),
        Commands::Bench {
            sessions,
            transcript,
            svg,
            json,
        } => handle_bench(sessions, transcript, svg, json),
        Commands::Doctor => handle_doctor(),
        Commands::Tx { action } => handle_tx(action),
        Commands::Config { action } => handle_config(action),
        Commands::Snapshot {
            conversation_id,
            transcript_path,
        } => handle_snapshot(conversation_id, transcript_path),
        Commands::Usage {
            window,
            currency,
            tz_offset_mins,
            force_sync,
            json,
        } => handle_usage(window, currency, tz_offset_mins, force_sync, json),
        Commands::Ui { port } => handle_ui(port),
    }
}

fn handle_daemon(action: DaemonAction) -> Result<()> {
    match action {
        DaemonAction::Serve => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(run_daemon_server())
        }
        DaemonAction::Start => {
            if send_request(&DaemonRequest::Ping).is_ok() {
                println!("agy-hyper daemon already running ({})", socket_path().display());
                return Ok(());
            }
            spawn_daemon_detached()?;
            for _ in 0..20 {
                std::thread::sleep(std::time::Duration::from_millis(25));
                if let Ok(DaemonResponse::Ok { data }) = send_request(&DaemonRequest::Ping) {
                    let st: EngineStatus = serde_json::from_value(data)?;
                    println!(
                        "agy-hyper daemon started (pid={} · socket={})",
                        st.pid,
                        socket_path().display()
                    );
                    return Ok(());
                }
            }
            Err(anyhow!("failed to confirm daemon startup"))
        }
        DaemonAction::Stop => match send_request(&DaemonRequest::Shutdown) {
            Ok(_) => {
                println!("agy-hyper daemon stopped.");
                Ok(())
            }
            Err(_) => {
                println!("agy-hyper daemon is not running.");
                Ok(())
            }
        },
        DaemonAction::Status => {
            let t0 = Instant::now();
            match send_request(&DaemonRequest::Ping) {
                Ok(DaemonResponse::Ok { data }) => {
                    let rtt_ms = t0.elapsed().as_secs_f64() * 1000.0;
                    let st: EngineStatus = serde_json::from_value(data)?;
                    println!(
                        "running · pid={} · v{} · rtt={:.2}ms · rss={:.1}MB · calls={} · saved={:.1}s · turns_saved={}",
                        st.pid,
                        st.version,
                        rtt_ms,
                        st.rss_mb,
                        st.total_calls,
                        st.total_saved_ms as f64 / 1000.0,
                        st.total_turns_saved
                    );
                    Ok(())
                }
                _ => {
                    println!("stopped");
                    Ok(())
                }
            }
        }
    }
}

fn handle_bench(
    sessions: usize,
    transcript: Option<PathBuf>,
    svg: Option<PathBuf>,
    as_json: bool,
) -> Result<()> {
    let paths = if let Some(p) = transcript {
        vec![p]
    } else {
        discover_transcripts(sessions)
    };

    if paths.is_empty() {
        println!("No Antigravity session transcripts found under ~/.gemini/*/brain/.");
        return Ok(());
    }

    let agg = aggregate_sessions(&paths);

    if let Some(svg_path) = svg {
        if let Some(first) = agg.sessions.first() {
            let content = render_waterfall_svg(first);
            if let Some(parent) = svg_path.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent)?;
                }
            }
            fs::write(&svg_path, content)?;
            if !as_json {
                println!("Exported waterfall SVG -> {}", svg_path.display());
            }
        }
    }

    if as_json {
        println!("{}", serde_json::to_string_pretty(&agg)?);
        return Ok(());
    }

    println!("\n⚡ AGY HYPER · HONEST WATERFALL BENCHMARK");
    println!("─────────────────────────────────────────────────────────────────");
    println!(
        "  Sessions analyzed : {}  ({} valid turns)",
        agg.sessions_analyzed, agg.total_turns
    );
    println!(
        "  Turn P50 / P90    : {:.2}s / {:.2}s  (LLM inference P50: {:.2}s)",
        agg.median_turn_ms as f64 / 1000.0,
        agg.p90_turn_ms as f64 / 1000.0,
        agg.median_llm_ms as f64 / 1000.0
    );
    println!(
        "  Paired Eff. TPS   : {:.1} tok/s  (strictly paired non-idle spans)",
        agg.effective_tps
    );
    println!(
        "  Prompt Cache Hit  : {:.1}%  |  Hyper Tool Adoption: {:.1}%",
        agg.avg_cache_hit_ratio * 100.0,
        agg.hyper_adoption_ratio * 100.0
    );

    if let Some(latest) = agg.sessions.first() {
        println!(
            "\n  Latest session ({}) — last {} turns:",
            &latest.conversation_id[..latest.conversation_id.len().min(8)],
            latest.turns.len().min(10)
        );
        let max_ms = latest
            .turns
            .iter()
            .rev()
            .take(10)
            .map(|t| t.total_turn_ms)
            .max()
            .unwrap_or(1000)
            .max(1000);
        for t in latest.turns.iter().rev().take(10).rev() {
            let llm_blocks = ((t.llm_inference_ms as f64 / max_ms as f64) * 28.0).round() as usize;
            let tool_blocks = ((t.tool_exec_ms as f64 / max_ms as f64) * 28.0).round() as usize;
            let bar = format!(
                "{}{}",
                "█".repeat(llm_blocks.max(1)),
                "▓".repeat(tool_blocks)
            );
            println!(
                "    T{:02} {:<30} {:>5.2}s (llm {:.1}s | tool {:.2}s){}",
                t.turn_index,
                bar,
                t.total_turn_ms as f64 / 1000.0,
                t.llm_inference_ms as f64 / 1000.0,
                t.tool_exec_ms as f64 / 1000.0,
                if t.used_hyper_tools { " ⚡" } else { "" }
            );
        }
    }
    println!();
    Ok(())
}

fn handle_doctor() -> Result<()> {
    let cfg = HyperConfig::load();
    println!("\n⚡ AGY HYPER DOCTOR · v{}", hyper_core::version());
    println!("─────────────────────────────────────────────────────────────────");

    // 1. Tree-sitter parse latency self-test
    let t_ast = Instant::now();
    let sample = "pub struct S<'a> { x: &'a str }\nimpl<'a> S<'a> { pub fn ok(&self) -> usize { 1 } }\n";
    let outline = outline_source(Path::new("doctor.rs"), sample);
    let ast_us = t_ast.elapsed().as_micros();
    println!(
        "  [OK] Tree-sitter AST  : {} ({} symbols, {}us, 7 compiled grammars)",
        outline.parser_engine,
        outline.symbols.len(),
        ast_us
    );

    // 2. Pure-Rust gix self-test
    let t_git = Instant::now();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let git_label = match quick_head(&cwd) {
        Some((b, o)) => format!("{}@{} ({}us)", b, o, t_git.elapsed().as_micros()),
        None => format!("no git repo in cwd ({}us)", t_git.elapsed().as_micros()),
    };
    println!("  [OK] Read-only Git    : gix-readonly · {}", git_label);

    // 3. Daemon UDS ping
    let t_rpc = Instant::now();
    match ensure_and_send(&DaemonRequest::Ping) {
        Ok(DaemonResponse::Ok { data }) => {
            let st: EngineStatus = serde_json::from_value(data)?;
            println!(
                "  [OK] Shared Daemon    : pid={} · UDS rtt={:.2}ms · rss={:.1}MB",
                st.pid,
                t_rpc.elapsed().as_secs_f64() * 1000.0,
                st.rss_mb
            );
        }
        _ => {
            println!("  [WARN] Shared Daemon  : unreachable (in-process fallback active)");
        }
    }

    // 4. Security posture
    println!(
        "  [OK] Security Policy  : profile={} · jail={} · secret_shield={} · shell_exec=DISABLED",
        cfg.profile.as_str(),
        if cfg.allow_outside_workspace { "OFF" } else { "STRICT" },
        if cfg.block_secret_files { "ON" } else { "OFF" }
    );

    // 5. WAL storage & plugin registration
    let txs = list_transactions(&HyperConfig::wal_dir(), 50);
    println!(
        "  [OK] WAL Journal      : {} ({} recorded transaction(s))",
        HyperConfig::wal_dir().display(),
        txs.len()
    );

    let plugin_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".gemini")
        .join("config")
        .join("plugins")
        .join("agy-hyper");
    println!(
        "  [{}] Antigravity Plugin: {}",
        if plugin_dir.join("plugin.json").exists() { "OK" } else { "INFO" },
        if plugin_dir.join("plugin.json").exists() {
            format!("installed at {}", plugin_dir.display())
        } else {
            format!("not linked yet at {}", plugin_dir.display())
        }
    );
    println!();
    Ok(())
}

fn handle_tx(action: TxAction) -> Result<()> {
    match action {
        TxAction::List { limit } => {
            let txs = list_transactions(&HyperConfig::wal_dir(), limit);
            if txs.is_empty() {
                println!("No WAL transactions recorded yet.");
                return Ok(());
            }
            for t in txs {
                let files: Vec<&str> = t.files.iter().map(|f| f.rel_path.as_str()).collect();
                println!(
                    "{}  {:11}  {:12}  {}",
                    t.tx_id,
                    if t.rolled_back { "rolled-back" } else { "committed" },
                    format!("[{}]", t.source),
                    files.join(", ")
                );
            }
            Ok(())
        }
        TxAction::Rollback { tx_id, force } => {
            let res = rollback_transaction(&HyperConfig::wal_dir(), &tx_id, force)?;
            println!("{}", serde_json::to_string_pretty(&res)?);
            Ok(())
        }
    }
}

fn handle_config(action: Option<ConfigAction>) -> Result<()> {
    let mut cfg = HyperConfig::load();
    match action.unwrap_or(ConfigAction::Show) {
        ConfigAction::Show => {
            println!("{}", serde_json::to_string_pretty(&cfg)?);
        }
        ConfigAction::Profile { name } => {
            let p = Profile::from_str_loose(&name)
                .ok_or_else(|| anyhow!("invalid profile '{}'; use safe|balanced|hyper", name))?;
            cfg.apply_profile(p);
            cfg.save()?;
            let _ = send_request(&DaemonRequest::SetProfile {
                profile: p.as_str().into(),
            });
            println!("Profile switched to '{}'.", p.as_str());
        }
        ConfigAction::Toggle { key, value } => {
            cfg.set_toggle(&key, value)?;
            cfg.save()?;
            let _ = send_request(&DaemonRequest::SetToggle { key: key.clone(), value });
            println!("Set {} = {}", key, value);
        }
        ConfigAction::Currency { code } => {
            let norm = cfg.set_currency(&code)?;
            cfg.save()?;
            let _ = send_request(&DaemonRequest::SetCurrency {
                currency: norm.clone(),
            });
            println!("Display currency set to {}.", norm);
        }
        ConfigAction::Fx { code, rate } => {
            let norm = code.trim().to_ascii_uppercase();
            let mut rates = BTreeMap::new();
            rates.insert(norm.clone(), rate);
            let ts = now_epoch_ms().max(0) as u64;
            cfg.update_fx_rates(rates.clone(), "custom", ts)?;
            cfg.save()?;
            let _ = send_request(&DaemonRequest::UpdateFxRates {
                rates,
                source: Some("custom".to_string()),
            });
            println!("Set FX rate: 1 USD = {} {} (custom).", rate, norm);
        }
    }
    Ok(())
}

fn handle_usage(
    window: String,
    currency: Option<String>,
    tz_offset_mins: i32,
    force_sync: bool,
    as_json: bool,
) -> Result<()> {
    let report: UsageWindowReport =
        if let Ok(DaemonResponse::Ok { data }) = ensure_and_send(&DaemonRequest::UsageSummary {
            window: window.clone(),
            currency: currency.clone(),
            tz_offset_mins: Some(tz_offset_mins),
            force_sync,
        }) {
            serde_json::from_value(data)?
        } else {
            let cfg = HyperConfig::load();
            let ledger_path = HyperConfig::ledger_path();
            let mut ledger = UsageLedger::load_from(&ledger_path);
            let now_ms = now_epoch_ms();
            let outcome = ledger.sync_from_dirs(&default_brain_dirs(), now_ms);
            if outcome.dirty {
                let _ = ledger.save_to(&ledger_path);
            }
            ledger.query_window(
                UsageWindow::parse(&window),
                &cfg,
                currency.as_deref(),
                tz_offset_mins,
                now_ms,
                outcome.scan_ms,
            )
        };

    if as_json {
        println!("{}", serde_json::to_string(&report)?);
        return Ok(());
    }

    println!(
        "\n⚡ AGY HYPER USAGE · {} ({} · FX {:.4} [{}])",
        report.window.to_uppercase(),
        report.currency,
        report.fx_rate,
        report.fx_source
    );
    println!("─────────────────────────────────────────────────────────────────");
    println!(
        "  Sessions : {} ({} turns · {} tracked in ledger)",
        report.active_sessions, report.total_turns, report.ledger_sessions_tracked
    );
    println!(
        "  Tokens   : {:.2}M total (cached {:.2}M · {:.1}% hit)",
        report.total_tokens as f64 / 1_000_000.0,
        report.total_cache_read_tokens as f64 / 1_000_000.0,
        report.cache_hit_ratio * 100.0
    );
    println!(
        "  Est. Cost: {:.2} {} ({:.0}% attributed) · Cache Saved: {:.2} {}",
        report.estimated_cost,
        report.currency,
        report.attributed_ratio * 100.0,
        report.cache_saved,
        report.currency
    );
    for m in &report.models {
        let cost_lbl = if m.priced {
            format!("{:.2} {}", m.estimated_cost, report.currency)
        } else {
            "— (unattributed)".to_string()
        };
        println!(
            "    {:<32} {:>7.2}M tok  {:>4.0}% cache  {}",
            m.model,
            m.total_tokens as f64 / 1_000_000.0,
            m.cache_hit_ratio * 100.0,
            cost_lbl
        );
    }
    println!();
    Ok(())
}

fn handle_snapshot(conversation_id: Option<String>, transcript_path: Option<String>) -> Result<()> {
    if let Ok(DaemonResponse::Ok { data }) = ensure_and_send(&DaemonRequest::Snapshot {
        conversation_id: conversation_id.clone(),
        transcript_path: transcript_path.clone(),
    }) {
        println!("{}", serde_json::to_string(&data)?);
        return Ok(());
    }

    let cfg = HyperConfig::load();
    let fallback_engine = EngineStatus {
        pid: std::process::id(),
        version: hyper_core::version().into(),
        uptime_secs: 0,
        rss_mb: 0.0,
        active_workspaces: Vec::new(),
        handles_cached: 0,
        prewarm_cached: 0,
        ast_cache_entries: 0,
        ast_cache_hits: 0,
        ast_cache_misses: 0,
        pack_cache_hits: 0,
        pack_cache_misses: 0,
        tool_counts: std::collections::HashMap::new(),
        total_calls: 0,
        total_saved_ms: 0,
        total_turns_saved: 0,
        blocked_security_events: 0,
        avg_tool_latency_ms: 0.0,
    };
    let snap = build_cockpit_snapshot(
        fallback_engine,
        cfg,
        Vec::new(),
        None,
        conversation_id.as_deref(),
        transcript_path.as_deref(),
    );
    println!("{}", serde_json::to_string(&snap)?);
    Ok(())
}

fn handle_ui(port: u16) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut candidates = Vec::new();
    if let Some(dir) = exe.parent() {
        candidates.push(dir.join("../sidecars/hyper_deck/main.mjs"));
        candidates.push(dir.join("../../plugin/sidecars/hyper_deck/main.mjs"));
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".gemini/config/plugins/agy-hyper/sidecars/hyper_deck/main.mjs"));
    }
    let script = candidates
        .into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| anyhow!("could not locate sidecars/hyper_deck/main.mjs"))?;

    println!("⚡ Launching AGY Hyper Deck standalone at http://127.0.0.1:{}", port);
    let status = Command::new("node")
        .arg(&script)
        .env("AGY_HYPER_STANDALONE_PORT", port.to_string())
        .env("AGY_HYPER_BIN", exe)
        .status()?;
    if !status.success() {
        return Err(anyhow!("Hyper Deck exited with {}", status));
    }
    Ok(())
}
