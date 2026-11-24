use crate::bench::{analyze_transcript, ContextCategoryShare, SessionBenchmark};
use crate::config::HyperConfig;
use crate::sessions::discover_session_summaries;
use crate::usage::{
    default_brain_dirs, model_ttl_family, now_epoch_ms, resolve_model_rate, ModelTtlEstimate,
    UsageLedger, UsageWindow, UsageWindowReport, UNATTRIBUTED_MODEL,
};
use anyhow::{anyhow, Result};
use resvg::{tiny_skia, usvg};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

pub use crate::share_card::{
    assert_share_card_clean, build_anonymized_share_card, encode_base64, render_share_card_png,
    render_share_card_svg, sanitize_model_family_label, scrub_sensitive_text, AnonymizedShareCard,
    AuditShareCardPayload, SHARE_PNG_PAD, SHARE_SVG_H, SHARE_SVG_W,
};

pub const AUDIT_SVG_W: u32 = 920;
pub const AUDIT_SVG_H: u32 = 560;
const AUDIT_PNG_PAD: u32 = 20;
const TTL_BAR_X: f64 = 184.0;
const TTL_BAR_MAX_W: f64 = 164.0;
pub(crate) const SANS_FONT_STACK: &str =
    "Helvetica Neue, Segoe UI, DejaVu Sans, Liberation Sans, Arial, system-ui, sans-serif";
