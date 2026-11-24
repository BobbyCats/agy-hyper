mod hooks;
mod mcp;
mod statusline;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use hyper_core::ast::outline_source;
use hyper_core::audit::{
    assert_share_card_clean, build_audit_report, render_audit_png, render_audit_svg,
    render_share_card_png, render_share_card_svg, BurnAuditReport,
};
use hyper_core::bench::{aggregate_sessions, render_waterfall_svg};
use hyper_core::config::{HyperConfig, Profile};
use hyper_core::git::quick_head;
use hyper_core::sessions::discover_transcripts;
use hyper_core::update::{
    check_latest_release, dismiss_update_version, is_newer_version, load_update_state,
    perform_verified_update, DEFAULT_GITHUB_REPO,
};
use hyper_core::usage::{
    default_brain_dirs, now_epoch_ms, ModelTtlEstimate, UsageLedger, UsageWindow,
    UsageWindowReport,
};
use hyper_core::wal::{inspect_transaction_diff, list_transactions, rollback_transaction};
use hyper_daemon::client::{ensure_and_send, send_request, spawn_daemon_detached};
use hyper_daemon::protocol::{DaemonRequest, DaemonResponse, EngineStatus};
use hyper_daemon::server::{run_daemon_server, socket_path};
use hyper_daemon::state::build_cockpit_snapshot;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
        /// Measure empirical prompt-cache TTL decay curves per model family
        #[arg(long)]
        cache_ttl: bool,
        /// Output raw JSON
        #[arg(long)]
        json: bool,
    },
    /// Run a local multi-session burn audit (cache TTL cliffs, split-cache, pre-edit bloat & PNG/SVG card)
    Audit {
        /// Lookback window in days (1..90)
        #[arg(long, default_value_t = 14)]
        days: u32,
        /// Maximum recent sessions to inspect
        #[arg(long, default_value_t = 80)]
        sessions: usize,
        /// Export a shareable 2x Retina PNG diagnostic report card (1920x1200, pure Rust)
        #[arg(long)]
        png: Option<PathBuf>,
        /// Export a shareable SVG diagnostic report card
        #[arg(long)]
        svg: Option<PathBuf>,
        /// Export an anonymized Health Badge share card (defaults to agy-hyper-share-card.png; .svg writes SVG)
        #[arg(long, num_args = 0..=1, default_missing_value = "agy-hyper-share-card.png")]
        share: Option<PathBuf>,
        /// Export an anonymized Health Badge SVG card (920x500, zero paths/UUIDs/prompts)
        #[arg(long)]
        share_svg: Option<PathBuf>,
        /// Export an anonymized Health Badge 2x Retina PNG card (1920x1080, pure Rust)
        #[arg(long)]
        share_png: Option<PathBuf>,
        /// Card theme for --png / --svg / --share*: dark | light
        #[arg(long, default_value = "dark")]
        theme: String,
        /// Local timezone offset in minutes
        #[arg(long, default_value_t = 0)]
        tz_offset_mins: i32,
        /// Output raw JSON
        #[arg(long)]
        json: bool,
    },
    /// Render a <2ms native statusline or plain-text terminal title for Antigravity CLI
    Statusline {
        /// Output format: compact | rich | json
        #[arg(long, default_value = "compact")]
        format: String,
        /// Render plain single-line terminal title (no ANSI) for Antigravity CLI `title`
        #[arg(long)]
        title: bool,
        /// Disable ANSI colors
        #[arg(long)]
        no_color: bool,
    },
    /// Print (or explicitly apply with --apply) Antigravity CLI statusLine & title config
    CliSetup {
        /// Write to ~/.gemini/antigravity-cli/settings.json (creates a .bak backup first)
        #[arg(long)]
        apply: bool,
        /// Default statusline format: compact | rich
        #[arg(long, default_value = "compact")]
        format: String,
    },
    /// Run comprehensive health, latency, conflict, context-load & network diagnostics
    Doctor {
        /// Probe HTTPS TLS/TTFB latency and flag IPv6 dual-stack blackholes
        #[arg(long)]
        net: bool,
        /// Output structured JSON report
        #[arg(long)]
        json: bool,
    },
    /// Check for new GitHub releases or install a SHA-256 verified update
    Update {
        /// Check GitHub releases without installing
        #[arg(long)]
        check: bool,
        /// Silent background check invoked by SessionStart
        #[arg(long, hide = true)]
        check_quiet: bool,
        /// Dismiss notifications for a specific version (e.g. 0.5.1, or 'clear')
        #[arg(long)]
        dismiss: Option<String>,
        /// Target release version to install (defaults to latest)
        #[arg(long)]
        version: Option<String>,
        /// Output raw JSON
        #[arg(long)]
        json: bool,
    },
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
    Diff {
        tx_id: String,
        #[arg(long, default_value_t = 240)]
        max_lines: usize,
        #[arg(long)]
        json: bool,
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
        /// prewarm_tier2 | block_secret_files | allow_custom_argv_checks | enable_ui_pill | enable_watchdog_notify | enable_update_check | enable_mcp_read | enable_mcp_edit | enable_mcp_check
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
            cache_ttl,
            json,
        } => {
            if cache_ttl {
                handle_bench_cache_ttl(json)
            } else {
                handle_bench(sessions, transcript, svg, json)
            }
        }
        Commands::Audit {
            days,
            sessions,
            png,
            svg,
            share,
            share_svg,
            share_png,
            theme,
            tz_offset_mins,
            json,
        } => handle_audit(
            days,
            sessions,
            png,
            svg,
            share,
            share_svg,
            share_png,
            &theme,
            tz_offset_mins,
            json,
        ),
        Commands::Statusline {
            format,
            title,
            no_color,
        } => statusline::run_statusline(&format, title, no_color),
        Commands::CliSetup { apply, format } => statusline::run_cli_setup(apply, &format),
        Commands::Doctor { net, json } => handle_doctor(net, json),
        Commands::Update {
            check,
            check_quiet,
            dismiss,
            version,
            json,
        } => handle_update(check, check_quiet, dismiss, version, json),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DoctorCheckItem {
    id: String,
    status: String,
    label: String,
    detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DoctorReport {
    version: String,
    profile: String,
    ok_count: usize,
    warn_count: usize,
    info_count: usize,
    checks: Vec<DoctorCheckItem>,
}

#[derive(Debug, Clone, Default)]
struct ContextBaselineDiag {
    agents_skill_count: usize,
    gemini_skill_count: usize,
    total_skill_files: usize,
    duplicate_skill_folders: Vec<String>,
    mcp_server_count: usize,
}

#[derive(Debug, Clone)]
struct NetProbeSample {
    ok: bool,
    http_code: u16,
    tls_ms: u64,
    ttfb_ms: u64,
    total_ms: u64,
    error: Option<String>,
}

fn has_competing_lifecycle_hooks(val: &serde_json::Value) -> Vec<&'static str> {
    let mut found = Vec::new();
    let Some(obj) = val.as_object() else {
        return found;
    };
    if obj.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
        return found;
    }

    let check_obj = |map: &serde_json::Map<String, serde_json::Value>, out: &mut Vec<&'static str>| {
        if map.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
            return;
        }
        for key in ["PreInvocation", "PostToolUse"] {
            if map
                .get(key)
                .and_then(|v| v.as_array())
                .is_some_and(|arr| !arr.is_empty())
                && !out.contains(&key)
            {
                out.push(key);
            }
        }
    };

    check_obj(obj, &mut found);
    for child in obj.values() {
        if let Some(sub) = child.as_object() {
            check_obj(sub, &mut found);
        }
    }
    found
}

