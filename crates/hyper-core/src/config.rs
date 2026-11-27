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

pub fn default_ui_language() -> String {
    "auto".to_string()
}

pub fn default_radar_glyph_style() -> String {
    "waveform".to_string()
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

pub fn default_true() -> bool {
    true
}

pub fn default_namer_model() -> String {
    "gemini-3.8-flash-low".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionNamingConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_namer_model")]
    pub model: String,
}

impl Default for SessionNamingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: default_namer_model(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Observe,
    Safe,
    #[default]
    Balanced,
    Hyper,
}

impl Profile {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Safe => "safe",
            Self::Balanced => "balanced",
            Self::Hyper => "hyper",
        }
    }

    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "observe" | "cockpit" | "readonly" | "read-only" => Some(Self::Observe),
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
    #[serde(default = "default_true")]
    pub enable_mcp_read: bool,
    #[serde(default = "default_true")]
    pub enable_mcp_edit: bool,
    #[serde(default = "default_true")]
    pub enable_mcp_check: bool,
    pub max_wal_entries: usize,
    pub enable_ui_pill: bool,
    #[serde(default = "default_true")]
    pub enable_watchdog_notify: bool,
    #[serde(default = "default_true")]
    pub enable_radar_sound: bool,
    #[serde(default = "default_true")]
    pub enable_update_check: bool,
    #[serde(default = "default_currency")]
    pub display_currency: String,
    #[serde(default = "default_ui_language")]
    pub ui_language: String,
    #[serde(default = "default_radar_glyph_style")]
    pub radar_glyph_style: String,
    #[serde(default = "default_fx_rates")]
    pub fx_rates: BTreeMap<String, f64>,
    #[serde(default)]
    pub fx_updated_at_ms: Option<u64>,
    #[serde(default = "default_fx_source")]
    pub fx_source: String,
    #[serde(default)]
    pub pricing_overrides: BTreeMap<String, ModelPriceRate>,
    #[serde(default)]
    pub session_naming: SessionNamingConfig,
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
            enable_mcp_read: true,
            enable_mcp_edit: true,
            enable_mcp_check: true,
            max_wal_entries: 100,
            enable_ui_pill: true,
            enable_watchdog_notify: true,
            enable_radar_sound: true,
            enable_update_check: true,
            display_currency: default_currency(),
            ui_language: default_ui_language(),
            radar_glyph_style: default_radar_glyph_style(),
            fx_rates: default_fx_rates(),
            fx_updated_at_ms: None,
            fx_source: default_fx_source(),
            pricing_overrides: BTreeMap::new(),
            session_naming: SessionNamingConfig::default(),
        }
    }
}

impl HyperConfig {
    pub fn apply_profile(&mut self, profile: Profile) {
        self.profile = profile;
        self.allow_outside_workspace = false;
        self.block_secret_files = true;
        match profile {
            Profile::Observe => {
                self.enable_mcp_read = false;
                self.enable_mcp_edit = false;
                self.enable_mcp_check = false;
                self.allow_custom_argv_checks = false;
                self.prewarm_tier2 = false;
                self.session_naming.enabled = false;
            }
            Profile::Safe => {
                self.enable_mcp_read = true;
                self.enable_mcp_edit = false;
                self.enable_mcp_check = true;
                self.allow_custom_argv_checks = false;
                self.prewarm_tier2 = false;
                self.session_naming.enabled = true;
            }
            Profile::Balanced => {
                self.enable_mcp_read = true;
                self.enable_mcp_edit = true;
                self.enable_mcp_check = true;
                self.allow_custom_argv_checks = true;
                self.prewarm_tier2 = false;
                self.session_naming.enabled = true;
            }
            Profile::Hyper => {
                self.enable_mcp_read = true;
                self.enable_mcp_edit = true;
                self.enable_mcp_check = true;
                self.allow_custom_argv_checks = true;
                self.prewarm_tier2 = true;
                self.session_naming.enabled = true;
            }
        }
    }

    pub fn any_mcp_accel_enabled(&self) -> bool {
        self.enable_mcp_read || self.enable_mcp_edit || self.enable_mcp_check
    }

    pub fn is_mcp_tool_enabled(&self, tool_name: &str) -> bool {
        match tool_name {
            "hyper_pack" | "hyper_search" => self.enable_mcp_read,
            "hyper_edit" => self.enable_mcp_edit,
            "hyper_check" => self.enable_mcp_check,
            "hyper_tx"
            | "hyper_cockpit"
            | "hyper_app_tx_diff"
            | "hyper_app_tx_rollback"
            | "hyper_app_cockpit_data"
            | "hyper_app_set_profile" => true,
            _ => true,
        }
    }

