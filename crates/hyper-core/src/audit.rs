use crate::bench::{analyze_transcript, ContextCategoryShare, SessionBenchmark};
use crate::config::HyperConfig;
use crate::sessions::discover_transcripts;
use crate::usage::{
    default_brain_dirs, now_epoch_ms, ModelTtlEstimate, UsageLedger, UsageWindow, UsageWindowReport,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditCohortStats {
    pub sessions: usize,
    pub coding_sessions: usize,
    pub total_turns: usize,
    pub turn_p50_ms: u64,
    pub turn_p90_ms: u64,
    pub tool_p50_ms: u64,
    pub cache_hit_ratio: f64,
    pub first_edit_p50_tokens: Option<u64>,
    pub blanket_read_turns: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AdoptionComparison {
    pub native_only: AuditCohortStats,
    pub with_hyper: AuditCohortStats,
    pub hyper_turn_count: usize,
    pub hyper_turn_tool_p50_ms: u64,
    pub native_turn_count: usize,
    pub native_turn_tool_p50_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditFinding {
    pub severity: String, // "high" | "warn" | "ok"
    pub code: String,
    pub title: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BurnAuditReport {
    pub version: String,
    pub window_days: u32,
    pub usage_window: String,
    pub sessions_analyzed: usize,
    pub coding_sessions: usize,
    pub total_turns: usize,
    pub currency: String,
    pub fx_rate: f64,
    pub total_tokens: u64,
    pub overall_cache_hit_ratio: f64,
    pub total_api_equiv_usd: f64,
    pub total_api_equiv: f64,
    pub cache_saved_usd: f64,
    pub cache_saved: f64,
    pub uncached_input_usd: f64,
    pub uncached_input_cost: f64,
    pub cache_break_turns: usize,
    pub first_edit_p50_tokens: Option<u64>,
    pub first_edit_p90_tokens: Option<u64>,
    pub first_edit_max_tokens: Option<u64>,
    pub blanket_read_turns: usize,
    pub sessions_with_blanket_reads: usize,
    pub context_categories: Vec<ContextCategoryShare>,
    pub model_ttl_cliffs: Vec<ModelTtlEstimate>,
    pub comparison: AdoptionComparison,
    pub findings: Vec<AuditFinding>,
}

fn percentile_u64(values: &mut [u64], pct: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let idx = (((values.len() - 1) as f64) * pct).round() as usize;
    values[idx.min(values.len() - 1)]
}

fn usage_window_for_days(days: u32) -> UsageWindow {
    if days <= 1 {
        UsageWindow::Hours24
    } else if days <= 7 {
        UsageWindow::Days7
    } else if days <= 30 {
        UsageWindow::Days30
    } else {
        UsageWindow::Days90
    }
}

fn compute_cohort(group: &[&SessionBenchmark]) -> AuditCohortStats {
    if group.is_empty() {
        return AuditCohortStats::default();
    }
    let mut turn_ms = Vec::new();
    let mut tool_ms = Vec::new();
    let mut first_edits = Vec::new();
    let mut total_in = 0u64;
    let mut total_cache = 0u64;
    let mut blanket = 0usize;

    for s in group {
        for t in &s.turns {
            turn_ms.push(t.total_turn_ms);
            if t.tool_exec_ms > 0 {
                tool_ms.push(t.tool_exec_ms);
            }
        }
        total_in = total_in.saturating_add(s.total_input_tokens);
        total_cache = total_cache.saturating_add(s.total_cache_read_tokens);
        if let Some(fe) = s.first_edit_context_tokens {
            first_edits.push(fe);
        }
        blanket += s.blanket_read_turns;
    }
    let denom = total_in + total_cache;
    AuditCohortStats {
        sessions: group.len(),
        coding_sessions: first_edits.len(),
        total_turns: turn_ms.len(),
        turn_p50_ms: percentile_u64(&mut turn_ms, 0.50),
        turn_p90_ms: percentile_u64(&mut turn_ms, 0.90),
        tool_p50_ms: percentile_u64(&mut tool_ms, 0.50),
        cache_hit_ratio: if denom > 0 {
            total_cache as f64 / denom as f64
        } else {
            0.0
        },
        first_edit_p50_tokens: if first_edits.is_empty() {
            None
        } else {
            Some(percentile_u64(&mut first_edits, 0.50))
        },
        blanket_read_turns: blanket,
    }
}

pub fn build_audit_report(
    days: u32,
    max_sessions: usize,
    cfg: &HyperConfig,
    tz_offset_mins: i32,
) -> BurnAuditReport {
    let clamped_days = days.clamp(1, 90);
    let cap = max_sessions.clamp(5, 200);
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(u64::from(clamped_days) * 86_400))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let paths: Vec<PathBuf> = discover_transcripts(cap)
        .into_iter()
        .filter(|p| {
            fs::metadata(p)
                .and_then(|m| m.modified())
                .map(|mt| mt >= cutoff)
                .unwrap_or(true)
        })
        .collect();

    let sessions: Vec<SessionBenchmark> = paths
        .iter()
        .filter_map(|p| analyze_transcript(p).ok())
        .filter(|s| s.valid_turns > 0)
        .collect();

    let ledger_path = HyperConfig::ledger_path();
    let mut ledger = UsageLedger::load_from(&ledger_path);
    let now_ms = now_epoch_ms();
    let outcome = ledger.sync_from_dirs(&default_brain_dirs(), now_ms);
    if outcome.dirty {
        let _ = ledger.save_to(&ledger_path);
    }
    let uwin = usage_window_for_days(clamped_days);
    let usage_report = ledger.query_window(uwin, cfg, None, tz_offset_mins, now_ms, outcome.scan_ms);
    let ttl_cliffs = ledger.all_model_ttl_estimates(cfg.cache_warm_window_secs);

    build_audit_report_from_parts(clamped_days, &sessions, &usage_report, ttl_cliffs)
}

pub fn build_audit_report_from_parts(
    window_days: u32,
    sessions: &[SessionBenchmark],
    usage: &UsageWindowReport,
    mut ttl_cliffs: Vec<ModelTtlEstimate>,
) -> BurnAuditReport {
    let mut first_edits: Vec<u64> = Vec::new();
    let mut blanket_read_turns = 0usize;
    let mut sessions_with_blanket_reads = 0usize;
    let mut cache_break_turns = 0usize;
    let mut total_turns = 0usize;

    let mut file_reads = 0u64;
    let mut cmd_outputs = 0u64;
    let mut hyper_tools = 0u64;
    let mut other_tools = 0u64;
    let mut thinking_text = 0u64;
    let mut user_sys = 0u64;

    let mut native_refs: Vec<&SessionBenchmark> = Vec::new();
    let mut hyper_refs: Vec<&SessionBenchmark> = Vec::new();
    let mut hyper_tool_ms: Vec<u64> = Vec::new();
    let mut native_tool_ms: Vec<u64> = Vec::new();

    for s in sessions {
        total_turns += s.valid_turns;
        cache_break_turns += s.cache_break_turns;
        if let Some(fe) = s.first_edit_context_tokens {
            first_edits.push(fe);
        }
        if s.blanket_read_turns > 0 {
            blanket_read_turns += s.blanket_read_turns;
            sessions_with_blanket_reads += 1;
        }
        file_reads = file_reads.saturating_add(s.context_breakdown.file_read_bytes);
        cmd_outputs = cmd_outputs.saturating_add(s.context_breakdown.command_output_bytes);
        hyper_tools = hyper_tools.saturating_add(s.context_breakdown.hyper_tool_bytes);
        other_tools = other_tools.saturating_add(s.context_breakdown.other_tool_bytes);
        thinking_text = thinking_text
            .saturating_add(s.context_breakdown.planner_thinking_bytes)
            .saturating_add(s.context_breakdown.planner_text_bytes);
        user_sys = user_sys.saturating_add(s.context_breakdown.user_system_bytes);

        if s.hyper_tool_calls > 0 {
            hyper_refs.push(s);
        } else {
            native_refs.push(s);
        }
        for t in &s.turns {
            if t.tool_exec_ms > 0 {
                if t.used_hyper_tools {
                    hyper_tool_ms.push(t.tool_exec_ms);
                } else {
                    native_tool_ms.push(t.tool_exec_ms);
                }
            }
        }
    }

    let total_ctx_bytes = file_reads + cmd_outputs + hyper_tools + other_tools + thinking_text + user_sys;
    let denom = total_ctx_bytes.max(1) as f64;
    let context_categories: Vec<ContextCategoryShare> = [
        ("file_reads", file_reads),
        ("command_outputs", cmd_outputs),
        ("hyper_tools", hyper_tools),
        ("other_tools", other_tools),
        ("thinking_text", thinking_text),
        ("user_system", user_sys),
    ]
    .into_iter()
    .filter(|(_, b)| *b > 0)
    .map(|(k, b)| ContextCategoryShare {
        key: k.to_string(),
        bytes: b,
        share_pct: ((b as f64 / denom) * 1000.0).round() / 10.0,
    })
    .collect();

    let first_edit_p50_tokens = if first_edits.is_empty() {
        None
    } else {
        Some(percentile_u64(&mut first_edits, 0.50))
    };
    let first_edit_p90_tokens = if first_edits.is_empty() {
        None
    } else {
        Some(percentile_u64(&mut first_edits, 0.90))
    };
    let first_edit_max_tokens = first_edits.iter().copied().max();

    let uncached_input_usd: f64 = usage
        .models
        .iter()
        .filter_map(|m| {
            m.rate_applied
                .map(|r| (m.input_tokens as f64 / 1_000_000.0) * r.input_per_m)
        })
        .sum();
    let fx = if usage.fx_rate.is_finite() && usage.fx_rate > 0.0 {
        usage.fx_rate
    } else {
        1.0
    };
    let uncached_input_cost = uncached_input_usd * fx;

    ttl_cliffs.sort_by(|a, b| {
        a.effective_ttl_secs
            .cmp(&b.effective_ttl_secs)
            .then_with(|| b.total_samples.cmp(&a.total_samples))
    });

    let comparison = AdoptionComparison {
        native_only: compute_cohort(&native_refs),
        with_hyper: compute_cohort(&hyper_refs),
        hyper_turn_count: hyper_tool_ms.len(),
        hyper_turn_tool_p50_ms: percentile_u64(&mut hyper_tool_ms, 0.50),
        native_turn_count: native_tool_ms.len(),
        native_turn_tool_p50_ms: percentile_u64(&mut native_tool_ms, 0.50),
    };

    let mut findings: Vec<AuditFinding> = Vec::new();
    if let Some(shortest) = ttl_cliffs
        .iter()
        .find(|m| m.source == "empirical" && m.effective_ttl_secs <= 180)
    {
        findings.push(AuditFinding {
            severity: if shortest.effective_ttl_secs <= 120 {
                "high"
            } else {
                "warn"
            }
            .into(),
            code: "short_cache_ttl".into(),
            title: format!(
                "{} drops cache past {}s (empirical, n={})",
                shortest.model_family, shortest.effective_ttl_secs, shortest.total_samples
            ),
            detail: format!(
                "Avoid background command waits longer than {}s on {}; keep verification synchronous inside the active turn.",
                shortest.effective_ttl_secs.saturating_sub(30).max(30),
                shortest.model_family
            ),
        });
    }

    if let Some(fe_p50) = first_edit_p50_tokens {
        if fe_p50 >= 90_000 || blanket_read_turns >= 10 {
            findings.push(AuditFinding {
                severity: if fe_p50 >= 120_000 { "high" } else { "warn" }.into(),
                code: "pre_edit_bloat".into(),
                title: format!(
                    "First-edit context P50 is {:.0}k ({} blanket-read turns)",
                    fe_p50 as f64 / 1000.0,
                    blanket_read_turns
                ),
                detail: format!(
                    "Across {} coding session(s), agents frequently bulk-read files before making the first edit. Use hyper_pack or ranged reads.",
                    first_edits.len()
                ),
            });
        }
    }

    if uncached_input_usd >= 1.0 && usage.estimated_cost_usd > 0.0 {
        let share = ((uncached_input_usd / usage.estimated_cost_usd) * 100.0).min(100.0);
        findings.push(AuditFinding {
            severity: if share >= 35.0 { "warn" } else { "ok" }.into(),
            code: "uncached_input_tax".into(),
            title: format!(
                "Uncached input accounts for {:.0}% of API-equiv spend (${:.2} USD)",
                share, uncached_input_usd
            ),
            detail: format!(
                "Prompt caching still saved ${:.2} USD ({:.1}% hit rate across {} turns).",
                usage.cache_saved_usd,
                usage.cache_hit_ratio * 100.0,
                usage.total_turns
            ),
        });
    }

    if let Some(top_cat) = context_categories.iter().max_by_key(|c| c.bytes) {
        if top_cat.share_pct >= 35.0 && findings.len() < 4 {
            findings.push(AuditFinding {
                severity: "ok".into(),
                code: "top_context_driver".into(),
                title: format!(
                    "Largest context slice is {} ({:.1}%)",
                    category_label(&top_cat.key),
                    top_cat.share_pct
                ),
                detail: "Inspect Context X-Ray in Hyper Deck to see live post-compact token attribution per session.".into(),
            });
        }
    }

    if findings.is_empty() {
        findings.push(AuditFinding {
            severity: "ok".into(),
            code: "healthy".into(),
            title: "Session telemetry looks balanced".into(),
            detail: "Run more coding sessions to unlock empirical per-model TTL cliffs and pre-edit bloat diagnostics.".into(),
        });
    }

    BurnAuditReport {
        version: crate::version().to_string(),
        window_days,
        usage_window: usage.window.clone(),
        sessions_analyzed: sessions.len(),
        coding_sessions: first_edits.len(),
        total_turns,
        currency: usage.currency.clone(),
        fx_rate: fx,
        total_tokens: usage.total_tokens,
        overall_cache_hit_ratio: usage.cache_hit_ratio,
        total_api_equiv_usd: usage.estimated_cost_usd,
        total_api_equiv: usage.estimated_cost,
        cache_saved_usd: usage.cache_saved_usd,
        cache_saved: usage.cache_saved,
        uncached_input_usd,
        uncached_input_cost,
        cache_break_turns,
        first_edit_p50_tokens,
        first_edit_p90_tokens,
        first_edit_max_tokens,
        blanket_read_turns,
        sessions_with_blanket_reads,
        context_categories,
        model_ttl_cliffs: ttl_cliffs,
        comparison,
        findings,
    }
}

fn category_label(key: &str) -> &'static str {
    match key {
        "file_reads" => "File Reads",
        "command_outputs" => "Command Output",
        "hyper_tools" => "Hyper MCP",
        "other_tools" => "Other Tools",
        "thinking_text" => "Thinking & Text",
        "user_system" => "User & System",
        _ => "Other",
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn fmt_tok_short(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.2}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn fmt_money(amount: f64, ccy: &str) -> String {
    let sym = match ccy {
        "CNY" | "JPY" => "¥",
        "EUR" => "€",
        "GBP" => "£",
        "HKD" => "HK$",
        "SGD" => "S$",
        "KRW" => "₩",
        _ => "$",
    };
    if matches!(ccy, "JPY" | "KRW") {
        format!("{}{:.0}", sym, amount)
    } else {
        format!("{}{:.2}", sym, amount)
    }
}

/// Render a clean, zero-dependency, shareable SVG diagnostic card (920×560).
pub fn render_audit_svg(report: &BurnAuditReport, dark: bool) -> String {
    let (bg, card_bg, border, fg, muted, accent, ok, warn, err, track) = if dark {
        (
            "#0B0F17",
            "#111827",
            "#1E293B",
            "#F8FAFC",
            "#94A3B8",
            "#38BDF8",
            "#34D399",
            "#FBBF24",
            "#FB7185",
            "#1E293B",
        )
    } else {
        (
            "#F8FAFC",
            "#FFFFFF",
            "#E2E8F0",
            "#0F172A",
            "#64748B",
            "#0284C7",
            "#059669",
            "#D97706",
            "#E11D48",
            "#E2E8F0",
        )
    };

    let fe_str = report
        .first_edit_p50_tokens
        .map(|v| format!("{} tok", fmt_tok_short(v)))
        .unwrap_or_else(|| "—".into());
    let fe_sub = report
        .first_edit_p90_tokens
        .map(|v| format!("P90 {} · {} coding sess", fmt_tok_short(v), report.coding_sessions))
        .unwrap_or_else(|| "No coding edits in window".into());

    let cache_pct = format!("{:.1}%", report.overall_cache_hit_ratio * 100.0);
    let uncached_str = fmt_money(report.uncached_input_cost, &report.currency);
    let saved_sub = format!("Cache saved {}", fmt_money(report.cache_saved, &report.currency));
    let blanket_str = format!("{}", report.blanket_read_turns);
    let blanket_sub = format!("in {} session(s)", report.sessions_with_blanket_reads);

    // Left panel: Empirical TTL Cliffs (up to 5 models)
    let mut ttl_rows = String::new();
    let max_ttl = 240.0f64;
    for (idx, m) in report.model_ttl_cliffs.iter().take(5).enumerate() {
        let y = 248 + idx * 34;
        let ratio = (m.effective_ttl_secs as f64 / max_ttl).clamp(0.08, 1.0);
        let fill_w = (ratio * 195.0).round();
        let bar_color = if m.effective_ttl_secs <= 120 {
            err
        } else if m.effective_ttl_secs <= 180 {
            warn
        } else {
            ok
        };
        let tag = if m.source == "empirical" { "emp" } else { "def" };
        let label = xml_escape(&m.model_family);
        ttl_rows.push_str(&format!(
            "  <text x=\"44\" y=\"{}\" fill=\"{}\" font-size=\"11.5\" font-family=\"ui-monospace,Menlo,monospace\">{}</text>\n\
             \x20 <rect x=\"184\" y=\"{}\" width=\"195\" height=\"10\" rx=\"5\" fill=\"{}\"/>\n\
             \x20 <rect x=\"184\" y=\"{}\" width=\"{:.0}\" height=\"10\" rx=\"5\" fill=\"{}\"/>\n\
             \x20 <text x=\"434\" y=\"{}\" text-anchor=\"end\" fill=\"{}\" font-size=\"11\" font-family=\"ui-monospace,Menlo,monospace\">{}s · {}</text>\n",
            y,
            fg,
            label,
            y - 9,
            track,
            y - 9,
            fill_w,
            bar_color,
            y,
            muted,
            m.effective_ttl_secs,
            tag
        ));
    }
    if report.model_ttl_cliffs.is_empty() {
        ttl_rows.push_str(&format!(
            "  <text x=\"44\" y=\"268\" fill=\"{}\" font-size=\"12\" font-family=\"system-ui,sans-serif\">Not enough multi-turn gaps yet for empirical TTL.</text>\n",
            muted
        ));
    }

    // Right panel: Context Share bars + Observed Native vs Hyper split
    let cat_colors = [accent, warn, ok, "#818CF8", "#A78BFA", muted];
    let mut ctx_bar = String::new();
    let mut ctx_legend = String::new();
    let mut cur_x = 486.0f64;
    let total_w = 390.0f64;
    for (idx, c) in report.context_categories.iter().take(5).enumerate() {
        let seg_w = ((c.share_pct / 100.0) * total_w).max(4.0);
        let col = cat_colors[idx % cat_colors.len()];
        ctx_bar.push_str(&format!(
            "  <rect x=\"{:.1}\" y=\"240\" width=\"{:.1}\" height=\"14\" rx=\"3\" fill=\"{}\"/>\n",
            cur_x,
            seg_w.min((486.0 + total_w - cur_x).max(0.0)),
            col
        ));
        cur_x += seg_w + 2.0;

        let lx = 486 + (idx % 2) * 195;
        let ly = 274 + (idx / 2) * 20;
        ctx_legend.push_str(&format!(
            "  <rect x=\"{}\" y=\"{}\" width=\"8\" height=\"8\" rx=\"2\" fill=\"{}\"/>\n\
             \x20 <text x=\"{}\" y=\"{}\" fill=\"{}\" font-size=\"11\" font-family=\"system-ui,sans-serif\">{} {:.1}%</text>\n",
            lx,
            ly - 8,
            col,
            lx + 14,
            ly,
            muted,
            xml_escape(category_label(&c.key)),
            c.share_pct
        ));
    }

    let nat = &report.comparison.native_only;
    let hyp = &report.comparison.with_hyper;
    let cohort_line_1 = format!(
        "Native-only ({} sess · {} turns): P50 {:.1}s · Cache {:.1}%",
        nat.sessions,
        nat.total_turns,
        nat.turn_p50_ms as f64 / 1000.0,
        nat.cache_hit_ratio * 100.0
    );
    let cohort_line_2 = format!(
        "With Hyper  ({} sess · {} turns): P50 {:.1}s · Cache {:.1}%",
        hyp.sessions,
        hyp.total_turns,
        hyp.turn_p50_ms as f64 / 1000.0,
        hyp.cache_hit_ratio * 100.0
    );

    // Bottom findings strip (up to 3 concise lines)
    let mut findings_svg = String::new();
    for (idx, f) in report.findings.iter().take(3).enumerate() {
        let y = 456 + idx * 24;
        let dot = match f.severity.as_str() {
            "high" => err,
            "warn" => warn,
            _ => ok,
        };
        let txt = xml_escape(&f.title);
        findings_svg.push_str(&format!(
            "  <circle cx=\"50\" cy=\"{}\" r=\"4\" fill=\"{}\"/>\n\
             \x20 <text x=\"64\" y=\"{}\" fill=\"{}\" font-size=\"12\" font-family=\"system-ui,-apple-system,sans-serif\">{}</text>\n",
            y - 4,
            dot,
            y,
            fg,
            txt
        ));
    }

    let header_sub = xml_escape(&format!(
        "Window: {}d (ledger {}) · {} sessions · {} turns · {} tokens",
        report.window_days,
        report.usage_window,
        report.sessions_analyzed,
        report.total_turns,
        fmt_tok_short(report.total_tokens)
    ));

    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"920\" height=\"560\" viewBox=\"0 0 920 560\">\n\
         \x20 <rect width=\"920\" height=\"560\" rx=\"16\" fill=\"{bg}\" stroke=\"{border}\" stroke-width=\"1.5\"/>\n\
         \x20 <text x=\"28\" y=\"42\" fill=\"{fg}\" font-size=\"18\" font-weight=\"700\" font-family=\"system-ui,-apple-system,sans-serif\">AGY HYPER · LOCAL TELEMETRY BURN AUDIT</text>\n\
         \x20 <text x=\"892\" y=\"40\" text-anchor=\"end\" fill=\"{accent}\" font-size=\"12\" font-weight=\"600\" font-family=\"ui-monospace,Menlo,monospace\">v{ver}</text>\n\
         \x20 <text x=\"28\" y=\"64\" fill=\"{muted}\" font-size=\"12\" font-family=\"ui-monospace,Menlo,monospace\">{header_sub}</text>\n\
         \x20 <g>\n\
         \x20   <rect x=\"28\" y=\"84\" width=\"206\" height=\"88\" rx=\"10\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20   <text x=\"44\" y=\"108\" fill=\"{muted}\" font-size=\"11\" font-family=\"system-ui,sans-serif\">PROMPT CACHE HIT</text>\n\
         \x20   <text x=\"44\" y=\"138\" fill=\"{accent}\" font-size=\"24\" font-weight=\"700\" font-family=\"ui-monospace,Menlo,monospace\">{cache_pct}</text>\n\
         \x20   <text x=\"44\" y=\"158\" fill=\"{muted}\" font-size=\"11\" font-family=\"system-ui,sans-serif\">{breaks} cold-cache breaks</text>\n\
         \x20   <rect x=\"248\" y=\"84\" width=\"206\" height=\"88\" rx=\"10\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20   <text x=\"264\" y=\"108\" fill=\"{muted}\" font-size=\"11\" font-family=\"system-ui,sans-serif\">UNCACHED INPUT SPEND</text>\n\
         \x20   <text x=\"264\" y=\"138\" fill=\"{warn}\" font-size=\"24\" font-weight=\"700\" font-family=\"ui-monospace,Menlo,monospace\">{uncached_str}</text>\n\
         \x20   <text x=\"264\" y=\"158\" fill=\"{ok}\" font-size=\"11\" font-family=\"system-ui,sans-serif\">{saved_sub}</text>\n\
         \x20   <rect x=\"468\" y=\"84\" width=\"206\" height=\"88\" rx=\"10\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20   <text x=\"484\" y=\"108\" fill=\"{muted}\" font-size=\"11\" font-family=\"system-ui,sans-serif\">FIRST-EDIT CONTEXT P50</text>\n\
         \x20   <text x=\"484\" y=\"138\" fill=\"{fg}\" font-size=\"24\" font-weight=\"700\" font-family=\"ui-monospace,Menlo,monospace\">{fe_str}</text>\n\
         \x20   <text x=\"484\" y=\"158\" fill=\"{muted}\" font-size=\"11\" font-family=\"system-ui,sans-serif\">{fe_sub}</text>\n\
         \x20   <rect x=\"688\" y=\"84\" width=\"204\" height=\"88\" rx=\"10\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20   <text x=\"704\" y=\"108\" fill=\"{muted}\" font-size=\"11\" font-family=\"system-ui,sans-serif\">BLANKET-READ TURNS</text>\n\
         \x20   <text x=\"704\" y=\"138\" fill=\"{err}\" font-size=\"24\" font-weight=\"700\" font-family=\"ui-monospace,Menlo,monospace\">{blanket_str}</text>\n\
         \x20   <text x=\"704\" y=\"158\" fill=\"{muted}\" font-size=\"11\" font-family=\"system-ui,sans-serif\">{blanket_sub}</text>\n\
         \x20 </g>\n\
         \x20 <rect x=\"28\" y=\"190\" width=\"426\" height=\"214\" rx=\"10\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20 <text x=\"44\" y=\"216\" fill=\"{fg}\" font-size=\"13\" font-weight=\"700\" font-family=\"system-ui,sans-serif\">EMPIRICAL MODEL CACHE TTL CLIFFS</text>\n\
         {ttl_rows}\
         \x20 <rect x=\"468\" y=\"190\" width=\"424\" height=\"214\" rx=\"10\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20 <text x=\"486\" y=\"216\" fill=\"{fg}\" font-size=\"13\" font-weight=\"700\" font-family=\"system-ui,sans-serif\">POST-COMPACT CONTEXT MIX &amp; OBSERVED COHORTS</text>\n\
         {ctx_bar}\
         {ctx_legend}\
         \x20 <line x1=\"486\" y1=\"342\" x2=\"874\" y2=\"342\" stroke=\"{border}\" stroke-width=\"1\"/>\n\
         \x20 <text x=\"486\" y=\"365\" fill=\"{muted}\" font-size=\"11\" font-family=\"ui-monospace,Menlo,monospace\">{c1}</text>\n\
         \x20 <text x=\"486\" y=\"386\" fill=\"{accent}\" font-size=\"11\" font-family=\"ui-monospace,Menlo,monospace\">{c2}</text>\n\
         \x20 <rect x=\"28\" y=\"420\" width=\"864\" height=\"96\" rx=\"10\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20 <text x=\"44\" y=\"438\" fill=\"{muted}\" font-size=\"10.5\" font-weight=\"700\" font-family=\"system-ui,sans-serif\">TOP LOCAL FINDINGS</text>\n\
         {findings_svg}\
         \x20 <text x=\"28\" y=\"542\" fill=\"{muted}\" font-size=\"11\" font-family=\"ui-monospace,Menlo,monospace\">100% local transcript audit · zero cloud upload · github.com/BobbyCats/agy-hyper</text>\n\
         </svg>\n",
        ver = xml_escape(&report.version),
        breaks = report.cache_break_turns,
        uncached_str = xml_escape(&uncached_str),
        saved_sub = xml_escape(&saved_sub),
        fe_str = xml_escape(&fe_str),
        fe_sub = xml_escape(&fe_sub),
        blanket_str = xml_escape(&blanket_str),
        blanket_sub = xml_escape(&blanket_sub),
        c1 = xml_escape(&cohort_line_1),
        c2 = xml_escape(&cohort_line_2),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::{ContextBreakdown, TurnWaterfall};
    use crate::config::ModelPriceRate;
    use crate::usage::{ModelUsageBreakdown, TtlBucketSummary};
    use std::collections::BTreeMap;

    fn sample_session(id: &str, hyper_calls: usize, first_edit: Option<u64>, blanket: usize) -> SessionBenchmark {
        SessionBenchmark {
            conversation_id: id.into(),
            transcript_path: "/tmp/t.jsonl".into(),
            total_turns: 2,
            valid_turns: 2,
            dropped_idle_gap_turns: 0,
            total_wall_ms: 20_000,
            total_llm_ms: 14_000,
            total_tool_ms: 6_000,
            median_turn_ms: 10_000,
            p90_turn_ms: 12_000,
            median_llm_ms: 7_000,
            effective_tps: 80.0,
            total_input_tokens: 20_000,
            total_cache_read_tokens: 180_000,
            total_output_tokens: 1_600,
            cache_hit_ratio: 0.9,
            cache_break_turns: 1,
            hyper_tool_calls: hyper_calls,
            total_tool_calls: 6,
            hyper_adoption_ratio: if hyper_calls > 0 { 0.5 } else { 0.0 },
            model_usage: BTreeMap::new(),
            context_breakdown: ContextBreakdown {
                file_read_bytes: 60_000,
                command_output_bytes: 30_000,
                hyper_tool_bytes: 10_000,
                total_bytes: 100_000,
                ..Default::default()
            },
            first_edit_context_tokens: first_edit,
            max_parallel_reads: if blanket > 0 { 5 } else { 2 },
            blanket_read_turns: blanket,
            turns: vec![TurnWaterfall {
                turn_index: 1,
                step_index: 4,
                started_ms: 1_700_000_000_000,
                total_turn_ms: 10_000,
                llm_inference_ms: 7_000,
                tool_exec_ms: 3_000,
                input_tokens: 10_000,
                cache_read_tokens: 90_000,
                output_tokens: 800,
                used_hyper_tools: hyper_calls > 0,
                tool_calls: 3,
                tool_names: vec![],
            }],
        }
    }

    #[test]
    fn audit_computes_honest_metrics_findings_and_valid_svg() {
        let sessions = vec![
            sample_session("s1", 0, Some(150_000), 4),
            sample_session("s2", 3, Some(80_000), 1),
        ];
        let usage = UsageWindowReport {
            window: "14d".into(),
            currency: "USD".into(),
            fx_rate: 1.0,
            fx_source: "offline".into(),
            fx_updated_at_ms: None,
            available_currencies: BTreeMap::from([("USD".into(), 1.0)]),
            total_turns: 4,
            active_sessions: 2,
            total_input_tokens: 1_000_000,
            total_cache_read_tokens: 9_000_000,
            total_output_tokens: 100_000,
            total_tokens: 10_100_000,
            cache_hit_ratio: 0.9,
            attributed_tokens: 10_100_000,
            unattributed_tokens: 0,
            attributed_ratio: 1.0,
            estimated_cost_usd: 8.5,
            cache_saved_usd: 22.0,
            estimated_cost: 8.5,
            cache_saved: 22.0,
            ledger_sessions_tracked: 2,
            ledger_scan_ms: 2,
            series: vec![],
            models: vec![ModelUsageBreakdown {
                model: "gemini-4-neon".into(),
                attributed: true,
                priced: true,
                turns: 4,
                input_tokens: 1_000_000,
                cache_read_tokens: 9_000_000,
                output_tokens: 100_000,
                total_tokens: 10_100_000,
                cache_hit_ratio: 0.9,
                estimated_cost_usd: 8.5,
                estimated_cost: 8.5,
                cache_saved_usd: 22.0,
                cache_saved: 22.0,
                rate_applied: Some(ModelPriceRate {
                    input_per_m: 3.0,
                    cache_read_per_m: 0.3,
                    output_per_m: 15.0,
                }),
            }],
        };
        let cliffs = vec![ModelTtlEstimate {
            model_family: "gemini-4-carbon".into(),
            effective_ttl_secs: 90,
            source: "empirical".into(),
            total_samples: 120,
            buckets: vec![TtlBucketSummary {
                label: "90-120s".into(),
                upper_secs: 120,
                samples: 30,
                cold_samples: 18,
                avg_hit_ratio: 0.35,
                cold_rate: 0.6,
            }],
        }];

        let report = build_audit_report_from_parts(14, &sessions, &usage, cliffs);
        assert_eq!(report.sessions_analyzed, 2);
        assert_eq!(report.blanket_read_turns, 5);
        assert!((report.uncached_input_usd - 3.0).abs() < 1e-6);
        assert!(report.findings.iter().any(|f| f.code == "short_cache_ttl"));
        assert!(report.findings.iter().any(|f| f.code == "pre_edit_bloat"));
        assert_eq!(report.comparison.native_only.sessions, 1);
        assert_eq!(report.comparison.with_hyper.sessions, 1);

        let svg = render_audit_svg(&report, true);
        assert!(svg.starts_with("<svg "));
        assert!(svg.contains("gemini-4-carbon"));
        assert!(svg.contains("90s · emp"));
        assert!(svg.ends_with("</svg>\n"));
    }
}
