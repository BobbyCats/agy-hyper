use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 8_500;
pub const DEFAULT_YELLOW_TOKENS: u64 = 180_000;
pub const DEFAULT_RED_TOKENS: u64 = 280_000;
pub const DEFAULT_YELLOW_STEPS: u64 = 160;
pub const DEFAULT_RED_STEPS: u64 = 300;
pub const DEFAULT_CACHE_WARM_SECS: u64 = 240;
pub const DEFAULT_MULTIMODAL_CLIFF_TOKENS: u64 = 180_000;

pub const SUPPORTED_CURRENCIES: &[&str] = &["USD", "CNY", "JPY", "EUR", "GBP", "HKD", "SGD", "KRW"];

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPriceRate {
    pub input_per_m: f64,
    pub cache_read_per_m: f64,
    pub output_per_m: f64,
}

pub fn default_currency() -> String {
    "USD".to_string()
}

pub fn default_fx_source() -> String {
    "offline".to_string()
}

pub fn default_fx_rates() -> BTreeMap<String, f64> {
    BTreeMap::from([
        ("CNY".to_string(), 7.25),
        ("EUR".to_string(), 0.92),
        ("GBP".to_string(), 0.79),
        ("HKD".to_string(), 7.78),
        ("JPY".to_string(), 154.0),
        ("KRW".to_string(), 1390.0),
        ("SGD".to_string(), 1.34),
        ("USD".to_string(), 1.0),
    ])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Safe,
    Balanced,
    Hyper,
}

impl Default for Profile {
    fn default() -> Self {
        Self::Balanced
    }
}

impl Profile {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Balanced => "balanced",
            Self::Hyper => "hyper",
        }
    }

    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "safe" => Some(Self::Safe),
            "balanced" | "default" => Some(Self::Balanced),
            "hyper" | "turbo" | "fast" => Some(Self::Hyper),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HyperConfig {
    pub profile: Profile,
    pub allow_outside_workspace: bool,
    pub block_secret_files: bool,
    pub max_output_bytes: usize,
    pub watermark_yellow_tokens: u64,
    pub watermark_red_tokens: u64,
    pub watermark_yellow_steps: u64,
    pub watermark_red_steps: u64,
    pub cache_warm_window_secs: u64,
    pub multimodal_cliff_tokens: u64,
    pub prewarm_tier2: bool,
    pub allow_custom_argv_checks: bool,
    pub max_wal_entries: usize,
    pub enable_ui_pill: bool,
    #[serde(default = "default_currency")]
    pub display_currency: String,
    #[serde(default = "default_fx_rates")]
    pub fx_rates: BTreeMap<String, f64>,
    #[serde(default)]
    pub fx_updated_at_ms: Option<u64>,
    #[serde(default = "default_fx_source")]
    pub fx_source: String,
    #[serde(default)]
    pub pricing_overrides: BTreeMap<String, ModelPriceRate>,
}

impl Default for HyperConfig {
    fn default() -> Self {
        Self {
            profile: Profile::Balanced,
            allow_outside_workspace: false,
            block_secret_files: true,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            watermark_yellow_tokens: DEFAULT_YELLOW_TOKENS,
            watermark_red_tokens: DEFAULT_RED_TOKENS,
            watermark_yellow_steps: DEFAULT_YELLOW_STEPS,
            watermark_red_steps: DEFAULT_RED_STEPS,
            cache_warm_window_secs: DEFAULT_CACHE_WARM_SECS,
            multimodal_cliff_tokens: DEFAULT_MULTIMODAL_CLIFF_TOKENS,
            prewarm_tier2: false,
            allow_custom_argv_checks: true,
            max_wal_entries: 100,
            enable_ui_pill: true,
            display_currency: default_currency(),
            fx_rates: default_fx_rates(),
            fx_updated_at_ms: None,
            fx_source: default_fx_source(),
            pricing_overrides: BTreeMap::new(),
        }
    }
}

impl HyperConfig {
    pub fn apply_profile(&mut self, profile: Profile) {
        self.profile = profile;
        self.allow_outside_workspace = false;
        self.block_secret_files = true;
        self.prewarm_tier2 = matches!(profile, Profile::Hyper);
        self.allow_custom_argv_checks = !matches!(profile, Profile::Safe);
    }

    pub fn set_toggle(&mut self, key: &str, value: bool) -> Result<()> {
        match key {
            "prewarm_tier2" => self.prewarm_tier2 = value,
            "block_secret_files" => self.block_secret_files = value,
            "allow_custom_argv_checks" => self.allow_custom_argv_checks = value,
            "enable_ui_pill" => self.enable_ui_pill = value,
            _ => anyhow::bail!("unknown config toggle '{}'", key),
        }
        Ok(())
    }