fn detect_anti_mcp_directive(markdown: &str) -> Option<String> {
    for raw_line in markdown.lines() {
        let normalized: String = raw_line
            .chars()
            .map(|c| match c {
                '`' | '"' | '\'' | '*' => ' ',
                _ => c.to_ascii_lowercase(),
            })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if normalized.is_empty() {
            continue;
        }
        let compact = normalized
            .replace(" / ", "/")
            .replace("/ ", "/")
            .replace(" /", "/");

        let patterns = [
            "only use read_file",
            "only use edit_file",
            "use only read_file",
            "use only edit_file",
            "only use native read_file",
            "only use native edit_file",
            "only use built-in read_file",
            "exclusively use read_file",
            "never use mcp",
            "do not use mcp",
            "don t use mcp",
            "avoid mcp tools",
            "do not call mcp",
            "never call_mcp_tool",
            "do not use call_mcp_tool",
        ];
        if patterns.iter().any(|p| compact.contains(p)) {
            let snippet: String = raw_line.trim().chars().take(72).collect();
            return Some(snippet);
        }
    }
    None
}

fn audit_plugin_conflicts(plugins_root: &Path, mcp_accel_enabled: bool) -> (usize, Vec<String>) {
    let Ok(entries) = fs::read_dir(plugins_root) else {
        return (0, Vec::new());
    };
    let mut scanned = 0usize;
    let mut conflicts = Vec::new();
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    dirs.sort();

    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let Some(name) = dir.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name == "agy-hyper"
            || name.starts_with('.')
            || name.starts_with("agy-hyper.bak.")
            || name.contains(".bak.")
        {
            continue;
        }
        scanned += 1;

        for rel_hook in ["hooks.json", "hooks/hooks.json"] {
            let hook_path = dir.join(rel_hook);
            if let Ok(raw) = fs::read_to_string(&hook_path) {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) {
                    let events = has_competing_lifecycle_hooks(&val);
                    if !events.is_empty() {
                        conflicts.push(format!(
                            "{}/{} (competing {} hook)",
                            name,
                            rel_hook,
                            events.join("+")
                        ));
                    }
                }
            }
        }

        if mcp_accel_enabled {
            let mut rule_files = Vec::new();
            let rules_dir = dir.join("rules");
            if let Ok(r_entries) = fs::read_dir(&rules_dir) {
                for re in r_entries.flatten() {
                    let p = re.path();
                    if p.extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
                    {
                        rule_files.push((
                            format!(
                                "{}/rules/{}",
                                name,
                                p.file_name().and_then(|s| s.to_str()).unwrap_or("rule.md")
                            ),
                            p,
                        ));
                    }
                }
            }
            rule_files.sort_by(|a, b| a.0.cmp(&b.0));
            for (rel_display, path) in rule_files {
                if let Ok(raw) = fs::read_to_string(&path) {
                    if let Some(snippet) = detect_anti_mcp_directive(&raw) {
                        conflicts.push(format!(
                            "{} (anti-MCP directive: \"{}\")",
                            rel_display, snippet
                        ));
                    }
                }
            }
        }
    }

    (scanned, conflicts)
}