pub(crate) const MONO_FONT_STACK: &str =
    "Menlo, Consolas, DejaVu Sans Mono, Liberation Mono, Courier New, ui-monospace, monospace";

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionRoleMeta {
    pub is_subagent: bool,
    pub parent_conversation_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TopQuotaDrain {
    pub model_family: String,
    pub share_pct: f64,
    pub context_category: String,
    pub context_share_pct: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SplitTransitionPair {
    pub from_model: String,
    pub to_model: String,
    pub kind: String, // "model_switch" | "subagent_handoff"
    pub transitions: usize,
    pub pre_cache_hit_ratio: f64,
    pub post_cache_hit_ratio: f64,
    pub cache_drop_pct: f64,
    pub uncached_input_tokens: u64,
    pub est_uncached_cost_usd: f64,
    pub est_uncached_cost: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SplitCacheDiagnostics {
    pub orchestrator_sessions: usize,
    pub orchestrator_turns: usize,
    pub orchestrator_cache_hit_ratio: f64,
    pub executor_sessions: usize,
    pub executor_turns: usize,
    pub executor_cache_hit_ratio: f64,
    pub executor_cold_start_turns: usize,
    pub executor_cold_start_hit_ratio: f64,
    pub executor_steady_hit_ratio: f64,
    pub executor_cross_model_handoffs: usize,
    pub model_switch_count: usize,
    pub cross_family_switch_count: usize,
    pub model_switch_sessions: usize,
    pub model_switch_pre_hit_ratio: f64,
    pub model_switch_post_hit_ratio: f64,
    pub model_switch_drop_pct: f64,
    pub subagent_handoff_drop_pct: f64,
    pub headline_cache_drop_pct: f64,
    pub total_transitions: usize,
    pub uncached_penalty_tokens: u64,
    pub est_uncached_penalty_usd: f64,
    pub est_uncached_penalty: f64,
    pub top_transitions: Vec<SplitTransitionPair>,
}

fn default_verdict() -> String {
    "HEALTHY".to_string()
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
    #[serde(default = "default_verdict")]
    pub verdict: String,
    #[serde(default)]
    pub verdict_reason: String,
    #[serde(default)]
    pub top_quota_drain: TopQuotaDrain,
    #[serde(default)]
    pub split_cache: SplitCacheDiagnostics,
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

    let summaries = discover_session_summaries(cap);
    let mut roles: HashMap<String, SessionRoleMeta> = HashMap::with_capacity(summaries.len());
    let mut paths: Vec<PathBuf> = Vec::with_capacity(summaries.len());
    for sum in summaries {
        let p = PathBuf::from(&sum.transcript_path);
        let within_window = fs::metadata(&p)
            .and_then(|m| m.modified())
            .map(|mt| mt >= cutoff)
            .unwrap_or(true);
        if within_window {
            roles.insert(
                sum.conversation_id.clone(),
                SessionRoleMeta {
                    is_subagent: sum.is_subagent,
                    parent_conversation_id: sum.parent_conversation_id,
                },
            );
            paths.push(p);
        }
    }

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

    build_audit_report_from_parts_with_roles(
        clamped_days,
        &sessions,
        &roles,
        &usage_report,
        ttl_cliffs,
        cfg,
    )
}

pub fn build_audit_report_from_parts(
    window_days: u32,
    sessions: &[SessionBenchmark],
    usage: &UsageWindowReport,
    ttl_cliffs: Vec<ModelTtlEstimate>,
) -> BurnAuditReport {
    let empty_roles = HashMap::new();
    let cfg = HyperConfig::default();
    build_audit_report_from_parts_with_roles(
        window_days,
        sessions,
        &empty_roles,
        usage,
        ttl_cliffs,
        &cfg,
    )
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

fn uncached_penalty_usd(model: &str, tokens: u64, cfg: &HyperConfig) -> f64 {
    if tokens == 0 {
        return 0.0;
    }
    let Some(rate) = resolve_model_rate(model, cfg) else {
        return 0.0;
    };
    let delta_per_m = (rate.input_per_m - rate.cache_read_per_m).max(0.0);
    (tokens as f64 / 1_000_000.0) * delta_per_m
}

fn compute_split_cache_diagnostics(
    sessions: &[SessionBenchmark],
    roles: &HashMap<String, SessionRoleMeta>,
    fx: f64,
    cfg: &HyperConfig,
) -> SplitCacheDiagnostics {
    let primary_by_conv: HashMap<&str, Option<&str>> = sessions
        .iter()
        .map(|s| {
            (
                s.conversation_id.as_str(),
                s.split_stats.primary_model.as_deref(),
            )
        })
        .collect();

    let mut orch_sessions = 0usize;
    let mut orch_turns = 0usize;
    let mut orch_in = 0u64;
    let mut orch_cr = 0u64;

    let mut exec_sessions = 0usize;
    let mut exec_turns = 0usize;
    let mut exec_in = 0u64;
    let mut exec_cr = 0u64;
    let mut exec_cold_turns = 0usize;
    let mut exec_cold_in = 0u64;
    let mut exec_cold_cr = 0u64;
    let mut exec_steady_in = 0u64;
    let mut exec_steady_cr = 0u64;
    let mut exec_cross_model = 0usize;

    let mut switch_count = 0usize;
    let mut cross_family_switches = 0usize;
    let mut switch_sessions = 0usize;
    let mut switch_pre_sum = 0.0f64;
    let mut switch_post_sum = 0.0f64;
    let mut switch_drop_sum = 0.0f64;
    let mut switch_uncached_tokens = 0u64;
    let mut penalty_usd = 0.0f64;

    #[derive(Default)]
    struct PairAccum {
        transitions: usize,
        pre_sum: f64,
        post_sum: f64,
        drop_sum: f64,
        uncached_tokens: u64,
        cost_usd: f64,
    }
    let mut pair_map: BTreeMap<(String, String, String), PairAccum> = BTreeMap::new();

    // First pass: accumulate orchestrator baseline cache hit ratio so subagent handoff pairs
    // can compare against the orchestrator average.
    for s in sessions {
        let is_sub = roles
            .get(&s.conversation_id)
            .map(|r| r.is_subagent)
            .unwrap_or(false);
        if is_sub {
            exec_sessions += 1;
            exec_turns += s.valid_turns;
            exec_in = exec_in.saturating_add(s.total_input_tokens);
            exec_cr = exec_cr.saturating_add(s.total_cache_read_tokens);
            exec_steady_in = exec_steady_in.saturating_add(s.split_stats.steady_input_tokens);
            exec_steady_cr = exec_steady_cr.saturating_add(s.split_stats.steady_cache_read_tokens);
        } else {
            orch_sessions += 1;
            orch_turns += s.valid_turns;
            orch_in = orch_in.saturating_add(s.total_input_tokens);
            orch_cr = orch_cr.saturating_add(s.total_cache_read_tokens);
        }
    }

    let orch_denom = orch_in.saturating_add(orch_cr);
    let orch_hit = if orch_denom > 0 {
        orch_cr as f64 / orch_denom as f64
    } else {
        0.0
    };
    let exec_denom = exec_in.saturating_add(exec_cr);
    let exec_hit = if exec_denom > 0 {
        exec_cr as f64 / exec_denom as f64
    } else {
        0.0
    };
    let exec_steady_denom = exec_steady_in.saturating_add(exec_steady_cr);
    let exec_steady_hit = if exec_steady_denom > 0 {
        exec_steady_cr as f64 / exec_steady_denom as f64
    } else {
        exec_hit
    };
    let handoff_baseline_hit = if orch_denom > 0 {
        orch_hit
    } else {
        exec_steady_hit
    };

    // Second pass: model switches & subagent cold-start handoffs.
    for s in sessions {
        let role = roles.get(&s.conversation_id);
        let is_sub = role.map(|r| r.is_subagent).unwrap_or(false);

        if s.split_stats.model_switch_count > 0 {
            switch_sessions += 1;
            for ev in &s.split_stats.model_switches {
                switch_count += 1;
                if !ev.same_family {
                    cross_family_switches += 1;
                }
                switch_pre_sum += ev.prev_cache_hit_ratio;
                switch_post_sum += ev.post_cache_hit_ratio;
                switch_drop_sum += ev.cache_drop_ratio;
                switch_uncached_tokens =
                    switch_uncached_tokens.saturating_add(ev.uncached_input_tokens);
                let c_usd = uncached_penalty_usd(&ev.to_raw_model, ev.uncached_input_tokens, cfg);
                penalty_usd += c_usd;

                let from_lbl = sanitize_model_family_label(&ev.from_model);
                let to_lbl = sanitize_model_family_label(&ev.to_model);
                let acc = pair_map
                    .entry((from_lbl, to_lbl, "model_switch".to_string()))
                    .or_default();
                acc.transitions += 1;
                acc.pre_sum += ev.prev_cache_hit_ratio;
                acc.post_sum += ev.post_cache_hit_ratio;
                acc.drop_sum += ev.cache_drop_ratio;
                acc.uncached_tokens = acc.uncached_tokens.saturating_add(ev.uncached_input_tokens);
                acc.cost_usd += c_usd;
            }
        }

        if is_sub {
            let first_in = s.split_stats.first_turn_input_tokens;
            let first_cr = s.split_stats.first_turn_cache_read_tokens;
            let first_tot = first_in.saturating_add(first_cr);
            if first_tot > 0 {
                exec_cold_turns += 1;
                exec_cold_in = exec_cold_in.saturating_add(first_in);
                exec_cold_cr = exec_cold_cr.saturating_add(first_cr);

                let child_raw = s
                    .split_stats
                    .primary_model
                    .as_deref()
                    .unwrap_or(UNATTRIBUTED_MODEL);
                let child_fam = sanitize_model_family_label(&model_ttl_family(Some(child_raw)));
                let parent_raw = role
                    .and_then(|r| r.parent_conversation_id.as_deref())
                    .and_then(|pid| primary_by_conv.get(pid).copied().flatten());
                let parent_fam = parent_raw
                    .map(|pm| sanitize_model_family_label(&model_ttl_family(Some(pm))))
                    .unwrap_or_else(|| "orchestrator".to_string());
                if let Some(pm) = parent_raw {
                    let p_fam = model_ttl_family(Some(pm));
                    let c_fam = model_ttl_family(Some(child_raw));
                    if p_fam != UNATTRIBUTED_MODEL && c_fam != UNATTRIBUTED_MODEL && p_fam != c_fam {
                        exec_cross_model += 1;
                    }
                }

                let post_hit = first_cr as f64 / first_tot as f64;
                let drop = (handoff_baseline_hit - post_hit).max(0.0);
                let c_usd = uncached_penalty_usd(child_raw, first_in, cfg);
                penalty_usd += c_usd;

                let acc = pair_map
                    .entry((parent_fam, child_fam, "subagent_handoff".to_string()))
                    .or_default();
                acc.transitions += 1;
                acc.pre_sum += handoff_baseline_hit;
                acc.post_sum += post_hit;
                acc.drop_sum += drop;
                acc.uncached_tokens = acc.uncached_tokens.saturating_add(first_in);
                acc.cost_usd += c_usd;
            }
        }
    }

    let exec_cold_denom = exec_cold_in.saturating_add(exec_cold_cr);
    let exec_cold_hit = if exec_cold_denom > 0 {
        exec_cold_cr as f64 / exec_cold_denom as f64
    } else {
        0.0
    };
    let subagent_drop_pct = if exec_cold_turns > 0 {
        round1(((handoff_baseline_hit - exec_cold_hit).max(0.0)) * 100.0)
    } else {
        0.0
    };

    let (sw_pre, sw_post, sw_drop_pct) = if switch_count > 0 {
        let n = switch_count as f64;
        (
            round4(switch_pre_sum / n),
            round4(switch_post_sum / n),
            round1((switch_drop_sum / n) * 100.0),
        )
    } else {
        (0.0, 0.0, 0.0)
    };

    let total_transitions = switch_count + exec_cold_turns;
    let headline_cache_drop_pct = if total_transitions > 0 {
        let weighted = sw_drop_pct * (switch_count as f64)
            + subagent_drop_pct * (exec_cold_turns as f64);
        round1(weighted / total_transitions as f64)
    } else {
        0.0
    };
    let uncached_penalty_tokens = switch_uncached_tokens.saturating_add(exec_cold_in);

    let mut top_transitions: Vec<SplitTransitionPair> = pair_map
        .into_iter()
        .map(|((from_model, to_model, kind), acc)| {
            let n = acc.transitions.max(1) as f64;
            SplitTransitionPair {
                from_model,
                to_model,
                kind,
                transitions: acc.transitions,
                pre_cache_hit_ratio: round4(acc.pre_sum / n),
                post_cache_hit_ratio: round4(acc.post_sum / n),
                cache_drop_pct: round1((acc.drop_sum / n) * 100.0),
                uncached_input_tokens: acc.uncached_tokens,
                est_uncached_cost_usd: round4(acc.cost_usd),
                est_uncached_cost: round4(acc.cost_usd * fx),
            }
        })
        .collect();
    top_transitions.sort_by(|a, b| {
        b.uncached_input_tokens
            .cmp(&a.uncached_input_tokens)
            .then_with(|| b.transitions.cmp(&a.transitions))
    });
    top_transitions.truncate(4);

    SplitCacheDiagnostics {
        orchestrator_sessions: orch_sessions,
        orchestrator_turns: orch_turns,
        orchestrator_cache_hit_ratio: round4(orch_hit),
        executor_sessions: exec_sessions,
        executor_turns: exec_turns,
        executor_cache_hit_ratio: round4(exec_hit),
        executor_cold_start_turns: exec_cold_turns,
        executor_cold_start_hit_ratio: round4(exec_cold_hit),
        executor_steady_hit_ratio: round4(exec_steady_hit),
        executor_cross_model_handoffs: exec_cross_model,
        model_switch_count: switch_count,
        cross_family_switch_count: cross_family_switches,
        model_switch_sessions: switch_sessions,
        model_switch_pre_hit_ratio: sw_pre,
        model_switch_post_hit_ratio: sw_post,
        model_switch_drop_pct: sw_drop_pct,
        subagent_handoff_drop_pct: subagent_drop_pct,
        headline_cache_drop_pct,
        total_transitions,
        uncached_penalty_tokens,
        est_uncached_penalty_usd: round4(penalty_usd),
        est_uncached_penalty: round4(penalty_usd * fx),
        top_transitions,
    }
}

fn compute_top_quota_drain(
    usage: &UsageWindowReport,
    context_categories: &[ContextCategoryShare],
) -> TopQuotaDrain {
    let mut by_family: BTreeMap<String, (f64, u64)> = BTreeMap::new();
    let mut total_attributed_cost = 0.0f64;
    let mut total_attributed_toks = 0u64;
    for m in &usage.models {
        if !m.attributed {
            continue;
        }
        let fam = sanitize_model_family_label(&model_ttl_family(Some(&m.model)));
        let e = by_family.entry(fam).or_insert((0.0, 0));
        e.0 += m.estimated_cost_usd;
        e.1 = e.1.saturating_add(m.total_tokens);
        total_attributed_cost += m.estimated_cost_usd;
        total_attributed_toks = total_attributed_toks.saturating_add(m.total_tokens);
    }

    let (model_family, share_pct) = if total_attributed_cost > 0.0 {
        by_family
            .into_iter()
            .max_by(|a, b| {
                a.1 .0
                    .partial_cmp(&b.1 .0)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(fam, (cost, _))| (fam, round1((cost / total_attributed_cost) * 100.0)))
            .unwrap_or_else(|| ("balanced".to_string(), 0.0))
    } else if total_attributed_toks > 0 {
        by_family
            .into_iter()
            .max_by_key(|(_, (_, toks))| *toks)
            .map(|(fam, (_, toks))| {
                (
                    fam,
                    round1((toks as f64 / total_attributed_toks as f64) * 100.0),
                )
            })
            .unwrap_or_else(|| ("balanced".to_string(), 0.0))
    } else {
        ("balanced".to_string(), 0.0)
    };

    let (context_category, context_share_pct) = context_categories
        .iter()
        .max_by_key(|c| c.bytes)
        .map(|c| (c.key.clone(), c.share_pct))
        .unwrap_or_else(|| ("none".to_string(), 0.0));

    TopQuotaDrain {
        model_family,
        share_pct,
        context_category,
        context_share_pct,
    }
}

fn classify_audit_verdict(
    overall_cache_hit_ratio: f64,
    total_turns: usize,
    first_edit_p50_tokens: Option<u64>,
    blanket_read_turns: usize,
    split_cache: &SplitCacheDiagnostics,
    findings: &[AuditFinding],
) -> (String, String) {
    let has_high = findings.iter().any(|f| f.severity == "high");
    let has_warn = findings.iter().any(|f| f.severity == "warn");

    if (total_turns >= 5 && overall_cache_hit_ratio < 0.65)
        || first_edit_p50_tokens.is_some_and(|v| v >= 140_000)
        || (split_cache.headline_cache_drop_pct >= 65.0
            && split_cache.uncached_penalty_tokens >= 100_000)
        || has_high
    {
        let reason = if total_turns >= 5 && overall_cache_hit_ratio < 0.65 {
            format!(
                "Low overall prompt cache hit ({:.1}%) causing heavy cold re-prefills.",
                overall_cache_hit_ratio * 100.0
            )
        } else if let Some(fe) = first_edit_p50_tokens.filter(|&v| v >= 140_000) {
            format!(
                "Heavy pre-edit read bloat (first-edit P50 {} tok).",
                fmt_tok_short(fe)
            )
        } else if split_cache.headline_cache_drop_pct >= 65.0
            && split_cache.uncached_penalty_tokens >= 100_000
        {
            format!(
                "Split/switch cache drop -{:.1}pp ({} uncached across {} transitions).",
                split_cache.headline_cache_drop_pct,
                fmt_tok_short(split_cache.uncached_penalty_tokens),
                split_cache.total_transitions
            )
        } else {
            "High-severity cache or context burn risk detected.".to_string()
        };
        return ("BLEEDING".to_string(), reason);
    }

    if (total_turns >= 5 && overall_cache_hit_ratio < 0.82)
        || first_edit_p50_tokens.is_some_and(|v| v >= 90_000)
        || blanket_read_turns >= 8
        || (split_cache.total_transitions > 0 && split_cache.headline_cache_drop_pct >= 30.0)
        || has_warn
    {
        let reason = if split_cache.total_transitions > 0 && split_cache.headline_cache_drop_pct >= 30.0
        {
            format!(
                "Moderate split/switch cache drop (-{:.1}pp across {} transitions).",
                split_cache.headline_cache_drop_pct, split_cache.total_transitions
            )
        } else if total_turns >= 5 && overall_cache_hit_ratio < 0.82 {
            format!(
                "Cache hit rate ({:.1}%) leaves measurable uncached input overhead.",
                overall_cache_hit_ratio * 100.0
            )
        } else {
            "Watch pre-edit context growth and cache TTL boundaries.".to_string()
        };
        return ("WATCH".to_string(), reason);
    }

    (
        "HEALTHY".to_string(),
        "Prompt cache efficiency and pre-edit context look well-controlled.".to_string(),
    )
}

pub fn build_audit_report_from_parts_with_roles(
    window_days: u32,
    sessions: &[SessionBenchmark],
    roles: &HashMap<String, SessionRoleMeta>,
    usage: &UsageWindowReport,
    mut ttl_cliffs: Vec<ModelTtlEstimate>,
    cfg: &HyperConfig,
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

    let split_cache = compute_split_cache_diagnostics(sessions, roles, fx, cfg);
    let top_quota_drain = compute_top_quota_drain(usage, &context_categories);

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

    if split_cache.total_transitions > 0
        && split_cache.headline_cache_drop_pct >= 25.0
        && split_cache.uncached_penalty_tokens >= 20_000
    {
        let sev = if split_cache.headline_cache_drop_pct >= 60.0
            && split_cache.uncached_penalty_tokens >= 100_000
        {
            "high"
        } else {
            "warn"
        };
        findings.push(AuditFinding {
            severity: sev.into(),
            code: "split_cache_drop".into(),
            title: format!(
                "Split/switch transitions drop cache by {:.1}pp ({} transitions · {} uncached)",
                split_cache.headline_cache_drop_pct,
                split_cache.total_transitions,
                fmt_tok_short(split_cache.uncached_penalty_tokens)
            ),
            detail: format!(
                "{} in-session model switch(es) and {} subagent cold-start(s) added ~{} in uncached prefill overhead.",
                split_cache.model_switch_count,
                split_cache.executor_cold_start_turns,
                fmt_money(split_cache.est_uncached_penalty, &usage.currency)
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

    let (verdict, verdict_reason) = classify_audit_verdict(
        usage.cache_hit_ratio,
        total_turns.max(usage.total_turns as usize),
        first_edit_p50_tokens,
        blanket_read_turns,
        &split_cache,
        &findings,
    );

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
        verdict,
        verdict_reason,
        top_quota_drain,
        split_cache,
    }
}

pub(crate) fn category_label(key: &str) -> &'static str {
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

pub(crate) fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub(crate) fn fmt_tok_short(n: u64) -> String {
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

pub(crate) fn fmt_money(amount: f64, ccy: &str) -> String {
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
        let fill_w = (ratio * TTL_BAR_MAX_W).round();
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
            "  <text x=\"44\" y=\"{}\" fill=\"{}\" font-size=\"11.5\" font-family=\"{mono}\">{}</text>\n\
             \x20 <rect x=\"{bar_x:.0}\" y=\"{}\" width=\"{bar_w:.0}\" height=\"10\" rx=\"5\" fill=\"{}\"/>\n\
             \x20 <rect x=\"{bar_x:.0}\" y=\"{}\" width=\"{:.0}\" height=\"10\" rx=\"5\" fill=\"{}\"/>\n\
             \x20 <text x=\"434\" y=\"{}\" text-anchor=\"end\" fill=\"{}\" font-size=\"11\" font-family=\"{mono}\">{}s · {}</text>\n",
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
            tag,
            mono = MONO_FONT_STACK,
            bar_x = TTL_BAR_X,
            bar_w = TTL_BAR_MAX_W,
        ));
    }
    if report.model_ttl_cliffs.is_empty() {
        ttl_rows.push_str(&format!(
            "  <text x=\"44\" y=\"268\" fill=\"{}\" font-size=\"12\" font-family=\"{sans}\">Not enough multi-turn gaps yet for empirical TTL.</text>\n",
            muted,
            sans = SANS_FONT_STACK
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
             \x20 <text x=\"{}\" y=\"{}\" fill=\"{}\" font-size=\"11\" font-family=\"{sans}\">{} {:.1}%</text>\n",
            lx,
            ly - 8,
            col,
            lx + 14,
            ly,
            muted,
            xml_escape(category_label(&c.key)),
            c.share_pct,
            sans = SANS_FONT_STACK
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
             \x20 <text x=\"64\" y=\"{}\" fill=\"{}\" font-size=\"12\" font-family=\"{sans}\">{}</text>\n",
            y - 4,
            dot,
            y,
            fg,
            txt,
            sans = SANS_FONT_STACK
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
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{svg_w}\" height=\"{svg_h}\" viewBox=\"0 0 {svg_w} {svg_h}\">\n\
         \x20 <rect width=\"{svg_w}\" height=\"{svg_h}\" rx=\"16\" fill=\"{bg}\" stroke=\"{border}\" stroke-width=\"1.5\"/>\n\
         \x20 <text x=\"28\" y=\"42\" fill=\"{fg}\" font-size=\"18\" font-weight=\"700\" font-family=\"{sans}\">AGY HYPER · LOCAL TELEMETRY BURN AUDIT</text>\n\
         \x20 <text x=\"892\" y=\"40\" text-anchor=\"end\" fill=\"{accent}\" font-size=\"12\" font-weight=\"600\" font-family=\"{mono}\">v{ver}</text>\n\
         \x20 <text x=\"28\" y=\"64\" fill=\"{muted}\" font-size=\"12\" font-family=\"{mono}\">{header_sub}</text>\n\
         \x20 <g>\n\
         \x20   <rect x=\"28\" y=\"84\" width=\"206\" height=\"88\" rx=\"10\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20   <text x=\"44\" y=\"108\" fill=\"{muted}\" font-size=\"11\" font-family=\"{sans}\">PROMPT CACHE HIT</text>\n\
         \x20   <text x=\"44\" y=\"138\" fill=\"{accent}\" font-size=\"24\" font-weight=\"700\" font-family=\"{mono}\">{cache_pct}</text>\n\
         \x20   <text x=\"44\" y=\"158\" fill=\"{muted}\" font-size=\"11\" font-family=\"{sans}\">{breaks} cold-cache breaks</text>\n\
         \x20   <rect x=\"248\" y=\"84\" width=\"206\" height=\"88\" rx=\"10\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20   <text x=\"264\" y=\"108\" fill=\"{muted}\" font-size=\"11\" font-family=\"{sans}\">UNCACHED INPUT SPEND</text>\n\
         \x20   <text x=\"264\" y=\"138\" fill=\"{warn}\" font-size=\"24\" font-weight=\"700\" font-family=\"{mono}\">{uncached_str}</text>\n\
         \x20   <text x=\"264\" y=\"158\" fill=\"{ok}\" font-size=\"11\" font-family=\"{sans}\">{saved_sub}</text>\n\
         \x20   <rect x=\"468\" y=\"84\" width=\"206\" height=\"88\" rx=\"10\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20   <text x=\"484\" y=\"108\" fill=\"{muted}\" font-size=\"11\" font-family=\"{sans}\">FIRST-EDIT CONTEXT P50</text>\n\
         \x20   <text x=\"484\" y=\"138\" fill=\"{fg}\" font-size=\"24\" font-weight=\"700\" font-family=\"{mono}\">{fe_str}</text>\n\
         \x20   <text x=\"484\" y=\"158\" fill=\"{muted}\" font-size=\"11\" font-family=\"{sans}\">{fe_sub}</text>\n\
         \x20   <rect x=\"688\" y=\"84\" width=\"204\" height=\"88\" rx=\"10\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20   <text x=\"704\" y=\"108\" fill=\"{muted}\" font-size=\"11\" font-family=\"{sans}\">BLANKET-READ TURNS</text>\n\
         \x20   <text x=\"704\" y=\"138\" fill=\"{err}\" font-size=\"24\" font-weight=\"700\" font-family=\"{mono}\">{blanket_str}</text>\n\
         \x20   <text x=\"704\" y=\"158\" fill=\"{muted}\" font-size=\"11\" font-family=\"{sans}\">{blanket_sub}</text>\n\
         \x20 </g>\n\
         \x20 <rect x=\"28\" y=\"190\" width=\"426\" height=\"214\" rx=\"10\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20 <text x=\"44\" y=\"216\" fill=\"{fg}\" font-size=\"13\" font-weight=\"700\" font-family=\"{sans}\">EMPIRICAL MODEL CACHE TTL CLIFFS</text>\n\
         {ttl_rows}\
         \x20 <rect x=\"468\" y=\"190\" width=\"424\" height=\"214\" rx=\"10\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20 <text x=\"486\" y=\"216\" fill=\"{fg}\" font-size=\"13\" font-weight=\"700\" font-family=\"{sans}\">POST-COMPACT CONTEXT MIX &amp; OBSERVED COHORTS</text>\n\
         {ctx_bar}\
         {ctx_legend}\
         \x20 <line x1=\"486\" y1=\"342\" x2=\"874\" y2=\"342\" stroke=\"{border}\" stroke-width=\"1\"/>\n\
         \x20 <text x=\"486\" y=\"365\" fill=\"{muted}\" font-size=\"11\" font-family=\"{mono}\">{c1}</text>\n\
         \x20 <text x=\"486\" y=\"386\" fill=\"{accent}\" font-size=\"11\" font-family=\"{mono}\">{c2}</text>\n\
         \x20 <rect x=\"28\" y=\"420\" width=\"864\" height=\"96\" rx=\"10\" fill=\"{card_bg}\" stroke=\"{border}\"/>\n\
         \x20 <text x=\"44\" y=\"438\" fill=\"{muted}\" font-size=\"10.5\" font-weight=\"700\" font-family=\"{sans}\">TOP LOCAL FINDINGS</text>\n\
         {findings_svg}\
         \x20 <text x=\"28\" y=\"542\" fill=\"{muted}\" font-size=\"11\" font-family=\"{mono}\">100% local transcript audit · zero cloud upload · github.com/BobbyCats/agy-hyper</text>\n\
         </svg>\n",
        svg_w = AUDIT_SVG_W,
        svg_h = AUDIT_SVG_H,
        sans = SANS_FONT_STACK,
        mono = MONO_FONT_STACK,
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

fn pick_installed_family(db: &usvg::fontdb::Database, preferred: &[&str]) -> Option<String> {
    for candidate in preferred {
        if db.faces().any(|f| {
            f.families
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(candidate))
        }) {
            return Some((*candidate).to_string());
        }
    }
    db.faces()
        .next()
        .and_then(|f| f.families.first().map(|(name, _)| name.clone()))
}

/// Rasterize any single-source audit SVG into a framed Retina PNG in pure Rust
/// with zero external browser/CLI dependencies.
pub(crate) fn render_svg_to_png_sized(
    svg_xml: &str,
    dark: bool,
    scale: f32,
    svg_w: u32,
    svg_h: u32,
    pad: u32,
) -> Result<Vec<u8>> {
    let scale = scale.clamp(1.0, 4.0);
    let mut fontdb = usvg::fontdb::Database::new();
    fontdb.load_system_fonts();
    if fontdb.is_empty() {
        return Err(anyhow!(
            "no system fonts available for local PNG rasterization (use --svg or `agy-hyper ui` to export via browser)"
        ));
    }

    let sans = pick_installed_family(
        &fontdb,
        &[
            "Helvetica Neue",
            "Segoe UI",
            "Inter",
            "DejaVu Sans",
            "Liberation Sans",
            "Noto Sans",
            "Arial",
        ],
    );
    let mono = pick_installed_family(
        &fontdb,
        &[
            "Menlo",
            "Consolas",
            "JetBrains Mono",
            "Cascadia Mono",
            "DejaVu Sans Mono",
            "Liberation Mono",
            "Courier New",
        ],
    );
    if let Some(ref s) = sans {
        fontdb.set_sans_serif_family(s);
    }
    if let Some(ref m) = mono {
        fontdb.set_monospace_family(m);
    }

    let mut opt = usvg::Options::default();
    if let Some(s) = sans {
        opt.font_family = s;
    }
    opt.fontdb = std::sync::Arc::new(fontdb);

    let tree = usvg::Tree::from_str(svg_xml, &opt)
        .map_err(|e| anyhow!("failed to parse audit SVG for PNG export: {}", e))?;

    let pad_px = (pad as f32) * scale;
    let out_w = ((svg_w + 2 * pad) as f32 * scale).round() as u32;
    let out_h = ((svg_h + 2 * pad) as f32 * scale).round() as u32;
    let mut pixmap = tiny_skia::Pixmap::new(out_w, out_h)
        .ok_or_else(|| anyhow!("failed to allocate {}x{} PNG canvas", out_w, out_h))?;

    let outer_bg = if dark {
        tiny_skia::Color::from_rgba8(6, 9, 17, 255)
    } else {
        tiny_skia::Color::from_rgba8(226, 232, 240, 255)
    };
    pixmap.fill(outer_bg);

    let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(pad_px, pad_px);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    pixmap
        .encode_png()
        .map_err(|e| anyhow!("failed to encode audit PNG: {}", e))
}

/// Rasterize the single-source audit SVG into a framed 2x Retina PNG (1920×1200 at scale=2.0)
/// in pure Rust with zero external browser/CLI dependencies.
pub fn render_svg_to_png(svg_xml: &str, dark: bool, scale: f32) -> Result<Vec<u8>> {
    render_svg_to_png_sized(svg_xml, dark, scale, AUDIT_SVG_W, AUDIT_SVG_H, AUDIT_PNG_PAD)
}

/// Render the shareable 2x Retina (1920×1200) local burn audit PNG directly from `BurnAuditReport`.
pub fn render_audit_png(report: &BurnAuditReport, dark: bool) -> Result<Vec<u8>> {
    let svg = render_audit_svg(report, dark);
    render_svg_to_png(&svg, dark, 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::{ContextBreakdown, SessionSplitStats, TurnWaterfall};
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
            split_stats: SessionSplitStats::default(),
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
        assert!(svg.contains("width=\"164\""), "TTL bar track must leave clearance before right label");
        assert!(TTL_BAR_X + TTL_BAR_MAX_W <= 352.0);
        assert!(svg.ends_with("</svg>\n"));

        // Pure-Rust 2x Retina PNG export verification (1920x1200)
        let png = render_audit_png(&report, true).expect("render_audit_png should succeed");
        assert!(png.len() > 12_000, "PNG should contain real rendered card pixels, got {}B", png.len());
        assert_eq!(&png[0..8], b"\x89PNG\r\n\x1a\n", "valid PNG signature");
        let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
        let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
        assert_eq!((w, h), (1920, 1200));
    }

    #[test]
    fn legacy_burn_audit_json_and_zero_transition_reports_deserialize_and_render_cleanly() {
        let legacy = serde_json::json!({
            "version": "0.4.3",
            "window_days": 7,
            "usage_window": "7d",
            "sessions_analyzed": 1,
            "coding_sessions": 0,
            "total_turns": 3,
            "currency": "USD",
            "fx_rate": 1.0,
            "total_tokens": 50000,
            "overall_cache_hit_ratio": 0.91,
            "total_api_equiv_usd": 0.25,
            "total_api_equiv": 0.25,
            "cache_saved_usd": 0.80,
            "cache_saved": 0.80,
            "uncached_input_usd": 0.05,
            "uncached_input_cost": 0.05,
            "cache_break_turns": 0,
            "first_edit_p50_tokens": null,
            "first_edit_p90_tokens": null,
            "first_edit_max_tokens": null,
            "blanket_read_turns": 0,
            "sessions_with_blanket_reads": 0,
            "context_categories": [],
            "model_ttl_cliffs": [],
            "comparison": {
                "native_only": {
                    "sessions": 0, "coding_sessions": 0, "total_turns": 0,
                    "turn_p50_ms": 0, "turn_p90_ms": 0, "tool_p50_ms": 0,
                    "cache_hit_ratio": 0.0, "first_edit_p50_tokens": null, "blanket_read_turns": 0
                },
                "with_hyper": {
                    "sessions": 1, "coding_sessions": 0, "total_turns": 3,
                    "turn_p50_ms": 2000, "turn_p90_ms": 3000, "tool_p50_ms": 500,
                    "cache_hit_ratio": 0.91, "first_edit_p50_tokens": null, "blanket_read_turns": 0
                },
                "hyper_turn_count": 3, "hyper_turn_tool_p50_ms": 500,
                "native_turn_count": 0, "native_turn_tool_p50_ms": 0
            },
            "findings": []
        });
        let report: BurnAuditReport = serde_json::from_value(legacy).unwrap();
        assert_eq!(report.verdict, "HEALTHY");
        assert_eq!(report.split_cache.total_transitions, 0);
        assert_eq!(report.split_cache.headline_cache_drop_pct, 0.0);
        assert!(report.split_cache.top_transitions.is_empty());

        let svg = render_share_card_svg(&report, false);
        assert!(svg.contains("HEALTHY"));
        assert!(svg.contains("No model switches or subagent splits detected"));
        assert_share_card_clean(&svg).unwrap();
    }
}