    pub fn set_toggle(&mut self, key: &str, value: bool) -> Result<()> {
        match key {
            "prewarm_tier2" => self.prewarm_tier2 = value,
            "block_secret_files" => self.block_secret_files = value,
            "allow_custom_argv_checks" => self.allow_custom_argv_checks = value,
            "enable_ui_pill" => self.enable_ui_pill = value,
            "enable_watchdog_notify" => self.enable_watchdog_notify = value,
            "enable_radar_sound" => self.enable_radar_sound = value,
            "enable_update_check" => self.enable_update_check = value,
            "enable_mcp_read" => self.enable_mcp_read = value,
            "enable_mcp_edit" => self.enable_mcp_edit = value,
            "enable_mcp_check" => self.enable_mcp_check = value,
            "session_naming" | "enable_session_naming" => self.session_naming.enabled = value,
            _ => anyhow::bail!("unknown config toggle '{}'", key),
        }
        Ok(())
    }

    pub fn set_ui_language(&mut self, raw: &str) -> Result<()> {
        let norm = match raw.trim().to_ascii_lowercase().as_str() {
            "" | "auto" | "system" => "auto",
            "zh" | "zh-cn" | "zh_cn" | "zh-hans" => "zh-CN",
            "en" | "en-us" | "en-gb" => "en",
            "ja" | "ja-jp" => "ja",
            _ => anyhow::bail!("unsupported ui_language '{}' (expected auto|zh-CN|en|ja)", raw),
        };
        self.ui_language = norm.to_string();
        Ok(())
    }

    pub fn set_radar_glyph_style(&mut self, raw: &str) -> Result<()> {
        let norm = match raw.trim().to_ascii_lowercase().as_str() {
            "" | "waveform" | "ldrs-waveform" => "waveform",
            "bars" | "bars_fade" | "svg-spinners-bars" => "bars_fade",
            "ripples" | "pulse_rings" | "sonar" => "pulse_rings",
            _ => anyhow::bail!(
                "unsupported radar_glyph_style '{}' (expected waveform|bars_fade|pulse_rings)",
                raw
            ),
        };
        self.radar_glyph_style = norm.to_string();
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

    pub fn session_registry_path() -> PathBuf {
        Self::data_dir().join("session_registry.json")
    }

    pub fn session_titles_dir() -> PathBuf {
        Self::data_dir().join("session_titles")
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
        assert!(cfg.enable_mcp_read);
        assert!(!cfg.enable_mcp_edit);
        assert!(cfg.enable_mcp_check);
    }

    #[test]
    fn profile_observe_is_zero_intrusion_and_legacy_json_defaults_cleanly() {
        let mut cfg = HyperConfig::default();
        cfg.apply_profile(Profile::Observe);
        assert!(!cfg.any_mcp_accel_enabled());
        assert!(!cfg.is_mcp_tool_enabled("hyper_pack"));
        assert!(!cfg.is_mcp_tool_enabled("hyper_edit"));
        assert!(!cfg.is_mcp_tool_enabled("hyper_check"));
        assert!(cfg.is_mcp_tool_enabled("hyper_tx"));
        assert!(cfg.is_mcp_tool_enabled("hyper_cockpit"));
        assert!(cfg.is_mcp_tool_enabled("hyper_app_tx_diff"));
        assert!(cfg.is_mcp_tool_enabled("hyper_app_tx_rollback"));
        assert!(cfg.is_mcp_tool_enabled("hyper_app_cockpit_data"));
        assert!(cfg.is_mcp_tool_enabled("hyper_app_set_profile"));

        // Legacy v0.3 config.json without enable_mcp_* fields preserves existing enabled defaults.
        let legacy: HyperConfig = serde_json::from_str(r#"{"profile":"balanced"}"#).unwrap();
        assert!(legacy.enable_mcp_read);
        assert!(legacy.enable_mcp_edit);
        assert!(legacy.enable_mcp_check);
        assert!(legacy.enable_update_check);

        let mut toggled = legacy;
        toggled.set_toggle("enable_update_check", false).unwrap();
        assert!(!toggled.enable_update_check);
        assert!(toggled.session_naming.enabled);
        assert_eq!(toggled.session_naming.model, "gemini-3.8-flash-low");
        toggled.set_toggle("session_naming", false).unwrap();
        assert!(!toggled.session_naming.enabled);
        assert!(!cfg.session_naming.enabled);
    }
}