fn count_skills_in_root(root: &Path) -> (usize, BTreeSet<String>) {
    let mut count = 0usize;
    let mut folder_names = BTreeSet::new();
    let Ok(entries) = fs::read_dir(root) else {
        return (0, folder_names);
    };

    fn walk_skill_dir(dir: &Path, depth: usize, count: &mut usize) {
        if depth > 3 {
            return;
        }
        if fs::metadata(dir.join("SKILL.md")).is_ok_and(|m| m.is_file()) {
            *count += 1;
        }
        if depth == 3 {
            return;
        }
        let Ok(children) = fs::read_dir(dir) else {
            return;
        };
        for entry in children.flatten() {
            let p = entry.path();
            let Some(fname) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if fname.starts_with('.') || fname == "node_modules" || fname == "references" || fname == "scripts" {
                continue;
            }
            if fs::metadata(&p).is_ok_and(|m| m.is_dir()) {
                walk_skill_dir(&p, depth + 1, count);
            }
        }
    }

    for entry in entries.flatten() {
        let p = entry.path();
        let Some(fname) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if fname.starts_with('.') {
            continue;
        }
        if fs::metadata(&p).is_ok_and(|m| m.is_dir()) {
            let before = count;
            walk_skill_dir(&p, 1, &mut count);
            if count > before {
                folder_names.insert(fname.to_string());
            }
        }
    }
    (count, folder_names)
}

fn inspect_context_baseline(
    agents_skills_root: &Path,
    gemini_skills_root: &Path,
    mcp_config_path: &Path,
) -> ContextBaselineDiag {
    let (agents_skill_count, agents_folders) = count_skills_in_root(agents_skills_root);
    let (gemini_skill_count, gemini_folders) = count_skills_in_root(gemini_skills_root);
    let duplicate_skill_folders: Vec<String> = agents_folders
        .intersection(&gemini_folders)
        .cloned()
        .collect();

    let mcp_server_count = fs::read_to_string(mcp_config_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|val| {
            val.get("mcpServers")
                .and_then(|m| m.as_object())
                .map(|m| m.len())
        })
        .unwrap_or(0);

    ContextBaselineDiag {
        agents_skill_count,
        gemini_skill_count,
        total_skill_files: agents_skill_count + gemini_skill_count,
        duplicate_skill_folders,
        mcp_server_count,
    }
}

fn run_curl_net_probe(force_ipv4: bool) -> NetProbeSample {
    let curl_bin = if cfg!(windows) { "curl.exe" } else { "curl" };
    let null_dev = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let mut cmd = Command::new(curl_bin);
    if force_ipv4 {
        cmd.arg("-4");
    }
    cmd.args([
        "-fsS",
        "-o",
        null_dev,
        "-w",
        "%{http_code} %{time_appconnect} %{time_starttransfer} %{time_total}",
        "--max-time",
        "5",
        "https://www.gstatic.com/generate_204",
    ])
    .stdin(Stdio::null());

    let t0 = Instant::now();
    match cmd.output() {
        Ok(out) => {
            let wall_ms = t0.elapsed().as_millis() as u64;
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                let mut cols = text.split_whitespace();
                let code = cols
                    .next()
                    .and_then(|s| s.parse::<u16>().ok())
                    .unwrap_or(0);
                let tls_ms = cols
                    .next()
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|sec| (sec * 1000.0).round() as u64)
                    .unwrap_or(0);
                let ttfb_ms = cols
                    .next()
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|sec| (sec * 1000.0).round() as u64)
                    .unwrap_or(wall_ms);
                let total_ms = cols
                    .next()
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|sec| (sec * 1000.0).round() as u64)
                    .unwrap_or(wall_ms);
                NetProbeSample {
                    ok: (200..400).contains(&code),
                    http_code: code,
                    tls_ms,
                    ttfb_ms,
                    total_ms,
                    error: None,
                }
            } else {
                let stderr = String::from_utf8_lossy(&out.stderr);
                NetProbeSample {
                    ok: false,
                    http_code: 0,
                    tls_ms: 0,
                    ttfb_ms: wall_ms,
                    total_ms: wall_ms,
                    error: Some(stderr.trim().to_string()),
                }
            }
        }
        Err(e) => NetProbeSample {
            ok: false,
            http_code: 0,
            tls_ms: 0,
            ttfb_ms: 0,
            total_ms: t0.elapsed().as_millis() as u64,
            error: Some(e.to_string()),
        },
    }
}

fn evaluate_dual_stack_probe(ipv4: &NetProbeSample, auto_stack: &NetProbeSample) -> (String, String) {
    if ipv4.ok && (!auto_stack.ok || auto_stack.total_ms.saturating_sub(ipv4.total_ms) > 2_500) {
        let auto_desc = if auto_stack.ok {
            format!("lagged at {}ms", auto_stack.total_ms)
        } else {
            format!(
                "failed/timed out after {}ms ({})",
                auto_stack.total_ms,
                auto_stack.error.as_deref().unwrap_or("timeout")
            )
        };
        return (
            "warn".to_string(),
            format!(
                "IPv6 blackhole suspected: IPv4 succeeded in {}ms (TLS {}ms, TTFB {}ms) while dual-stack {} — prefer IPv4 or fix broken IPv6 routing",
                ipv4.total_ms, ipv4.tls_ms, ipv4.ttfb_ms, auto_desc
            ),
        );
    }
    if !ipv4.ok && !auto_stack.ok {
        return (
            "warn".to_string(),
            format!(
                "HTTPS probe failed on both stacks (auto: {}, ipv4: {})",
                auto_stack.error.as_deref().unwrap_or("error"),
                ipv4.error.as_deref().unwrap_or("error")
            ),
        );
    }
    (
        "ok".to_string(),
        format!(
            "dual-stack HTTP {} {}ms (TLS {}ms, TTFB {}ms) · IPv4 HTTP {} {}ms (TLS {}ms, TTFB {}ms)",
            auto_stack.http_code,
            auto_stack.total_ms,
            auto_stack.tls_ms,
            auto_stack.ttfb_ms,
            ipv4.http_code,
            ipv4.total_ms,
            ipv4.tls_ms,
            ipv4.ttfb_ms
        ),
    )
}

