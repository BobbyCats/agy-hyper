use crate::audit::{
    category_label, fmt_money, fmt_tok_short, render_svg_to_png_sized, xml_escape,
    BurnAuditReport, MONO_FONT_STACK, SANS_FONT_STACK,
};
use crate::usage::{model_ttl_family, UNATTRIBUTED_MODEL};
use anyhow::{anyhow, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

pub const SHARE_SVG_W: u32 = 920;
pub const SHARE_SVG_H: u32 = 500;
pub const SHARE_PNG_PAD: u32 = 20;

const SHARE_BAR_X: f64 = 166.0;
const SHARE_BAR_W: f64 = 198.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnonymizedShareCard {
    pub version: String,
    pub window_days: u32,
    pub sessions_analyzed: usize,
    pub coding_sessions: usize,
    pub total_turns: usize,
    pub total_tokens: u64,
    pub verdict: String,
    pub cache_hit_pct: f64,
    pub cache_break_turns: usize,
    pub currency: String,
    pub uncached_input_cost: f64,
    pub cache_saved: f64,
    pub first_edit_p50_tokens: Option<u64>,
    pub first_edit_p90_tokens: Option<u64>,
    pub blanket_read_turns: usize,
    pub sessions_with_blanket_reads: usize,
    pub split_cache_drop_pct: f64,
    pub split_total_transitions: usize,
    pub model_switch_count: usize,
    pub executor_cold_start_turns: usize,
    pub split_uncached_penalty_tokens: u64,
    pub orchestrator_hit_pct: Option<f64>,
    pub executor_spawn_hit_pct: Option<f64>,
    pub model_switch_post_hit_pct: Option<f64>,
    pub top_drain_label: String,
    pub top_drain_share_pct: f64,
    pub top_context_label: String,
    pub top_context_share_pct: f64,
    pub shortest_ttl_secs: Option<u64>,
    pub shortest_ttl_model: Option<String>,
    pub takeaway_primary: String,
    pub takeaway_secondary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditShareCardPayload {
    pub svg: String,
    pub png_base64: Option<String>,
    pub svg_width: u32,
    pub svg_height: u32,
    pub png_width: u32,
    pub png_height: u32,
    pub theme: String,
    pub verdict: String,
    pub redacted: bool,
}

fn uuid_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
            .expect("valid uuid regex")
    })
}

fn sensitive_path_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(?:file://[^\s"'<>]+|/(?:Users|home|var|tmp|private|opt|etc|Volumes)/[^\s"'<>]+|[A-Za-z]:\\[^\s"'<>]+|\\\\[^\s"'<>]+)"#,
        )
        .expect("valid path regex")
    })
}

/// Strictly whitelist-sanitize a model family identifier so no filesystem path, username,
/// repo name, or UUID can ever slip into anonymized share artifacts.
pub fn sanitize_model_family_label(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case(UNATTRIBUTED_MODEL)
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains(':')
        || trimmed.chars().any(char::is_whitespace)
        || uuid_regex().is_match(trimmed)
    {
        return "other-model".to_string();
    }
    let fam = model_ttl_family(Some(trimmed));
    if fam.is_empty()
        || fam == UNATTRIBUTED_MODEL
        || fam.contains('/')
        || fam.contains('\\')
        || fam.contains(':')
        || uuid_regex().is_match(&fam)
    {
        return "other-model".to_string();
    }
    let cleaned: String = fam
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        .take(24)
        .collect();
    if cleaned.is_empty() {
        "other-model".to_string()
    } else {
        cleaned
    }
}

/// Defense-in-depth redaction filter stripping Unix/Windows paths, file:// URIs, and UUIDs.
pub fn scrub_sensitive_text(raw: &str) -> String {
    let no_paths = sensitive_path_regex().replace_all(raw, "[redacted]");
    let no_uuids = uuid_regex().replace_all(&no_paths, "[id]");
    no_uuids.into_owned()
}

/// Hard privacy gate verified on every rendered shareable SVG/PNG card.
pub fn assert_share_card_clean(svg_xml: &str) -> Result<()> {
    if sensitive_path_regex().is_match(svg_xml) {
        return Err(anyhow!(
            "privacy guard blocked share card export: detected filesystem path pattern"
        ));
    }
    if uuid_regex().is_match(svg_xml) {
        return Err(anyhow!(
            "privacy guard blocked share card export: detected conversation UUID pattern"
        ));
    }
    Ok(())
}

fn sanitize_currency_code(raw: &str) -> String {
    let up = raw.trim().to_ascii_uppercase();
    if up.len() == 3 && up.chars().all(|c| c.is_ascii_alphabetic()) {
        up
    } else {
        "USD".to_string()
    }
}