    pub fn effective_fx_rates(&self) -> BTreeMap<String, f64> {
        let mut merged = default_fx_rates();
        for (code, rate) in &self.fx_rates {
            let norm = code.trim().to_ascii_uppercase();
            if SUPPORTED_CURRENCIES.contains(&norm.as_str())
                && rate.is_finite()
                && *rate > 0.0
                && *rate < 1_000_000.0
            {
                merged.insert(norm, *rate);
            }
        }
        merged.insert("USD".to_string(), 1.0);
        merged
    }

    pub fn fx_rate_for(&self, currency: &str) -> (String, f64) {
        let norm = currency.trim().to_ascii_uppercase();
        let rates = self.effective_fx_rates();
        if let Some(&r) = rates.get(&norm) {
            (norm, r)
        } else {
            ("USD".to_string(), 1.0)
        }
    }

    pub fn set_currency(&mut self, currency: &str) -> Result<String> {
        let norm = currency.trim().to_ascii_uppercase();
        if !SUPPORTED_CURRENCIES.contains(&norm.as_str()) {
            anyhow::bail!(
                "unsupported currency '{}' (supported: {})",
                currency,
                SUPPORTED_CURRENCIES.join(", ")
            );
        }
        self.display_currency = norm.clone();
        Ok(norm)
    }

    pub fn update_fx_rates(
        &mut self,
        incoming: BTreeMap<String, f64>,
        source: &str,
        updated_at_ms: u64,
    ) -> Result<()> {
        let mut merged = self.effective_fx_rates();
        let mut applied = 0usize;
        for (code, rate) in incoming {
            let norm = code.trim().to_ascii_uppercase();
            if norm == "USD" {
                continue;
            }
            if SUPPORTED_CURRENCIES.contains(&norm.as_str())
                && rate.is_finite()
                && rate > 0.0
                && rate < 1_000_000.0
            {
                merged.insert(norm, (rate * 10_000.0).round() / 10_000.0);
                applied += 1;
            }
        }
        if applied == 0 {
            anyhow::bail!("no valid currency rates provided");
        }
        merged.insert("USD".to_string(), 1.0);
        self.fx_rates = merged;
        self.fx_source = match source.trim().to_ascii_lowercase().as_str() {
            "live" => "live".to_string(),
            "custom" => "custom".to_string(),
            _ => "offline".to_string(),
        };
        self.fx_updated_at_ms = Some(updated_at_ms);
        Ok(())
    }

    pub fn data_dir() -> PathBuf {
        if let Ok(custom) = std::env::var("AGY_HYPER_DATA_DIR") {
            if !custom.trim().is_empty() {
                return PathBuf::from(custom);
            }
        }
        // Always use one canonical user-global state directory so Antigravity hooks,
        // MCP server, Sidecar UI, and terminal CLI share the exact same daemon & WAL
        // (avoids split-brain when Antigravity sets PLUGIN_DATA only for host processes).
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".gemini")
            .join("antigravity")
            .join("agy-hyper")
    }

    pub fn run_dir() -> PathBuf {
        Self::data_dir().join("run")
    }

    pub fn wal_dir() -> PathBuf {
        Self::data_dir().join("wal")
    }

    pub fn config_path() -> PathBuf {
        Self::data_dir().join("config.json")
    }

    pub fn ledger_path() -> PathBuf {
        Self::data_dir().join("usage_ledger.json")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::config_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create config dir {}", parent.display()))?;
        }
        let body = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        fs::write(&tmp, body.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Validate a single path-safe identifier (conversation_id, tx_id) against traversal chars.
pub fn sanitize_id(raw: &str) -> Option<&str> {
    let s = raw.trim();
    if s.is_empty()
        || s.contains('/')
        || s.contains('\\')
        || s.contains("..")
        || s.contains('\0')
    {
        None
    } else {
        Some(s)
    }
}

/// Truncate a UTF-8 string cleanly on a char boundary (never panicking on multi-byte UTF-8).
pub fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> (&str, bool) {
    if s.len() <= max_bytes {
        return (s, false);
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_truncation_never_splits_multibyte_char() {
        let text = "你好世界hello";
        let (cut, truncated) = truncate_utf8_bytes(text, 7);
        assert!(truncated);
        assert_eq!(cut, "你好");
        assert!(cut.len() <= 7);
    }

    #[test]
    fn profile_safe_disables_custom_argv_and_tier2() {
        let mut cfg = HyperConfig::default();
        cfg.apply_profile(Profile::Safe);
        assert!(!cfg.allow_custom_argv_checks);
        assert!(!cfg.prewarm_tier2);
        assert!(!cfg.allow_outside_workspace);
        assert!(cfg.block_secret_files);
    }
}