fn handle_doctor(with_net: bool, as_json: bool) -> Result<()> {
    let cfg = HyperConfig::load();
    let home = dirs::home_dir();
    let mut checks: Vec<DoctorCheckItem> = Vec::new();

    // 1. Tree-sitter parse latency self-test
    let t_ast = Instant::now();
    let sample = "pub struct S<'a> { x: &'a str }\nimpl<'a> S<'a> { pub fn ok(&self) -> usize { 1 } }\n";
    let outline = outline_source(Path::new("doctor.rs"), sample);
    let ast_us = t_ast.elapsed().as_micros();
    checks.push(DoctorCheckItem {
        id: "tree_sitter_ast".into(),
        status: "ok".into(),
        label: "Tree-sitter AST".into(),
        detail: format!(
            "{} ({} symbols, {}us, 7 compiled grammars)",
            outline.parser_engine,
            outline.symbols.len(),
            ast_us
        ),
    });

    // 2. Pure-Rust gix self-test
    let t_git = Instant::now();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let git_label = match quick_head(&cwd) {
        Some((b, o)) => format!("{}@{} ({}us)", b, o, t_git.elapsed().as_micros()),
        None => format!("no git repo in cwd ({}us)", t_git.elapsed().as_micros()),
    };
    checks.push(DoctorCheckItem {
        id: "readonly_git".into(),
        status: "ok".into(),
        label: "Read-only Git".into(),
        detail: format!("gix-readonly · {}", git_label),
    });

    // 3. Daemon UDS ping
    let t_rpc = Instant::now();
    match ensure_and_send(&DaemonRequest::Ping) {
        Ok(DaemonResponse::Ok { data }) => {
            let st: EngineStatus = serde_json::from_value(data)?;
            checks.push(DoctorCheckItem {
                id: "shared_daemon".into(),
                status: "ok".into(),
                label: "Shared Daemon".into(),
                detail: format!(
                    "pid={} · UDS rtt={:.2}ms · rss={:.1}MB",
                    st.pid,
                    t_rpc.elapsed().as_secs_f64() * 1000.0,
                    st.rss_mb
                ),
            });
        }
        _ => {
            checks.push(DoctorCheckItem {
                id: "shared_daemon".into(),
                status: "warn".into(),
                label: "Shared Daemon".into(),
                detail: "unreachable (in-process fallback active)".into(),
            });
        }
    }

    // 4. Security posture
    checks.push(DoctorCheckItem {
        id: "security_policy".into(),
        status: "ok".into(),
        label: "Security Policy".into(),
        detail: format!(
            "profile={} · jail={} · secret_shield={} · shell_exec=DISABLED",
            cfg.profile.as_str(),
            if cfg.allow_outside_workspace { "OFF" } else { "STRICT" },
            if cfg.block_secret_files { "ON" } else { "OFF" }
        ),
    });

    // 5. WAL storage
    let txs = list_transactions(&HyperConfig::wal_dir(), 50);
    checks.push(DoctorCheckItem {
        id: "wal_journal".into(),
        status: "ok".into(),
        label: "WAL Journal".into(),
        detail: format!(
            "{} ({} recorded transaction(s))",
            HyperConfig::wal_dir().display(),
            txs.len()
        ),
    });

    // 6. Antigravity Plugin registration
    let home_base = home.clone().unwrap_or_default();
    let plugins_root = home_base.join(".gemini").join("config").join("plugins");
    let plugin_dir = plugins_root.join("agy-hyper");
    let plugin_installed = plugin_dir.join("plugin.json").exists();
    checks.push(DoctorCheckItem {
        id: "antigravity_plugin".into(),
        status: if plugin_installed { "ok" } else { "info" }.into(),
        label: "Antigravity Plugin".into(),
        detail: if plugin_installed {
            format!("installed at {}", plugin_dir.display())
        } else {
            format!("not linked yet at {}", plugin_dir.display())
        },
    });

    // 7. Plugin Conflict & Hook Audit
    let (other_plugins, conflicts) =
        audit_plugin_conflicts(&plugins_root, cfg.any_mcp_accel_enabled());
    if conflicts.is_empty() {
        checks.push(DoctorCheckItem {
            id: "plugin_conflicts".into(),
            status: "ok".into(),
            label: "Plugin Conflicts".into(),
            detail: format!(
                "none detected across {} other plugin(s)",
                other_plugins
            ),
        });
    } else {
        checks.push(DoctorCheckItem {
            id: "plugin_conflicts".into(),
            status: "warn".into(),
            label: "Plugin Conflicts".into(),
            detail: conflicts.join("; "),
        });
    }

    // 8. Context Baseline Load
    let ctx_diag = inspect_context_baseline(
        &home_base.join(".agents").join("skills"),
        &home_base.join(".gemini").join("config").join("skills"),
        &home_base
            .join(".gemini")
            .join("config")
            .join("mcp_config.json"),
    );
    let dup_note = if ctx_diag.duplicate_skill_folders.is_empty() {
        String::new()
    } else {
        let preview: Vec<&str> = ctx_diag
            .duplicate_skill_folders
            .iter()
            .take(4)
            .map(String::as_str)
            .collect();
        let more = if ctx_diag.duplicate_skill_folders.len() > 4 {
            format!(", +{} more", ctx_diag.duplicate_skill_folders.len() - 4)
        } else {
            String::new()
        };
        format!(
            " · {} duplicate skill folder(s) across roots ({}{})",
            ctx_diag.duplicate_skill_folders.len(),
            preview.join(", "),
            more
        )
    };
    let trunc_note = if ctx_diag.total_skill_files > 120 {
        " [>120 SKILL.md risks context-budget truncation]"
    } else {
        ""
    };
    checks.push(DoctorCheckItem {
        id: "context_baseline".into(),
        status: if ctx_diag.total_skill_files > 120 {
            "warn"
        } else {
            "ok"
        }
        .into(),
        label: "Context Baseline".into(),
        detail: format!(
            "{} SKILL.md ({} ~/.agents + {} ~/.gemini/config){} · {} MCP server(s){}",
            ctx_diag.total_skill_files,
            ctx_diag.agents_skill_count,
            ctx_diag.gemini_skill_count,
            dup_note,
            ctx_diag.mcp_server_count,
            trunc_note
        ),
    });

    // 9. Workspace Safety
    let is_broad_cwd = hooks::is_home_or_root_workspace(&cwd, home.as_deref());
    checks.push(DoctorCheckItem {
        id: "workspace_safety".into(),
        status: if is_broad_cwd { "warn" } else { "ok" }.into(),
        label: "Workspace Safety".into(),
        detail: if is_broad_cwd {
            format!(
                "cwd is $HOME or filesystem root ({}) — open a specific project directory",
                cwd.display()
            )
        } else {
            format!("project-scoped cwd ({})", cwd.display())
        },
    });

    // 10. Release Status
    let cur_ver = hyper_core::version();
    let upd_state = load_update_state(cur_ver);
    let (rel_status, rel_detail) = match upd_state.latest_version.as_deref() {
        Some(lat) if upd_state.update_available && is_newer_version(cur_ver, lat) => (
            "info",
            format!(
                "update v{} available (current v{} — run 'agy-hyper update')",
                lat, cur_ver
            ),
        ),
        Some(_) => ("ok", format!("up to date (v{})", cur_ver)),
        None => ("ok", format!("v{} (release radar idle)", cur_ver)),
    };
    checks.push(DoctorCheckItem {
        id: "release_status".into(),
        status: rel_status.into(),
        label: "Release Status".into(),
        detail: rel_detail,
    });

    // 11. Optional --net Stream & Dual-Stack Probe
    if with_net {
        let ipv4 = run_curl_net_probe(true);
        let auto_stack = run_curl_net_probe(false);
        let (net_status, net_detail) = evaluate_dual_stack_probe(&ipv4, &auto_stack);
        checks.push(DoctorCheckItem {
            id: "network_probe".into(),
            status: net_status,
            label: "Stream & Dual-Stack".into(),
            detail: net_detail,
        });
    }

    let ok_count = checks.iter().filter(|c| c.status == "ok").count();
    let warn_count = checks.iter().filter(|c| c.status == "warn").count();
    let info_count = checks.iter().filter(|c| c.status == "info").count();
    let report = DoctorReport {
        version: cur_ver.to_string(),
        profile: cfg.profile.as_str().to_string(),
        ok_count,
        warn_count,
        info_count,
        checks,
    };

    if as_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("\n⚡ AGY HYPER DOCTOR · v{}", report.version);
    println!("─────────────────────────────────────────────────────────────────");
    for item in &report.checks {
        let badge = match item.status.as_str() {
            "warn" => "[WARN]",
            "info" => "[INFO]",
            _ => "[OK]  ",
        };
        println!("  {} {:<19}: {}", badge, item.label, item.detail);
    }
    println!();
    Ok(())
}