fn sanitize_version(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        .take(16)
        .collect();
    if cleaned.is_empty() {
        crate::version().to_string()
    } else {
        cleaned
    }
}

fn plural<'a>(n: usize, singular: &'a str, many: &'a str) -> &'a str {
    if n == 1 {
        singular
    } else {
        many
    }
}

fn truncate_ellipsis(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

/// Project a `BurnAuditReport` into a strict whitelist-only anonymized structure.
/// Never reads raw transcripts, paths, usernames, workspace names, UUIDs, or prompt snippets.
pub fn build_anonymized_share_card(report: &BurnAuditReport) -> AnonymizedShareCard {
    let verdict = match report.verdict.as_str() {
        "BLEEDING" | "WATCH" | "HEALTHY" => report.verdict.clone(),
        _ => "HEALTHY".to_string(),
    };
    let currency = sanitize_currency_code(&report.currency);
    let cache_hit_pct = ((report.overall_cache_hit_ratio.clamp(0.0, 1.0)) * 1000.0).round() / 10.0;

    let sc = &report.split_cache;
    let orchestrator_hit_pct = (sc.orchestrator_turns > 0)
        .then(|| ((sc.orchestrator_cache_hit_ratio.clamp(0.0, 1.0)) * 1000.0).round() / 10.0);
    let executor_spawn_hit_pct = (sc.executor_cold_start_turns > 0)
        .then(|| ((sc.executor_cold_start_hit_ratio.clamp(0.0, 1.0)) * 1000.0).round() / 10.0);
    let model_switch_post_hit_pct = (sc.model_switch_count > 0)
        .then(|| ((sc.model_switch_post_hit_ratio.clamp(0.0, 1.0)) * 1000.0).round() / 10.0);

    let top_context_label = if report.top_quota_drain.context_category == "none"
        || report.top_quota_drain.context_category.is_empty()
    {
        "Balanced".to_string()
    } else {
        category_label(&report.top_quota_drain.context_category).to_string()
    };
    let top_context_share_pct = report.top_quota_drain.context_share_pct.clamp(0.0, 100.0);

    let (top_drain_label, top_drain_share_pct) =
        if !report.top_quota_drain.model_family.is_empty()
            && report.top_quota_drain.model_family != "balanced"
            && report.top_quota_drain.share_pct > 0.0
        {
            (
                sanitize_model_family_label(&report.top_quota_drain.model_family),
                report.top_quota_drain.share_pct.clamp(0.0, 100.0),
            )
        } else if top_context_share_pct > 0.0 {
            (top_context_label.clone(), top_context_share_pct)
        } else {
            ("Balanced".to_string(), 0.0)
        };

    let shortest_emp = report
        .model_ttl_cliffs
        .iter()
        .find(|m| m.source == "empirical")
        .or_else(|| report.model_ttl_cliffs.first());
    let shortest_ttl_secs = shortest_emp.map(|m| m.effective_ttl_secs);
    let shortest_ttl_model = shortest_emp.map(|m| sanitize_model_family_label(&m.model_family));

    // Synthesize strictly numeric + static-template takeaways (zero raw user strings).
    let takeaway_primary = if sc.total_transitions > 0 && sc.headline_cache_drop_pct >= 25.0 {
        format!(
            "Split/switch drop -{:.1}pp ({} {}, {} cold).",
            sc.headline_cache_drop_pct,
            sc.total_transitions,
            plural(sc.total_transitions, "split", "splits"),
            fmt_tok_short(sc.uncached_penalty_tokens)
        )
    } else if let Some(fe) = report.first_edit_p50_tokens.filter(|&v| v >= 90_000) {
        format!(
            "First-edit blind read P50: {} tok ({} blanket {}).",
            fmt_tok_short(fe),
            report.blanket_read_turns,
            plural(report.blanket_read_turns, "turn", "turns")
        )
    } else if let Some(ttl) = shortest_ttl_secs.filter(|&s| s <= 180) {
        format!(
            "Empirical prompt-cache TTL cliff observed at {}s.",
            ttl
        )
    } else {
        format!(
            "Cache hit {:.1}% over {} {} ({} cold {}).",
            cache_hit_pct,
            report.total_turns,
            plural(report.total_turns, "turn", "turns"),
            report.cache_break_turns,
            plural(report.cache_break_turns, "break", "breaks")
        )
    };

    let takeaway_secondary = match verdict.as_str() {
        "BLEEDING" => {
            if let Some(ttl) = shortest_ttl_secs.filter(|&s| s <= 180) {
                format!(
                    "Keep turns <={}s; bound reads and avoid mid-session swaps.",
                    ttl.saturating_sub(30).max(30)
                )
            } else {
                "Avoid mid-session model swaps; bound pre-edit file reads.".to_string()
            }
        }
        "WATCH" => {
            if sc.total_transitions > 0 {
                "Batch subagent handoffs and keep turns within cache TTL.".to_string()
            } else {
                "Prefer ranged reads or hyper_pack before the first edit.".to_string()
            }
        }
        _ => "Prompt cache reuse and pre-edit hygiene look healthy.".to_string(),
    };

    AnonymizedShareCard {
        version: sanitize_version(&report.version),
        window_days: report.window_days.clamp(1, 90),
        sessions_analyzed: report.sessions_analyzed,
        coding_sessions: report.coding_sessions,
        total_turns: report.total_turns,
        total_tokens: report.total_tokens,
        verdict,
        cache_hit_pct,
        cache_break_turns: report.cache_break_turns,
        currency,
        uncached_input_cost: report.uncached_input_cost.max(0.0),
        cache_saved: report.cache_saved.max(0.0),
        first_edit_p50_tokens: report.first_edit_p50_tokens,
        first_edit_p90_tokens: report.first_edit_p90_tokens,
        blanket_read_turns: report.blanket_read_turns,
        sessions_with_blanket_reads: report.sessions_with_blanket_reads,
        split_cache_drop_pct: sc.headline_cache_drop_pct.clamp(0.0, 100.0),
        split_total_transitions: sc.total_transitions,
        model_switch_count: sc.model_switch_count,
        executor_cold_start_turns: sc.executor_cold_start_turns,
        split_uncached_penalty_tokens: sc.uncached_penalty_tokens,
        orchestrator_hit_pct,
        executor_spawn_hit_pct,
        model_switch_post_hit_pct,
        top_drain_label,
        top_drain_share_pct,
        top_context_label,
        top_context_share_pct,
        shortest_ttl_secs,
        shortest_ttl_model,
        takeaway_primary: scrub_sensitive_text(&takeaway_primary),
        takeaway_secondary: scrub_sensitive_text(&takeaway_secondary),
    }
}

/// Render a high-contrast, strictly anonymized Health Badge SVG card (920×500).
pub fn render_share_card_svg(report: &BurnAuditReport, dark: bool) -> String {
    let card = build_anonymized_share_card(report);
    let (
        bg,
        card_bg,
        border,
        fg,
        muted,
        accent,
        ok,
        warn,
        err,
        track,
        ok_bg,
        warn_bg,
        err_bg,
        logo_bg,
    ) = if dark {
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
            "#064E3B",
            "#451A03",
            "#4C0519",
            "#0F172A",
        )
    } else {
        (
            "#F8FAFC",
            "#FFFFFF",
            "#CBD5E1",
            "#0F172A",
            "#475569",
            "#0284C7",
            "#059669",
            "#B45309",
            "#BE123C",
            "#E2E8F0",
            "#ECFDF5",
            "#FFFBEB",
            "#FFF1F2",
            "#0F172A",
        )
    };

    let (stamp_fg, stamp_bg, stamp_stroke) = match card.verdict.as_str() {
        "BLEEDING" => (err, err_bg, err),
        "WATCH" => (warn, warn_bg, warn),
        _ => (ok, ok_bg, ok),
    };

    let cache_color = if card.total_turns == 0 {
        muted
    } else if card.cache_hit_pct >= 82.0 {
        ok
    } else if card.cache_hit_pct >= 65.0 {
        warn
    } else {
        err
    };
    let cache_hero = if card.total_turns > 0 || card.total_tokens > 0 {
        format!("{:.1}%", card.cache_hit_pct)
    } else {
        "—".to_string()
    };
    let cache_sub = truncate_ellipsis(
        &format!(
            "{} cold {} · saved {} · uncached {}",
            card.cache_break_turns,
            plural(card.cache_break_turns, "break", "breaks"),
            fmt_money(card.cache_saved, &card.currency),
            fmt_money(card.uncached_input_cost, &card.currency)
        ),
        52,
    );

    let (fe_hero, fe_color, fe_sub) = match card.first_edit_p50_tokens {
        Some(v) => {
            let col = if v >= 140_000 {
                err
            } else if v >= 90_000 {
                warn
            } else {
                fg
            };
            let p90 = card.first_edit_p90_tokens.unwrap_or(v);
            (
                format!("{} tok", fmt_tok_short(v)),
                col,
                truncate_ellipsis(
                    &format!(
                        "P90 {} · {} blanket-read {} ({} sess)",
                        fmt_tok_short(p90),
                        card.blanket_read_turns,
                        plural(card.blanket_read_turns, "turn", "turns"),
                        card.sessions_with_blanket_reads
                    ),
                    52,
                ),
            )
        }
        None => (
            "—".to_string(),
            muted,
            "No coding edits observed in window".to_string(),
        ),
    };

    let (split_hero, split_color, split_sub) = if card.split_total_transitions > 0 {
        let col = if card.split_cache_drop_pct >= 60.0 {
            err
        } else if card.split_cache_drop_pct >= 25.0 {
            warn
        } else {
            ok
        };
        (
            format!("-{:.1}pp", card.split_cache_drop_pct),
            col,
            truncate_ellipsis(
                &format!(
                    "{} {} · {} {} · {} uncached",
                    card.model_switch_count,
                    plural(card.model_switch_count, "switch", "switches"),
                    card.executor_cold_start_turns,
                    plural(card.executor_cold_start_turns, "spawn", "spawns"),
                    fmt_tok_short(card.split_uncached_penalty_tokens)
                ),
                52,
            ),
        )
    } else {
        (
            "0.0pp".to_string(),
            ok,
            "No model switches or subagent splits detected".to_string(),
        )
    };

    let drain_hero = if card.top_drain_share_pct > 0.0 {
        format!(
            "{} · {:.0}%",
            truncate_ellipsis(&card.top_drain_label, 16),
            card.top_drain_share_pct
        )
    } else {
        "Balanced".to_string()
    };
    let drain_sub = {
        let ctx_part = if card.top_context_share_pct > 0.0 {
            format!("Context: {} {:.0}%", card.top_context_label, card.top_context_share_pct)
        } else {
            "Context mix balanced".to_string()
        };
        let ttl_part = match (card.shortest_ttl_secs, card.shortest_ttl_model.as_deref()) {
            (Some(s), Some(_)) => format!(" · TTL {}s", s),
            (Some(s), None) => format!(" · TTL {}s", s),
            _ => String::new(),
        };
        truncate_ellipsis(&format!("{}{}", ctx_part, ttl_part), 52)
    };

    let header_sub = format!(
        "{}d window · {} sessions · {} turns · {} tokens",
        card.window_days,
        card.sessions_analyzed,
        card.total_turns,
        fmt_tok_short(card.total_tokens)
    );

    let bar_rows = [
        ("Orchestrator", card.orchestrator_hit_pct, accent),
        ("Subagent Spawn", card.executor_spawn_hit_pct, warn),
        ("Model Switch", card.model_switch_post_hit_pct, err),
    ];
    let mut profile_svg = String::new();
    for (idx, (label, pct_opt, default_col)) in bar_rows.iter().enumerate() {
        let y = 386 + idx * 22;
        let (fill_w, val_txt, col) = match pct_opt {
            Some(pct) => {
                let w = if *pct <= 0.0 {
                    0.0
                } else {
                    ((pct / 100.0) * SHARE_BAR_W).clamp(6.0, SHARE_BAR_W).round()
                };
                let c = if *pct >= 80.0 {
                    ok
                } else if *pct >= 40.0 {
                    *default_col
                } else {
                    err
                };
                (w, format!("{:.1}%", pct), c)
            }
            None => (0.0, "—".to_string(), muted),
        };
        profile_svg.push_str(&format!(
            "  <text x=\"50\" y=\"{y}\" fill=\"{muted}\" font-size=\"11\" font-family=\"{mono}\">{lbl}</text>\n\
             \x20 <rect x=\"{bx:.0}\" y=\"{by}\" width=\"{bw:.0}\" height=\"8\" rx=\"4\" fill=\"{track}\"/>\n",
            y = y,
            muted = muted,
            mono = MONO_FONT_STACK,
            lbl = xml_escape(label),
            bx = SHARE_BAR_X,
            by = y - 8,
            bw = SHARE_BAR_W,
            track = track,
        ));
        if fill_w > 0.0 {
            profile_svg.push_str(&format!(
                "  <rect x=\"{bx:.0}\" y=\"{by}\" width=\"{fw:.0}\" height=\"8\" rx=\"4\" fill=\"{col}\"/>\n",
                bx = SHARE_BAR_X,
                by = y - 8,
                fw = fill_w,
                col = col,
            ));
        }
        profile_svg.push_str(&format!(
            "  <text x=\"434\" y=\"{y}\" text-anchor=\"end\" fill=\"{fg}\" font-size=\"11\" font-weight=\"600\" font-family=\"{mono}\">{val}</text>\n",
            y = y,
            fg = if pct_opt.is_some() { fg } else { muted },
            mono = MONO_FONT_STACK,
            val = xml_escape(&val_txt),
        ));
    }

    let takeaway_1 = truncate_ellipsis(&card.takeaway_primary, 64);
    let takeaway_2 = truncate_ellipsis(&card.takeaway_secondary, 66);

    let raw_svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{svg_w}\" height=\"{svg_h}\" viewBox=\"0 0 {svg_w} {svg_h}\">\n\
         \x20 <rect width=\"{svg_w}\" height=\"{svg_h}\" rx=\"18\" fill=\"{bg}\" stroke=\"{border}\" stroke-width=\"1.5\"/>\n\
         \x20 <rect x=\"32\" y=\"0\" width=\"856\" height=\"3.5\" rx=\"2\" fill=\"{stamp_fg}\"/>\n\
         \x20 <g transform=\"translate(32, 24)\">\n\
         \x20   <rect x=\"0\" y=\"0\" width=\"34\" height=\"34\" rx=\"9\" fill=\"{logo_bg}\" stroke=\"{border}\" stroke-width=\"1\"/>\n\
         \x20   <path d=\"M9 9L16 17L9 25H13.2L20.2 17L13.2 9H9Z\" fill=\"#F8FAFC\"/>\n\
         \x20   <path d=\"M16 9L23 17L16 25H20.2L27.2 17L20.2 9H16Z\" fill=\"#38BDF8\"/>\n\
         \x20 </g>\n\
         \x20 <text x=\"76\" y=\"39\" fill=\"{fg}\" font-size=\"16\" font-weight=\"700\" font-family=\"{sans}\">AGY HYPER · LOCAL AUDIT HEALTH BADGE</text>\n\
         \x20 <text x=\"76\" y=\"58\" fill=\"{muted}\" font-size=\"11.5\" font-family=\"{mono}\">{header_sub}</text>\n\
         \x20 <rect x=\"726\" y=\"20\" width=\"162\" height=\"44\" rx=\"10\" fill=\"{stamp_bg}\" stroke=\"{stamp_stroke}\" stroke-width=\"1.5\"/>\n\
         \x20 <circle cx=\"748\" cy=\"42\" r=\"5\" fill=\"{stamp_fg}\"/>\n\
         \x20 <text x=\"816\" y=\"48\" text-anchor=\"middle\" fill=\"{stamp_fg}\" font-size=\"16\" font-weight=\"700\" letter-spacing=\"1.2\" font-family=\"{mono}\">{verdict}</text>\n\
         \x20 <g>\n\
         \x20   <rect x=\"32\" y=\"84\" width=\"420\" height=\"114\" rx=\"12\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20   <text x=\"50\" y=\"108\" fill=\"{muted}\" font-size=\"11\" font-weight=\"700\" font-family=\"{sans}\">CACHE HIT %</text>\n\
         \x20   <text x=\"50\" y=\"154\" fill=\"{cache_color}\" font-size=\"36\" font-weight=\"700\" font-family=\"{mono}\">{cache_hero}</text>\n\
         \x20   <text x=\"50\" y=\"180\" fill=\"{muted}\" font-size=\"11.5\" font-family=\"{sans}\">{cache_sub}</text>\n\
         \x20   <rect x=\"468\" y=\"84\" width=\"420\" height=\"114\" rx=\"12\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20   <text x=\"486\" y=\"108\" fill=\"{muted}\" font-size=\"11\" font-weight=\"700\" font-family=\"{sans}\">FIRST-EDIT BLIND READ P50</text>\n\
         \x20   <text x=\"486\" y=\"154\" fill=\"{fe_color}\" font-size=\"36\" font-weight=\"700\" font-family=\"{mono}\">{fe_hero}</text>\n\
         \x20   <text x=\"486\" y=\"180\" fill=\"{muted}\" font-size=\"11.5\" font-family=\"{sans}\">{fe_sub}</text>\n\
         \x20   <rect x=\"32\" y=\"212\" width=\"420\" height=\"114\" rx=\"12\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20   <text x=\"50\" y=\"236\" fill=\"{muted}\" font-size=\"11\" font-weight=\"700\" font-family=\"{sans}\">SPLIT / SWITCH CACHE DROP</text>\n\
         \x20   <text x=\"50\" y=\"282\" fill=\"{split_color}\" font-size=\"36\" font-weight=\"700\" font-family=\"{mono}\">{split_hero}</text>\n\
         \x20   <text x=\"50\" y=\"308\" fill=\"{muted}\" font-size=\"11.5\" font-family=\"{sans}\">{split_sub}</text>\n\
         \x20   <rect x=\"468\" y=\"212\" width=\"420\" height=\"114\" rx=\"12\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20   <text x=\"486\" y=\"236\" fill=\"{muted}\" font-size=\"11\" font-weight=\"700\" font-family=\"{sans}\">TOP QUOTA DRAIN</text>\n\
         \x20   <text x=\"486\" y=\"280\" fill=\"{accent}\" font-size=\"26\" font-weight=\"700\" font-family=\"{mono}\">{drain_hero}</text>\n\
         \x20   <text x=\"486\" y=\"308\" fill=\"{muted}\" font-size=\"11.5\" font-family=\"{sans}\">{drain_sub}</text>\n\
         \x20 </g>\n\
         \x20 <rect x=\"32\" y=\"342\" width=\"856\" height=\"106\" rx=\"12\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20 <text x=\"50\" y=\"364\" fill=\"{muted}\" font-size=\"10.5\" font-weight=\"700\" font-family=\"{sans}\">ROLE &amp; TRANSITION CACHE PROFILE</text>\n\
         {profile_svg}\
         \x20 <line x1=\"454\" y1=\"358\" x2=\"454\" y2=\"434\" stroke=\"{border}\" stroke-width=\"1\"/>\n\
         \x20 <text x=\"474\" y=\"364\" fill=\"{muted}\" font-size=\"10.5\" font-weight=\"700\" font-family=\"{sans}\">ANONYMIZED VERDICT &amp; PRIVACY LOCK</text>\n\
         \x20 <text x=\"474\" y=\"386\" fill=\"{ok}\" font-size=\"11\" font-weight=\"600\" font-family=\"{mono}\">REDACTED · 0 paths · 0 repos · 0 UUIDs · 0 prompts</text>\n\
         \x20 <text x=\"474\" y=\"411\" fill=\"{fg}\" font-size=\"12\" font-family=\"{sans}\">{takeaway_1}</text>\n\
         \x20 <text x=\"474\" y=\"431\" fill=\"{muted}\" font-size=\"11.5\" font-family=\"{sans}\">{takeaway_2}</text>\n\
         \x20 <text x=\"32\" y=\"476\" fill=\"{muted}\" font-size=\"11.5\" font-family=\"{mono}\">audited locally by agy-hyper · github.com/BobbyCats/agy-hyper</text>\n\
         \x20 <text x=\"888\" y=\"476\" text-anchor=\"end\" fill=\"{muted}\" font-size=\"11.5\" font-family=\"{mono}\">v{ver} · 100% local · zero cloud</text>\n\
         </svg>\n",
        svg_w = SHARE_SVG_W,
        svg_h = SHARE_SVG_H,
        sans = SANS_FONT_STACK,
        mono = MONO_FONT_STACK,
        header_sub = xml_escape(&header_sub),
        verdict = xml_escape(&card.verdict),
        cache_hero = xml_escape(&cache_hero),
        cache_sub = xml_escape(&cache_sub),
        fe_hero = xml_escape(&fe_hero),
        fe_sub = xml_escape(&fe_sub),
        split_hero = xml_escape(&split_hero),
        split_sub = xml_escape(&split_sub),
        drain_hero = xml_escape(&drain_hero),
        drain_sub = xml_escape(&drain_sub),
        takeaway_1 = xml_escape(&takeaway_1),
        takeaway_2 = xml_escape(&takeaway_2),
        ver = xml_escape(&card.version),
    );
    scrub_sensitive_text(&raw_svg)
}

