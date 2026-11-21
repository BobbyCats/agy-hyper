use crate::config::{HyperConfig, ModelPriceRate};
use crate::transcript::parse_iso8601_epoch_ms;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;

pub const UNATTRIBUTED_MODEL: &str = "unattributed";
const LEDGER_VERSION: u32 = 1;
const RETENTION_MS: i64 = 180 * 86_400_000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenBucket {
    pub turns: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
}

impl TokenBucket {
    pub fn add_assign(&mut self, other: &Self) {
        self.turns = self.turns.saturating_add(other.turns);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(other.cache_read_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
    }

    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.output_tokens)
    }

    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.cache_read_tokens)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConversationLedgerEntry {
    pub mtime_ns: u128,
    pub size_bytes: u64,
    pub byte_offset: u64,
    pub first_ts_ms: i64,
    pub last_ts_ms: i64,
    pub hourly: BTreeMap<i64, BTreeMap<String, TokenBucket>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageLedger {
    #[serde(default = "default_ledger_version")]
    pub version: u32,
    #[serde(default)]
    pub conversations: BTreeMap<String, ConversationLedgerEntry>,
}

fn default_ledger_version() -> u32 {
    LEDGER_VERSION
}

impl Default for UsageLedger {
    fn default() -> Self {
        Self {
            version: LEDGER_VERSION,
            conversations: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageWindow {
    Hours24,
    Days7,
    Days30,
    Days90,
}

impl UsageWindow {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "24h" | "1d" | "day" | "today" => Self::Hours24,
            "30d" | "1m" | "month" => Self::Days30,
            "90d" | "3m" | "quarter" => Self::Days90,
            _ => Self::Days7,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Hours24 => "24h",
            Self::Days7 => "7d",
            Self::Days30 => "30d",
            Self::Days90 => "90d",
        }
    }

    fn day_count(&self) -> Option<i64> {
        match self {
            Self::Hours24 => None,
            Self::Days7 => Some(7),
            Self::Days30 => Some(30),
            Self::Days90 => Some(90),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelUsageBreakdown {
    pub model: String,
    pub attributed: bool,
    pub priced: bool,
    pub turns: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cache_hit_ratio: f64,
    pub estimated_cost_usd: f64,
    pub estimated_cost: f64,
    pub cache_saved_usd: f64,
    pub cache_saved: f64,
    #[serde(default)]
    pub rate_applied: Option<ModelPriceRate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageTimePoint {
    pub label: String,
    pub iso_key: String,
    pub turns: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub attributed_tokens: u64,
    pub estimated_cost: f64,
    pub cache_saved: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageWindowReport {
    pub window: String,
    pub currency: String,
    pub fx_rate: f64,
    pub fx_source: String,
    #[serde(default)]
    pub fx_updated_at_ms: Option<u64>,
    pub available_currencies: BTreeMap<String, f64>,
    pub total_turns: u64,
    pub active_sessions: usize,
    pub total_input_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_output_tokens: u64,
    pub total_tokens: u64,
    pub cache_hit_ratio: f64,
    pub attributed_tokens: u64,
    pub unattributed_tokens: u64,
    pub attributed_ratio: f64,
    pub estimated_cost_usd: f64,
    pub cache_saved_usd: f64,
    pub estimated_cost: f64,
    pub cache_saved: f64,
    pub ledger_sessions_tracked: usize,
    pub ledger_scan_ms: u64,
    pub series: Vec<UsageTimePoint>,
    pub models: Vec<ModelUsageBreakdown>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionUsageSummary {
    pub currency: String,
    pub fx_rate: f64,
    pub fx_source: String,
    pub session_tokens: u64,
    pub session_input_tokens: u64,
    pub session_cache_read_tokens: u64,
    pub session_output_tokens: u64,
    pub session_cost_usd: f64,
    pub session_cache_saved_usd: f64,
    pub session_cost: f64,
    pub session_cache_saved: f64,
    pub session_attributed_ratio: f64,
    pub tree_tokens: u64,
    pub tree_cost_usd: f64,
    pub tree_cache_saved_usd: f64,
    pub tree_cost: f64,
    pub tree_cache_saved: f64,
    pub tree_attributed_ratio: f64,
    pub subagent_count: usize,
    pub subagent_token_share: f64,
    pub models: Vec<ModelUsageBreakdown>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SyncOutcome {
    pub files_scanned: usize,
    pub dirty: bool,
    pub scan_ms: u64,
}

pub fn default_brain_dirs() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    ["antigravity", "antigravity-ide", "antigravity-cli"]
        .into_iter()
        .map(|p| home.join(".gemini").join(p).join("brain"))
        .filter(|p| p.is_dir())
        .collect()
}

pub fn normalize_model_key(raw: Option<&str>) -> String {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some(m) => m.to_string(),
        None => UNATTRIBUTED_MODEL.to_string(),
    }
}

pub fn resolve_model_rate(model: &str, cfg: &HyperConfig) -> Option<ModelPriceRate> {
    let lower = model.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }

    if let Some(&exact) = cfg
        .pricing_overrides
        .iter()
        .find(|(k, _)| k.trim().to_ascii_lowercase() == lower)
        .map(|(_, v)| v)
    {
        return valid_rate(exact);
    }

    if lower == UNATTRIBUTED_MODEL {
        return cfg
            .pricing_overrides
            .iter()
            .find(|(k, _)| {
                let kl = k.trim().to_ascii_lowercase();
                kl == "default" || kl == UNATTRIBUTED_MODEL
            })
            .and_then(|(_, &v)| valid_rate(v));
    }

    let mut best_prefix: Option<(usize, ModelPriceRate)> = None;
    for (k, &v) in &cfg.pricing_overrides {
        let kl = k.trim().to_ascii_lowercase();
        if kl.is_empty() || kl == "default" || kl == UNATTRIBUTED_MODEL {
            continue;
        }
        if lower.starts_with(&kl) {
            if let Some(rate) = valid_rate(v) {
                if best_prefix.is_none_or(|(len, _)| kl.len() > len) {
                    best_prefix = Some((kl.len(), rate));
                }
            }
        }
    }
    if let Some((_, rate)) = best_prefix {
        return Some(rate);
    }

    builtin_model_rate(&lower)
}

fn valid_rate(r: ModelPriceRate) -> Option<ModelPriceRate> {
    if r.input_per_m.is_finite()
        && r.cache_read_per_m.is_finite()
        && r.output_per_m.is_finite()
        && r.input_per_m >= 0.0
        && r.cache_read_per_m >= 0.0
        && r.output_per_m >= 0.0
    {
        Some(r)
    } else {
        None
    }
}

fn builtin_model_rate(lower: &str) -> Option<ModelPriceRate> {
    if lower.contains("claude-opus") {
        let legacy_opus = lower.contains("opus-3")
            || lower.contains("opus-4-0")
            || lower.contains("opus-4-1")
            || lower.contains("opus-4.0")
            || lower.contains("opus-4.1");
        return Some(if legacy_opus {
            ModelPriceRate {
                input_per_m: 15.0,
                cache_read_per_m: 1.5,
                output_per_m: 75.0,
            }
        } else {
            ModelPriceRate {
                input_per_m: 5.0,
                cache_read_per_m: 0.5,
                output_per_m: 25.0,
            }
        });
    }
    if lower.contains("claude-sonnet") {
        return Some(ModelPriceRate {
            input_per_m: 3.0,
            cache_read_per_m: 0.3,
            output_per_m: 15.0,
        });
    }
    if lower.contains("claude-haiku") {
        return Some(ModelPriceRate {
            input_per_m: 0.8,
            cache_read_per_m: 0.08,
            output_per_m: 4.0,
        });
    }
    if lower.contains("hydrogen") || lower.contains("flash-lite") || lower.contains("lite") {
        return Some(ModelPriceRate {
            input_per_m: 0.15,
            cache_read_per_m: 0.015,
            output_per_m: 0.6,
        });
    }
    if lower.contains("argon") || lower.contains("carbon") || lower.contains("flash") {
        return Some(ModelPriceRate {
            input_per_m: 0.5,
            cache_read_per_m: 0.05,
            output_per_m: 3.0,
        });
    }
    if lower.contains("neon")
        || lower.contains("gemini-3-pro")
        || lower.contains("gemini-2.5-pro")
        || lower.contains("gemini-pro")
    {
        return Some(ModelPriceRate {
            input_per_m: 2.0,
            cache_read_per_m: 0.2,
            output_per_m: 12.0,
        });
    }
    if lower.starts_with("gemini") {
        return Some(ModelPriceRate {
            input_per_m: 1.25,
            cache_read_per_m: 0.125,
            output_per_m: 10.0,
        });
    }
    if lower.starts_with("gpt-5") || lower.starts_with("o3") || lower.starts_with("o4") {
        return Some(ModelPriceRate {
            input_per_m: 1.25,
            cache_read_per_m: 0.125,
            output_per_m: 10.0,
        });
    }
    if lower.starts_with("gpt-4") {
        return Some(ModelPriceRate {
            input_per_m: 2.5,
            cache_read_per_m: 0.25,
            output_per_m: 10.0,
        });
    }
    None
}

pub fn price_bucket_usd(bucket: &TokenBucket, rate: ModelPriceRate) -> (f64, f64) {
    let cost = (bucket.input_tokens as f64 * rate.input_per_m
        + bucket.cache_read_tokens as f64 * rate.cache_read_per_m
        + bucket.output_tokens as f64 * rate.output_per_m)
        / 1_000_000.0;
    let saved = (bucket.cache_read_tokens as f64
        * (rate.input_per_m - rate.cache_read_per_m).max(0.0))
        / 1_000_000.0;
    (cost, saved)
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

fn summarize_model_map(
    map: &BTreeMap<String, TokenBucket>,
    cfg: &HyperConfig,
    fx_rate: f64,
) -> (Vec<ModelUsageBreakdown>, u64, u64, u64, f64, f64) {
    let mut breakdowns = Vec::with_capacity(map.len());
    let mut attributed_tokens = 0u64;
    let mut unattributed_tokens = 0u64;
    let mut total_tokens = 0u64;
    let mut total_cost_usd = 0.0f64;
    let mut total_saved_usd = 0.0f64;

    for (model, bucket) in map {
        let tot = bucket.total_tokens();
        let attributed = model != UNATTRIBUTED_MODEL;
        total_tokens = total_tokens.saturating_add(tot);
        if attributed {
            attributed_tokens = attributed_tokens.saturating_add(tot);
        } else {
            unattributed_tokens = unattributed_tokens.saturating_add(tot);
        }

        let rate = resolve_model_rate(model, cfg);
        let (cost_usd, saved_usd) = rate
            .map(|r| price_bucket_usd(bucket, r))
            .unwrap_or((0.0, 0.0));
        total_cost_usd += cost_usd;
        total_saved_usd += saved_usd;

        let prompt = bucket.prompt_tokens();
        let cache_hit_ratio = if prompt > 0 {
            bucket.cache_read_tokens as f64 / prompt as f64
        } else {
            0.0
        };

        breakdowns.push(ModelUsageBreakdown {
            model: model.clone(),
            attributed,
            priced: rate.is_some(),
            turns: bucket.turns,
            input_tokens: bucket.input_tokens,
            cache_read_tokens: bucket.cache_read_tokens,
            output_tokens: bucket.output_tokens,
            total_tokens: tot,
            cache_hit_ratio,
            estimated_cost_usd: round4(cost_usd),
            estimated_cost: round4(cost_usd * fx_rate),
            cache_saved_usd: round4(saved_usd),
            cache_saved: round4(saved_usd * fx_rate),
            rate_applied: rate,
        });
    }

    breakdowns.sort_by_key(|b| std::cmp::Reverse(b.total_tokens));
    (
        breakdowns,
        total_tokens,
        attributed_tokens,
        unattributed_tokens,
        total_cost_usd,
        total_saved_usd,
    )
}

pub fn build_session_usage(
    cfg: &HyperConfig,
    session_models: &BTreeMap<String, TokenBucket>,
    tree_models: &BTreeMap<String, TokenBucket>,
    subagent_count: usize,
) -> SessionUsageSummary {
    let (currency, fx_rate) = cfg.fx_rate_for(&cfg.display_currency);

    let (_, s_tot, s_attr, _, s_cost_usd, s_saved_usd) =
        summarize_model_map(session_models, cfg, fx_rate);
    let s_in: u64 = session_models.values().map(|b| b.input_tokens).sum();
    let s_cr: u64 = session_models.values().map(|b| b.cache_read_tokens).sum();
    let s_out: u64 = session_models.values().map(|b| b.output_tokens).sum();

    let (models, t_tot, t_attr, _, t_cost_usd, t_saved_usd) =
        summarize_model_map(tree_models, cfg, fx_rate);

    let subagent_tokens = t_tot.saturating_sub(s_tot);
    let subagent_token_share = if t_tot > 0 && subagent_count > 0 {
        subagent_tokens as f64 / t_tot as f64
    } else {
        0.0
    };

    SessionUsageSummary {
        currency,
        fx_rate,
        fx_source: cfg.fx_source.clone(),
        session_tokens: s_tot,
        session_input_tokens: s_in,
        session_cache_read_tokens: s_cr,
        session_output_tokens: s_out,
        session_cost_usd: round4(s_cost_usd),
        session_cache_saved_usd: round4(s_saved_usd),
        session_cost: round4(s_cost_usd * fx_rate),
        session_cache_saved: round4(s_saved_usd * fx_rate),
        session_attributed_ratio: if s_tot > 0 {
            s_attr as f64 / s_tot as f64
        } else {
            1.0
        },
        tree_tokens: t_tot,
        tree_cost_usd: round4(t_cost_usd),
        tree_cache_saved_usd: round4(t_saved_usd),
        tree_cost: round4(t_cost_usd * fx_rate),
        tree_cache_saved: round4(t_saved_usd * fx_rate),
        tree_attributed_ratio: if t_tot > 0 {
            t_attr as f64 / t_tot as f64
        } else {
            1.0
        },
        subagent_count,
        subagent_token_share,
        models,
    }
}

impl UsageLedger {
    pub fn load_from(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str::<Self>(&raw)
                .ok()
                .filter(|l| l.version == LEDGER_VERSION)
                .unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create ledger dir {}", parent.display()))?;
        }
        let bytes = serde_json::to_vec(self)?;
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        fs::write(&tmp, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn sync_from_dirs(&mut self, brain_dirs: &[PathBuf], now_ms: i64) -> SyncOutcome {
        let t0 = Instant::now();
        let mut files_scanned = 0usize;
        let mut dirty = false;
        let mut seen_on_disk = HashSet::new();

        for brain in brain_dirs {
            let Ok(entries) = fs::read_dir(brain) else {
                continue;
            };
            for ent in entries.flatten() {
                let conv_id = ent.file_name().to_string_lossy().to_string();
                let transcript = ent
                    .path()
                    .join(".system_generated")
                    .join("logs")
                    .join("transcript.jsonl");
                let Ok(meta) = fs::metadata(&transcript) else {
                    continue;
                };
                let size_bytes = meta.len();
                if size_bytes < 64 {
                    continue;
                }
                let mtime_ns = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                seen_on_disk.insert(conv_id.clone());

                let unchanged = self
                    .conversations
                    .get(&conv_id)
                    .is_some_and(|e| e.mtime_ns == mtime_ns && e.size_bytes == size_bytes);
                if unchanged {
                    continue;
                }

                files_scanned += 1;
                let entry = self.conversations.entry(conv_id).or_default();
                if size_bytes < entry.byte_offset {
                    *entry = ConversationLedgerEntry::default();
                }

                if ingest_transcript_incremental(&transcript, entry, mtime_ns, size_bytes).is_ok() {
                    dirty = true;
                }
            }
        }

        let cutoff = now_ms.saturating_sub(RETENTION_MS);
        let before = self.conversations.len();
        self.conversations
            .retain(|cid, entry| seen_on_disk.contains(cid) || entry.last_ts_ms >= cutoff);
        if self.conversations.len() != before {
            dirty = true;
        }

        SyncOutcome {
            files_scanned,
            dirty,
            scan_ms: t0.elapsed().as_millis() as u64,
        }
    }

    pub fn query_window(
        &self,
        window: UsageWindow,
        cfg: &HyperConfig,
        currency_override: Option<&str>,
        tz_offset_mins: i32,
        now_ms: i64,
        scan_ms: u64,
    ) -> UsageWindowReport {
        let clamped_tz = tz_offset_mins.clamp(-720, 840);
        let tz_offset_ms = (clamped_tz as i64) * 60_000;
        let requested_ccy = currency_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&cfg.display_currency);
        let (currency, fx_rate) = cfg.fx_rate_for(requested_ccy);

        let now_hour = now_ms.div_euclid(3_600_000);
        let today_local_day = (now_ms + tz_offset_ms).div_euclid(86_400_000);

        let (start_bucket, bucket_count, hourly_mode) = match window.day_count() {
            None => (now_hour - 23, 24usize, true),
            Some(days) => (today_local_day - (days - 1), days as usize, false),
        };
        let end_bucket = start_bucket + (bucket_count as i64) - 1;

        let mut bucket_models: Vec<BTreeMap<String, TokenBucket>> =
            vec![BTreeMap::new(); bucket_count];
        let mut model_totals: BTreeMap<String, TokenBucket> = BTreeMap::new();
        let mut active_sessions = 0usize;
        let mut total_turns = 0u64;
        let mut total_input = 0u64;
        let mut total_cache_read = 0u64;
        let mut total_output = 0u64;

        for entry in self.conversations.values() {
            let mut touched = false;
            for (&utc_hour, models) in &entry.hourly {
                let target_bucket = if hourly_mode {
                    utc_hour
                } else {
                    (utc_hour * 3_600_000 + tz_offset_ms).div_euclid(86_400_000)
                };
                if target_bucket < start_bucket || target_bucket > end_bucket {
                    continue;
                }
                let idx = (target_bucket - start_bucket) as usize;
                touched = true;
                for (model, tb) in models {
                    bucket_models[idx]
                        .entry(model.clone())
                        .or_default()
                        .add_assign(tb);
                    model_totals
                        .entry(model.clone())
                        .or_default()
                        .add_assign(tb);
                    total_turns = total_turns.saturating_add(tb.turns);
                    total_input = total_input.saturating_add(tb.input_tokens);
                    total_cache_read = total_cache_read.saturating_add(tb.cache_read_tokens);
                    total_output = total_output.saturating_add(tb.output_tokens);
                }
            }
            if touched {
                active_sessions += 1;
            }
        }

        let (models, total_tokens, attributed_tokens, unattributed_tokens, cost_usd, saved_usd) =
            summarize_model_map(&model_totals, cfg, fx_rate);

        let mut series = Vec::with_capacity(bucket_count);
        for (i, bmap) in bucket_models.iter().enumerate() {
            let bkey = start_bucket + (i as i64);
            let (label, iso_key) = if hourly_mode {
                format_local_hour(bkey, clamped_tz)
            } else {
                format_local_day(bkey)
            };
            let mut b_turns = 0u64;
            let mut b_in = 0u64;
            let mut b_cr = 0u64;
            let mut b_out = 0u64;
            let mut b_attr = 0u64;
            let mut b_cost_usd = 0.0f64;
            let mut b_saved_usd = 0.0f64;
            for (model, tb) in bmap {
                b_turns = b_turns.saturating_add(tb.turns);
                b_in = b_in.saturating_add(tb.input_tokens);
                b_cr = b_cr.saturating_add(tb.cache_read_tokens);
                b_out = b_out.saturating_add(tb.output_tokens);
                if model != UNATTRIBUTED_MODEL {
                    b_attr = b_attr.saturating_add(tb.total_tokens());
                }
                if let Some(rate) = resolve_model_rate(model, cfg) {
                    let (c, s) = price_bucket_usd(tb, rate);
                    b_cost_usd += c;
                    b_saved_usd += s;
                }
            }
            series.push(UsageTimePoint {
                label,
                iso_key,
                turns: b_turns,
                input_tokens: b_in,
                cache_read_tokens: b_cr,
                output_tokens: b_out,
                total_tokens: b_in.saturating_add(b_cr).saturating_add(b_out),
                attributed_tokens: b_attr,
                estimated_cost: round4(b_cost_usd * fx_rate),
                cache_saved: round4(b_saved_usd * fx_rate),
            });
        }

        let prompt_denom = total_input.saturating_add(total_cache_read);
        let cache_hit_ratio = if prompt_denom > 0 {
            total_cache_read as f64 / prompt_denom as f64
        } else {
            0.0
        };
        let attributed_ratio = if total_tokens > 0 {
            attributed_tokens as f64 / total_tokens as f64
        } else {
            1.0
        };

        UsageWindowReport {
            window: window.as_str().to_string(),
            currency,
            fx_rate,
            fx_source: cfg.fx_source.clone(),
            fx_updated_at_ms: cfg.fx_updated_at_ms,
            available_currencies: cfg.effective_fx_rates(),
            total_turns,
            active_sessions,
            total_input_tokens: total_input,
            total_cache_read_tokens: total_cache_read,
            total_output_tokens: total_output,
            total_tokens,
            cache_hit_ratio,
            attributed_tokens,
            unattributed_tokens,
            attributed_ratio,
            estimated_cost_usd: round4(cost_usd),
            cache_saved_usd: round4(saved_usd),
            estimated_cost: round4(cost_usd * fx_rate),
            cache_saved: round4(saved_usd * fx_rate),
            ledger_sessions_tracked: self.conversations.len(),
            ledger_scan_ms: scan_ms,
            series,
            models,
        }
    }
}

fn ingest_transcript_incremental(
    path: &Path,
    entry: &mut ConversationLedgerEntry,
    mtime_ns: u128,
    size_bytes: u64,
) -> Result<()> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(entry.byte_offset))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut offset = entry.byte_offset;

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        if !line.ends_with('\n') {
            break;
        }
        offset = offset.saturating_add(n as u64);

        if !line.contains("\"PLANNER_RESPONSE\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("PLANNER_RESPONSE") {
            continue;
        }
        let Some(ts_ms) = v
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_epoch_ms)
        else {
            continue;
        };

        let input_tokens = v.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        let cache_read_tokens = v
            .get("cache_read_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output_tokens = v.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
        let model = normalize_model_key(
            v.get("model")
                .or_else(|| v.get("model_name"))
                .and_then(Value::as_str),
        );

        let hour_bucket = ts_ms.div_euclid(3_600_000);
        let bucket = entry
            .hourly
            .entry(hour_bucket)
            .or_default()
            .entry(model)
            .or_default();
        bucket.turns = bucket.turns.saturating_add(1);
        bucket.input_tokens = bucket.input_tokens.saturating_add(input_tokens);
        bucket.cache_read_tokens = bucket.cache_read_tokens.saturating_add(cache_read_tokens);
        bucket.output_tokens = bucket.output_tokens.saturating_add(output_tokens);

        if entry.first_ts_ms == 0 || ts_ms < entry.first_ts_ms {
            entry.first_ts_ms = ts_ms;
        }
        if ts_ms > entry.last_ts_ms {
            entry.last_ts_ms = ts_ms;
        }
    }

    entry.byte_offset = offset;
    entry.mtime_ns = mtime_ns;
    entry.size_bytes = size_bytes;
    Ok(())
}

fn format_local_hour(utc_hour: i64, tz_offset_mins: i32) -> (String, String) {
    let local_sec = utc_hour * 3600 + (tz_offset_mins as i64) * 60;
    match OffsetDateTime::from_unix_timestamp(local_sec) {
        Ok(dt) => (
            format!("{:02}:00", dt.hour()),
            format!(
                "{:04}-{:02}-{:02}T{:02}",
                dt.year(),
                dt.month() as u8,
                dt.day(),
                dt.hour()
            ),
        ),
        Err(_) => (utc_hour.to_string(), utc_hour.to_string()),
    }
}

fn format_local_day(local_day: i64) -> (String, String) {
    let sec = local_day * 86_400;
    match OffsetDateTime::from_unix_timestamp(sec) {
        Ok(dt) => (
            format!("{:02}/{:02}", dt.month() as u8, dt.day()),
            format!("{:04}-{:02}-{:02}", dt.year(), dt.month() as u8, dt.day()),
        ),
        Err(_) => (local_day.to_string(), local_day.to_string()),
    }
}

pub fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn incremental_ledger_cursor_and_window_aggregation() {
        let dir = tempdir().unwrap();
        let brain = dir.path().join("brain");
        let log_dir = brain
            .join("conv-alpha")
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&log_dir).unwrap();
        let transcript = log_dir.join("transcript.jsonl");

        let line1 = r#"{"type":"PLANNER_RESPONSE","created_at":"2026-11-20T02:15:00Z","model":"gemini-4-neon-xhigh","input_tokens":100000,"cache_read_tokens":900000,"output_tokens":5000}"#;
        let line2 = r#"{"type":"PLANNER_RESPONSE","created_at":"2026-11-15T05:00:00Z","input_tokens":200000,"cache_read_tokens":300000,"output_tokens":2000}"#;
        fs::write(&transcript, format!("{}\n{}\n", line1, line2)).unwrap();

        let now_ms = parse_iso8601_epoch_ms("2026-11-20T04:00:00Z").unwrap();
        let mut ledger = UsageLedger::default();
        let s1 = ledger.sync_from_dirs(std::slice::from_ref(&brain), now_ms);
        assert_eq!(s1.files_scanned, 1);
        assert!(s1.dirty);

        let s2 = ledger.sync_from_dirs(std::slice::from_ref(&brain), now_ms);
        assert_eq!(s2.files_scanned, 0);
        assert!(!s2.dirty);

        let mut f = fs::OpenOptions::new().append(true).open(&transcript).unwrap();
        writeln!(
            f,
            r#"{{"type":"PLANNER_RESPONSE","created_at":"2026-11-20T03:30:00Z","model":"claude-opus-5-5-medium","input_tokens":50000,"cache_read_tokens":450000,"output_tokens":10000}}"#
        )
        .unwrap();
        drop(f);

        let s3 = ledger.sync_from_dirs(std::slice::from_ref(&brain), now_ms);
        assert_eq!(s3.files_scanned, 1);
        assert!(s3.dirty);

        let mut cfg = HyperConfig::default();
        cfg.set_currency("CNY").unwrap();

        let rep_24h = ledger.query_window(UsageWindow::Hours24, &cfg, Some("CNY"), 480, now_ms, 0);
        assert_eq!(rep_24h.series.len(), 24);
        assert_eq!(rep_24h.total_turns, 2);
        assert_eq!(rep_24h.unattributed_tokens, 0);
        assert!((rep_24h.attributed_ratio - 1.0).abs() < 1e-6);
        assert!(rep_24h.estimated_cost_usd > 0.0);
        assert!(rep_24h.cache_saved_usd > rep_24h.estimated_cost_usd);
        assert!((rep_24h.estimated_cost - rep_24h.estimated_cost_usd * 7.25).abs() < 0.01);

        let rep_7d = ledger.query_window(UsageWindow::Days7, &cfg, Some("USD"), 480, now_ms, 0);
        assert_eq!(rep_7d.series.len(), 7);
        assert_eq!(rep_7d.total_turns, 3);
        assert_eq!(rep_7d.unattributed_tokens, 502_000);
        assert!((rep_7d.estimated_cost_usd - rep_24h.estimated_cost_usd).abs() < 1e-4);
        assert!(rep_7d.attributed_ratio < 1.0 && rep_7d.attributed_ratio > 0.7);
        let unattr = rep_7d
            .models
            .iter()
            .find(|m| m.model == UNATTRIBUTED_MODEL)
            .unwrap();
        assert!(!unattr.attributed);
        assert!(!unattr.priced);
        assert_eq!(unattr.estimated_cost_usd, 0.0);

        fs::remove_file(&transcript).unwrap();
        ledger.sync_from_dirs(std::slice::from_ref(&brain), now_ms);
        let rep_after = ledger.query_window(UsageWindow::Days7, &cfg, Some("USD"), 480, now_ms, 0);
        assert_eq!(rep_after.total_turns, 3);
    }

    #[test]
    fn pricing_overrides_and_fx_rate_validation() {
        let mut cfg = HyperConfig::default();
        cfg.pricing_overrides.insert(
            "gemini-4-neon".to_string(),
            ModelPriceRate {
                input_per_m: 10.0,
                cache_read_per_m: 1.0,
                output_per_m: 30.0,
            },
        );
        let rate = resolve_model_rate("gemini-4-neon-xhigh", &cfg).unwrap();
        assert_eq!(rate.input_per_m, 10.0);

        let mut incoming = BTreeMap::new();
        incoming.insert("CNY".to_string(), 7.12345);
        incoming.insert("BAD".to_string(), 99.0);
        incoming.insert("EUR".to_string(), -1.0);
        cfg.update_fx_rates(incoming, "live", 1_700_000_000_000)
            .unwrap();
        assert_eq!(cfg.fx_source, "live");
        assert_eq!(cfg.fx_rate_for("CNY").1, 7.1235);
        assert_eq!(cfg.fx_rate_for("EUR").1, 0.92);
        assert_eq!(cfg.fx_rate_for("BAD").0, "USD");
    }
}