fn handle_update(
    check: bool,
    check_quiet: bool,
    dismiss: Option<String>,
    version: Option<String>,
    as_json: bool,
) -> Result<()> {
    let repo = std::env::var("AGY_HYPER_REPO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_GITHUB_REPO.to_string());
    let current_ver = hyper_core::version();

    if check_quiet {
        let cfg = HyperConfig::load();
        if !cfg.enable_update_check || std::env::var_os("AGY_HYPER_NO_UPDATE_CHECK").is_some() {
            return Ok(());
        }
        let _ = check_latest_release(&repo, current_ver);
        return Ok(());
    }

    if let Some(dismiss_ver) = dismiss {
        let st = dismiss_update_version(&dismiss_ver, current_ver)?;
        if as_json {
            println!("{}", serde_json::to_string_pretty(&st)?);
        } else if let Some(v) = &st.dismissed_version {
            println!("Dismissed update notifications for v{}.", v);
        } else {
            println!("Cleared dismissed update version.");
        }
        return Ok(());
    }

    if check {
        let st = check_latest_release(&repo, current_ver)?;
        if as_json {
            println!("{}", serde_json::to_string_pretty(&st)?);
        } else if st.update_available {
            let latest = st.latest_version.as_deref().unwrap_or("latest");
            println!(
                "⚡ Update available: v{} -> v{} ({})",
                st.current_version,
                latest,
                st.release_url.as_deref().unwrap_or("")
            );
            println!("  Run 'agy-hyper update' to install (SHA-256 verified).");
        } else {
            println!("✓ AGY Hyper v{} is up to date.", st.current_version);
        }
        return Ok(());
    }

    if !as_json {
        println!(
            "⚡ Updating AGY Hyper ({})...",
            version.as_deref().unwrap_or("latest")
        );
    }
    let outcome = perform_verified_update(&repo, version.as_deref())?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        println!(
            "✓ Installed AGY Hyper v{} (was v{}) at {} [SHA-256 verified]",
            outcome.installed_version, outcome.previous_version, outcome.plugin_dir
        );
    }
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
        TxAction::Diff {
            tx_id,
            max_lines,
            json,
        } => {
            let diff = inspect_transaction_diff(&HyperConfig::wal_dir(), &tx_id, max_lines)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&diff)?);
                return Ok(());
            }
            println!(
                "WAL Diff · {} [{}] {}",
                diff.tx_id,
                diff.source,
                if diff.rolled_back { "(rolled-back)" } else { "" }
            );
            for f in &diff.files {
                println!(
                    "\n── {} [{}] (+{} -{})",
                    f.rel_path, f.action, f.lines_added, f.lines_removed
                );
                if let Some(reason) = &f.unavailable_reason {
                    println!("  ({})", reason);
                } else if f.unified_diff.is_empty() {
                    println!("  (no line changes)");
                } else {
                    println!("{}", f.unified_diff);
                    if f.truncated {
                        println!("  … (diff truncated at {} lines)", max_lines);
                    }
                }
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
            let p = Profile::from_str_loose(&name).ok_or_else(|| {
                anyhow!(
                    "invalid profile '{}'; use observe|safe|balanced|hyper",
                    name
                )
            })?;
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
        None,
        conversation_id.as_deref(),
        transcript_path.as_deref(),
    );
    println!("{}", serde_json::to_string(&snap)?);
    Ok(())
}