/// Rasterize the anonymized shareable Health Badge card into a 2x Retina 16:9 PNG (1920×1080).
pub fn render_share_card_png(report: &BurnAuditReport, dark: bool) -> Result<Vec<u8>> {
    let svg = render_share_card_svg(report, dark);
    assert_share_card_clean(&svg)?;
    render_svg_to_png_sized(&svg, dark, 2.0, SHARE_SVG_W, SHARE_SVG_H, SHARE_PNG_PAD)
}

/// Zero-dependency RFC 4648 base64 encoder for daemon RPC PNG transport.
pub fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{
        build_audit_report_from_parts_with_roles, SessionRoleMeta,
    };
    use crate::bench::{ContextBreakdown, ModelSwitchEvent, SessionBenchmark, SessionSplitStats};
    use crate::config::{HyperConfig, ModelPriceRate};
    use crate::usage::{ModelTtlEstimate, ModelUsageBreakdown, TtlBucketSummary, UsageWindowReport};
    use std::collections::{BTreeMap, HashMap};

    fn make_synthetic_report_with_sensitive_inputs() -> BurnAuditReport {
        let orch = SessionBenchmark {
            conversation_id: "11111111-2222-3333-4444-555555555555".into(),
            transcript_path: "/Users/alice/secret-workspace/brain/transcript.jsonl".into(),
            total_turns: 8,
            valid_turns: 8,
            dropped_idle_gap_turns: 0,
            total_wall_ms: 40_000,
            total_llm_ms: 30_000,
            total_tool_ms: 10_000,
            median_turn_ms: 5_000,
            p90_turn_ms: 8_000,
            median_llm_ms: 3_500,
            effective_tps: 75.0,
            total_input_tokens: 180_000,
            total_cache_read_tokens: 920_000,
            total_output_tokens: 4_000,
            cache_hit_ratio: 0.836,
            cache_break_turns: 2,
            hyper_tool_calls: 4,
            total_tool_calls: 12,
            hyper_adoption_ratio: 0.33,
            model_usage: BTreeMap::new(),
            context_breakdown: ContextBreakdown {
                file_read_bytes: 120_000,
                command_output_bytes: 40_000,
                total_bytes: 160_000,
                ..Default::default()
            },
            first_edit_context_tokens: Some(152_000),
            max_parallel_reads: 6,
            blanket_read_turns: 5,
            split_stats: SessionSplitStats {
                primary_model: Some("gemini-4-neon-xhigh".into()),
                distinct_models: vec!["gemini-4-neon-xhigh".into(), "gemini-4-carbon-medium".into()],
                model_switch_count: 1,
                cross_family_switch_count: 1,
                switch_uncached_input_tokens: 145_000,
                avg_switch_pre_hit_ratio: 0.96,
                avg_switch_post_hit_ratio: 0.02,
                avg_switch_cache_drop: 0.94,
                first_turn_input_tokens: 20_000,
                first_turn_cache_read_tokens: 0,
                steady_input_tokens: 15_000,
                steady_cache_read_tokens: 920_000,
                steady_turns: 6,
                delegation_tool_calls: 2,
                model_switches: vec![ModelSwitchEvent {
                    step_index: 12,
                    from_model: "gemini-4-neon".into(),
                    to_model: "gemini-4-carbon".into(),
                    from_raw_model: "gemini-4-neon-xhigh".into(),
                    to_raw_model: "gemini-4-carbon-medium".into(),
                    same_family: false,
                    prev_cache_hit_ratio: 0.96,
                    post_cache_hit_ratio: 0.02,
                    cache_drop_ratio: 0.94,
                    uncached_input_tokens: 145_000,
                    prompt_tokens: 148_000,
                }],
            },
            turns: vec![],
        };

        let sub = SessionBenchmark {
            conversation_id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            transcript_path: "C:\\Users\\alice\\private-repo\\transcript.jsonl".into(),
            total_turns: 4,
            valid_turns: 4,
            dropped_idle_gap_turns: 0,
            total_wall_ms: 16_000,
            total_llm_ms: 12_000,
            total_tool_ms: 4_000,
            median_turn_ms: 4_000,
            p90_turn_ms: 6_000,
            median_llm_ms: 3_000,
            effective_tps: 65.0,
            total_input_tokens: 52_000,
            total_cache_read_tokens: 180_000,
            total_output_tokens: 1_200,
            cache_hit_ratio: 0.775,
            cache_break_turns: 0,
            hyper_tool_calls: 2,
            total_tool_calls: 6,
            hyper_adoption_ratio: 0.33,
            model_usage: BTreeMap::new(),
            context_breakdown: ContextBreakdown::default(),
            first_edit_context_tokens: None,
            max_parallel_reads: 2,
            blanket_read_turns: 0,
            split_stats: SessionSplitStats {
                primary_model: Some("gemini-4-carbon-medium".into()),
                distinct_models: vec!["gemini-4-carbon-medium".into()],
                first_turn_input_tokens: 46_000,
                first_turn_cache_read_tokens: 2_000,
                steady_input_tokens: 6_000,
                steady_cache_read_tokens: 178_000,
                steady_turns: 3,
                ..Default::default()
            },
            turns: vec![],
        };

        let mut roles = HashMap::new();
        roles.insert(
            orch.conversation_id.clone(),
            SessionRoleMeta {
                is_subagent: false,
                parent_conversation_id: None,
            },
        );
        roles.insert(
            sub.conversation_id.clone(),
            SessionRoleMeta {
                is_subagent: true,
                parent_conversation_id: Some(orch.conversation_id.clone()),
            },
        );

        let usage = UsageWindowReport {
            window: "14d".into(),
            currency: "USD".into(),
            fx_rate: 1.0,
            fx_source: "offline".into(),
            fx_updated_at_ms: None,
            available_currencies: BTreeMap::from([("USD".into(), 1.0)]),
            total_turns: 12,
            active_sessions: 2,
            total_input_tokens: 232_000,
            total_cache_read_tokens: 1_100_000,
            total_output_tokens: 5_200,
            total_tokens: 1_337_200,
            cache_hit_ratio: 0.825,
            attributed_tokens: 1_337_200,
            unattributed_tokens: 0,
            attributed_ratio: 1.0,
            estimated_cost_usd: 4.2,
            cache_saved_usd: 11.5,
            estimated_cost: 4.2,
            cache_saved: 11.5,
            ledger_sessions_tracked: 2,
            ledger_scan_ms: 1,
            series: vec![],
            models: vec![
                ModelUsageBreakdown {
                    model: "gemini-4-neon-xhigh".into(),
                    attributed: true,
                    priced: true,
                    turns: 7,
                    input_tokens: 100_000,
                    cache_read_tokens: 920_000,
                    output_tokens: 3_500,
                    total_tokens: 1_023_500,
                    cache_hit_ratio: 0.902,
                    estimated_cost_usd: 3.1,
                    estimated_cost: 3.1,
                    cache_saved_usd: 9.8,
                    cache_saved: 9.8,
                    rate_applied: Some(ModelPriceRate {
                        input_per_m: 2.0,
                        cache_read_per_m: 0.2,
                        output_per_m: 12.0,
                    }),
                },
                ModelUsageBreakdown {
                    model: "/Users/alice/custom-model".into(),
                    attributed: true,
                    priced: false,
                    turns: 1,
                    input_tokens: 500,
                    cache_read_tokens: 500,
                    output_tokens: 100,
                    total_tokens: 1_100,
                    cache_hit_ratio: 0.5,
                    estimated_cost_usd: 0.0,
                    estimated_cost: 0.0,
                    cache_saved_usd: 0.0,
                    cache_saved: 0.0,
                    rate_applied: None,
                },
            ],
        };
        let cliffs = vec![ModelTtlEstimate {
            model_family: "gemini-4-neon".into(),
            effective_ttl_secs: 120,
            source: "empirical".into(),
            total_samples: 40,
            buckets: vec![TtlBucketSummary {
                label: "120-180s".into(),
                upper_secs: 180,
                samples: 12,
                cold_samples: 8,
                avg_hit_ratio: 0.3,
                cold_rate: 0.66,
            }],
        }];

        build_audit_report_from_parts_with_roles(
            14,
            &[orch, sub],
            &roles,
            &usage,
            cliffs,
            &HyperConfig::default(),
        )
    }

    #[test]
    fn share_card_enforces_hard_redaction_and_renders_retina_png_dark_and_light() {
        let report = make_synthetic_report_with_sensitive_inputs();
        assert_eq!(report.verdict, "BLEEDING");
        assert_eq!(report.split_cache.model_switch_count, 1);
        assert_eq!(report.split_cache.executor_cold_start_turns, 1);
        assert_eq!(report.split_cache.executor_cross_model_handoffs, 1);
        assert_eq!(report.split_cache.total_transitions, 2);
        assert!(report.split_cache.headline_cache_drop_pct > 75.0);
        assert_eq!(report.split_cache.uncached_penalty_tokens, 191_000);
        assert!(report.findings.iter().any(|f| f.code == "split_cache_drop"));

        for dark in [true, false] {
            let svg = render_share_card_svg(&report, dark);
            assert!(svg.starts_with("<svg "));
            assert!(svg.ends_with("</svg>\n"));
            assert!(svg.contains("BLEEDING"));
            assert!(svg.contains("CACHE HIT %"));
            assert!(svg.contains("FIRST-EDIT BLIND READ P50"));
            assert!(svg.contains("SPLIT / SWITCH CACHE DROP"));
            assert!(svg.contains("TOP QUOTA DRAIN"));
            assert!(svg.contains("audited locally by agy-hyper · github.com/BobbyCats/agy-hyper"));

            // Strict privacy assertions: ZERO paths, usernames, workspaces, or UUIDs!
            assert_share_card_clean(&svg).expect("share card must pass privacy gate");
            assert!(!svg.contains("/Users/"));
            assert!(!svg.contains("C:\\"));
            assert!(!svg.contains("alice"));
            assert!(!svg.contains("secret-workspace"));
            assert!(!svg.contains("private-repo"));
            assert!(!svg.contains("11111111-2222-3333-4444-555555555555"));
            assert!(!svg.contains("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"));

            let png = render_share_card_png(&report, dark).expect("share PNG render");
            assert_eq!(&png[0..8], b"\x89PNG\r\n\x1a\n");
            let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
            let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
            assert_eq!((w, h), (1920, 1080));
        }
    }

    #[test]
    fn sanitizers_and_base64_handle_hostile_inputs() {
        assert_eq!(sanitize_model_family_label("/Users/alice/model"), "other-model");
        assert_eq!(
            sanitize_model_family_label("11111111-2222-3333-4444-555555555555"),
            "other-model"
        );
        assert_eq!(sanitize_model_family_label("gemini-4-neon-xhigh"), "gemini-4-neon");
        let scrubbed = scrub_sensitive_text(
            "Found /Users/alice/repo and C:\\Users\\bob\\proj and 11111111-2222-3333-4444-555555555555",
        );
        assert!(!scrubbed.contains("/Users/"));
        assert!(!scrubbed.contains("C:\\"));
        assert!(!scrubbed.contains("11111111-2222"));
        assert_eq!(encode_base64(b"agy-hyper"), "YWd5LWh5cGVy");
    }
}