fn handle_bench_cache_ttl(as_json: bool) -> Result<()> {
    let estimates: Vec<ModelTtlEstimate> = if let Ok(DaemonResponse::Ok { data }) =
        ensure_and_send(&DaemonRequest::CacheTtlReport { force_sync: true })
    {
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
        ledger.all_model_ttl_estimates(cfg.cache_warm_window_secs)
    };

    if as_json {
        println!("{}", serde_json::to_string_pretty(&estimates)?);
        return Ok(());
    }

    println!("\n⚡ AGY HYPER · EMPIRICAL PROMPT-CACHE TTL RADAR");
    println!("─────────────────────────────────────────────────────────────────");
    if estimates.is_empty() {
        println!("  No inter-turn cache observations yet across ~/.gemini/*/brain/.\n");
        return Ok(());
    }
    for est in &estimates {
        println!(
            "  {:<22} effective TTL: {:>3}s [{}]  (n={} turns)",
            est.model_family, est.effective_ttl_secs, est.source, est.total_samples
        );
        for b in &est.buckets {
            if b.samples == 0 {
                continue;
            }
            let filled = ((b.avg_hit_ratio * 16.0).round() as usize).min(16);
            let bar = format!("{}{}", "█".repeat(filled), "░".repeat(16 - filled));
            let cliff = if b.samples >= 5 && b.cold_rate > 0.25 {
                " ◄ CLIFF"
            } else {
                ""
            };
            println!(
                "    {:<8} {}  hit {:>5.1}%  cold {:>5.1}%  (n={}){}",
                b.label,
                bar,
                b.avg_hit_ratio * 100.0,
                b.cold_rate * 100.0,
                b.samples,
                cliff
            );
        }
        println!();
    }
    Ok(())
}

fn write_output_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(path, bytes)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_audit(
    days: u32,
    sessions: usize,
    png: Option<PathBuf>,
    svg: Option<PathBuf>,
    share: Option<PathBuf>,
    share_svg: Option<PathBuf>,
    share_png: Option<PathBuf>,
    theme: &str,
    tz_offset_mins: i32,
    as_json: bool,
) -> Result<()> {
    let cfg = HyperConfig::load();
    let bypass_daemon = std::env::var_os("AGY_HYPER_BRAIN_DIR").is_some();
    let report: BurnAuditReport = if bypass_daemon {
        build_audit_report(days, sessions, &cfg, tz_offset_mins)
    } else {
        match send_request(&DaemonRequest::AuditReport {
            days,
            max_sessions: sessions,
            tz_offset_mins,
        }) {
            Ok(DaemonResponse::Ok { data }) => serde_json::from_value(data)
                .unwrap_or_else(|_| build_audit_report(days, sessions, &cfg, tz_offset_mins)),
            _ => build_audit_report(days, sessions, &cfg, tz_offset_mins),
        }
    };

    let dark = !theme.trim().eq_ignore_ascii_case("light");
    if let Some(png_path) = png {
        let png_bytes = render_audit_png(&report, dark)?;
        write_output_file(&png_path, &png_bytes)?;
        if !as_json {
            println!(
                "Exported shareable 2x PNG audit card -> {} (1920x1200)",
                png_path.display()
            );
        }
    }

    if let Some(svg_path) = svg {
        let svg_xml = render_audit_svg(&report, dark);
        write_output_file(&svg_path, svg_xml.as_bytes())?;
        if !as_json {
            println!("Exported shareable SVG audit card -> {}", svg_path.display());
        }
    }

    if let Some(share_path) = share {
        let is_svg = share_path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"));
        if is_svg {
            let svg_xml = render_share_card_svg(&report, dark);
            assert_share_card_clean(&svg_xml)?;
            write_output_file(&share_path, svg_xml.as_bytes())?;
            if !as_json {
                println!(
                    "Exported anonymized Health Badge SVG -> {} (920x500 · 0 paths/UUIDs)",
                    share_path.display()
                );
            }
        } else {
            let png_bytes = render_share_card_png(&report, dark)?;
            write_output_file(&share_path, &png_bytes)?;
            if !as_json {
                println!(
                    "Exported anonymized Health Badge PNG -> {} (1920x1080 · 0 paths/UUIDs)",
                    share_path.display()
                );
            }
        }
    }

    if let Some(svg_path) = share_svg {
        let svg_xml = render_share_card_svg(&report, dark);
        assert_share_card_clean(&svg_xml)?;
        write_output_file(&svg_path, svg_xml.as_bytes())?;
        if !as_json {
            println!(
                "Exported anonymized Health Badge SVG -> {} (920x500 · 0 paths/UUIDs)",
                svg_path.display()
            );
        }
    }

    if let Some(png_path) = share_png {
        let png_bytes = render_share_card_png(&report, dark)?;
        write_output_file(&png_path, &png_bytes)?;
        if !as_json {
            println!(
                "Exported anonymized Health Badge PNG -> {} (1920x1080 · 0 paths/UUIDs)",
                png_path.display()
            );
        }
    }

    if as_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!(
        "\n⚡ AGY HYPER · LOCAL TELEMETRY BURN AUDIT ({}d) · [{}]",
        report.window_days, report.verdict
    );
    println!("─────────────────────────────────────────────────────────────────");
    if !report.verdict_reason.is_empty() {
        println!("  Verdict           : {} — {}", report.verdict, report.verdict_reason);
    }
    println!(
        "  Sessions / Turns  : {} sessions ({} coding) · {} valid turns",
        report.sessions_analyzed, report.coding_sessions, report.total_turns
    );
    println!(
        "  Prompt Cache Hit  : {:.1}%  ({} cold-cache breaks)",
        report.overall_cache_hit_ratio * 100.0,
        report.cache_break_turns
    );
    println!(
        "  Uncached Input    : {:.2} {} (API-equiv {:.2} {} · cache saved {:.2} {})",
        report.uncached_input_cost,
        report.currency,
        report.total_api_equiv,
        report.currency,
        report.cache_saved,
        report.currency
    );
    if let Some(p50) = report.first_edit_p50_tokens {
        let p90 = report.first_edit_p90_tokens.unwrap_or(p50);
        println!(
            "  First-Edit Context: P50 {:.1}k · P90 {:.1}k · Blanket-read turns: {} (in {} sess)",
            p50 as f64 / 1000.0,
            p90 as f64 / 1000.0,
            report.blanket_read_turns,
            report.sessions_with_blanket_reads
        );
    }
    if let Some(line) = report.transport_summary_line() {
        println!("{}", line);
    }
    let sc = &report.split_cache;
    if sc.total_transitions > 0 {
        println!("\n  Split-Session / Model-Switch Cache Diagnostics:");
        println!(
            "    Orchestrator  : {:>3} sess · {:>4} turns · Cache {:>5.1}%",
            sc.orchestrator_sessions,
            sc.orchestrator_turns,
            sc.orchestrator_cache_hit_ratio * 100.0
        );
        if sc.executor_sessions > 0 {
            println!(
                "    Subagent Exec : {:>3} sess · {:>4} turns · Spawn {:>5.1}% -> Steady {:>5.1}% ({} cross-model)",
                sc.executor_sessions,
                sc.executor_turns,
                sc.executor_cold_start_hit_ratio * 100.0,
                sc.executor_steady_hit_ratio * 100.0,
                sc.executor_cross_model_handoffs
            );
        }
        if sc.model_switch_count > 0 {
            println!(
                "    Model Switches: {} in {} sess ({} cross-family) · {:.1}% -> {:.1}% (-{:.1}pp)",
                sc.model_switch_count,
                sc.model_switch_sessions,
                sc.cross_family_switch_count,
                sc.model_switch_pre_hit_ratio * 100.0,
                sc.model_switch_post_hit_ratio * 100.0,
                sc.model_switch_drop_pct
            );
        }
        println!(
            "    Transition Tax: -{:.1}pp avg drop · {:.1}k uncached penalty (~{:.2} {})",
            sc.headline_cache_drop_pct,
            sc.uncached_penalty_tokens as f64 / 1000.0,
            sc.est_uncached_penalty,
            report.currency
        );
    } else {
        println!("\n  Split-Session / Model-Switch Cache Diagnostics:");
        println!("    No model switches or subagent splits detected in window.");
    }
    if !report.model_ttl_cliffs.is_empty() {
        println!("\n  Empirical Model Cache TTL Cliffs:");
        for m in report.model_ttl_cliffs.iter().take(6) {
            println!(
                "    {:<20} {:>3}s [{}]  (n={})",
                m.model_family, m.effective_ttl_secs, m.source, m.total_samples
            );
        }
    }
    let nat = &report.comparison.native_only;
    let hyp = &report.comparison.with_hyper;
    println!("\n  Observed Local Cohorts (unadjusted local history):");
    println!(
        "    Native-only : {:>3} sess · {:>4} turns · Turn P50 {:>5.1}s · Cache {:>5.1}%",
        nat.sessions,
        nat.total_turns,
        nat.turn_p50_ms as f64 / 1000.0,
        nat.cache_hit_ratio * 100.0
    );
    println!(
        "    With Hyper  : {:>3} sess · {:>4} turns · Turn P50 {:>5.1}s · Cache {:>5.1}%",
        hyp.sessions,
        hyp.total_turns,
        hyp.turn_p50_ms as f64 / 1000.0,
        hyp.cache_hit_ratio * 100.0
    );
    println!("\n  Top Local Findings:");
    for f in &report.findings {
        let tag = match f.severity.as_str() {
            "high" => "[HIGH]",
            "warn" => "[WARN]",
            _ => "[INFO]",
        };
        println!("    {} {}", tag, f.title);
        println!("           {}", f.detail);
    }
    println!();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_audit_share_flags_with_and_without_explicit_path() {
        let cli1 = Cli::try_parse_from(["agy-hyper", "audit", "--share", "--theme", "light", "--json"]).unwrap();
        match cli1.command {
            Commands::Audit { share, theme, json, .. } => {
                assert_eq!(share, Some(PathBuf::from("agy-hyper-share-card.png")));
                assert_eq!(theme, "light");
                assert!(json);
            }
            _ => panic!("expected Audit command"),
        }

        let cli2 = Cli::try_parse_from([
            "agy-hyper",
            "audit",
            "--share",
            "custom-badge.svg",
            "--share-svg",
            "badge.svg",
            "--share-png",
            "badge.png",
        ])
        .unwrap();
        match cli2.command {
            Commands::Audit {
                share,
                share_svg,
                share_png,
                ..
            } => {
                assert_eq!(share, Some(PathBuf::from("custom-badge.svg")));
                assert_eq!(share_svg, Some(PathBuf::from("badge.svg")));
                assert_eq!(share_png, Some(PathBuf::from("badge.png")));
            }
            _ => panic!("expected Audit command"),
        }
    }

    #[test]
    fn parses_update_and_doctor_cli_flags() {
        let doc = Cli::try_parse_from(["agy-hyper", "doctor", "--net", "--json"]).unwrap();
        match doc.command {
            Commands::Doctor { net, json } => {
                assert!(net);
                assert!(json);
            }
            _ => panic!("expected Doctor command"),
        }

        let upd = Cli::try_parse_from([
            "agy-hyper",
            "update",
            "--check",
            "--dismiss",
            "0.5.1",
            "--version",
            "v0.5.1",
            "--json",
        ])
        .unwrap();
        match upd.command {
            Commands::Update {
                check,
                check_quiet,
                dismiss,
                version,
                json,
            } => {
                assert!(check);
                assert!(!check_quiet);
                assert_eq!(dismiss.as_deref(), Some("0.5.1"));
                assert_eq!(version.as_deref(), Some("v0.5.1"));
                assert!(json);
            }
            _ => panic!("expected Update command"),
        }

        let quiet = Cli::try_parse_from(["agy-hyper", "update", "--check-quiet"]).unwrap();
        match quiet.command {
            Commands::Update { check_quiet, .. } => assert!(check_quiet),
            _ => panic!("expected Update command"),
        }
    }

    #[test]
    fn doctor_conflict_audit_context_baseline_and_dual_stack_probe_work() {
        let tmp = std::env::temp_dir().join(format!(
            "agy-hyper-doctor-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);

        let plugins = tmp.join("plugins");
        fs::create_dir_all(plugins.join("agy-hyper/rules")).unwrap();
        fs::write(
            plugins.join("agy-hyper/hooks.json"),
            r#"{"agy-hyper":{"enabled":true,"PreInvocation":[{"type":"command"}]}}"#,
        )
        .unwrap();

        // Competing enabled plugin with PreInvocation + anti-MCP rule
        fs::create_dir_all(plugins.join("other-guard/rules")).unwrap();
        fs::write(
            plugins.join("other-guard/hooks.json"),
            r#"{"guard":{"enabled":true,"PreInvocation":[{"type":"command"}],"PostToolUse":[{"hooks":[1]}]}}"#,
        )
        .unwrap();
        fs::write(
            plugins.join("other-guard/rules/AGENTS.md"),
            "# Policy\nAlways only use `read_file` / `edit_file` for code changes.\n",
        )
        .unwrap();

        // Disabled plugin must not be flagged for hooks
        fs::create_dir_all(plugins.join("disabled-plugin")).unwrap();
        fs::write(
            plugins.join("disabled-plugin/hooks.json"),
            r#"{"enabled":false,"PreInvocation":[{"type":"command"}]}"#,
        )
        .unwrap();

        let (scanned, conflicts_accel) = audit_plugin_conflicts(&plugins, true);
        assert_eq!(scanned, 2);
        assert_eq!(conflicts_accel.len(), 2);
        assert!(conflicts_accel
            .iter()
            .any(|c| c.contains("other-guard/hooks.json") && c.contains("PreInvocation+PostToolUse")));
        assert!(conflicts_accel
            .iter()
            .any(|c| c.contains("other-guard/rules/AGENTS.md") && c.contains("anti-MCP directive")));

        // In Observe mode (mcp_accel_enabled = false), anti-MCP rules are not flagged
        let (_, conflicts_observe) = audit_plugin_conflicts(&plugins, false);
        assert_eq!(conflicts_observe.len(), 1);

        // Context baseline load + duplicate skill folders + MCP count
        let agents_skills = tmp.join("agents-skills");
        let gemini_skills = tmp.join("gemini-skills");
        for idx in 0..65 {
            let s1 = agents_skills.join(format!("skill-{}", idx));
            fs::create_dir_all(&s1).unwrap();
            fs::write(s1.join("SKILL.md"), "---\nname: test\n---\n").unwrap();

            let s2 = gemini_skills.join(format!("skill-{}", idx));
            fs::create_dir_all(&s2).unwrap();
            fs::write(s2.join("SKILL.md"), "---\nname: test\n---\n").unwrap();
        }
        let mcp_cfg = tmp.join("mcp_config.json");
        fs::write(
            &mcp_cfg,
            r#"{"mcpServers":{"agy-hyper_core":{},"docs_srv":{}}}"#,
        )
        .unwrap();
        let diag = inspect_context_baseline(&agents_skills, &gemini_skills, &mcp_cfg);
        assert_eq!(diag.total_skill_files, 130);
        assert_eq!(diag.duplicate_skill_folders.len(), 65);
        assert_eq!(diag.mcp_server_count, 2);

        // Dual-stack blackhole classifier
        let fast_v4 = NetProbeSample {
            ok: true,
            http_code: 204,
            tls_ms: 45,
            ttfb_ms: 68,
            total_ms: 70,
            error: None,
        };
        let hung_auto = NetProbeSample {
            ok: false,
            http_code: 0,
            tls_ms: 0,
            ttfb_ms: 5002,
            total_ms: 5002,
            error: Some("Connection timed out".into()),
        };
        let (st_bad, msg_bad) = evaluate_dual_stack_probe(&fast_v4, &hung_auto);
        assert_eq!(st_bad, "warn");
        assert!(msg_bad.contains("IPv6 blackhole"));

        let healthy_auto = NetProbeSample {
            ok: true,
            http_code: 204,
            tls_ms: 52,
            ttfb_ms: 75,
            total_ms: 78,
            error: None,
        };
        let (st_ok, _) = evaluate_dual_stack_probe(&fast_v4, &healthy_auto);
        assert_eq!(st_ok, "ok");

        let _ = fs::remove_dir_all(&tmp);
    }
}
