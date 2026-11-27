use crate::config::HyperConfig;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_GITHUB_REPO: &str = "BobbyCats/agy-hyper";
pub const SESSION_START_UPDATE_TTL_MS: u64 = 12 * 3600 * 1000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateState {
    pub checked_at_ms: u64,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub release_url: Option<String>,
    pub published_at: Option<String>,
    pub dismissed_version: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateOutcome {
    pub previous_version: String,
    pub installed_version: String,
    pub plugin_dir: String,
    pub release_url: String,
}

impl UpdateState {
    pub fn sync_current_version(&mut self, current_version: &str) {
        let norm_cur = normalize_semver_str(current_version);
        self.current_version = norm_cur.clone();
        self.update_available = self
            .latest_version
            .as_deref()
            .is_some_and(|latest| is_newer_version(&norm_cur, latest));
    }

    pub fn is_actionable(&self, current_version: &str) -> bool {
        if !self.update_available {
            return false;
        }
        let Some(latest) = self.latest_version.as_deref() else {
            return false;
        };
        if !is_newer_version(current_version, latest) {
            return false;
        }
        match self.dismissed_version.as_deref() {
            Some(dismissed) if !dismissed.trim().is_empty() => {
                normalize_semver_str(dismissed) != normalize_semver_str(latest)
            }
            _ => true,
        }
    }
}

pub fn normalize_semver_str(raw: &str) -> String {
    raw.trim()
        .trim_start_matches(['v', 'V'])
        .trim()
        .to_string()
}

/// Parses strict stable `(major, minor, patch)` semver.
/// Strips leading `v`/`V` and optional `+build` metadata, and rejects pre-release tags like `-rc1`.
pub fn parse_strict_semver(raw: &str) -> Option<(u64, u64, u64)> {
    let trimmed = raw.trim().trim_start_matches(['v', 'V']).trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_build = trimmed.split('+').next().unwrap_or("");
    if without_build.contains('-') {
        return None;
    }
    let mut parts = without_build.split('.');
    let major_s = parts.next()?;
    let minor_s = parts.next()?;
    let patch_s = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    for seg in [major_s, minor_s, patch_s] {
        if seg.is_empty() || !seg.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
    }
    Some((
        major_s.parse().ok()?,
        minor_s.parse().ok()?,
        patch_s.parse().ok()?,
    ))
}

/// Strict semver comparison: strips leading `v`, compares `(major, minor, patch)`,
/// and ignores pre-release candidate tags such as `-rc1` or `-beta`.
pub fn is_newer_version(current: &str, candidate: &str) -> bool {
    let Some(cand_ver) = parse_strict_semver(candidate) else {
        return false;
    };
    let cur_trimmed = current.trim().trim_start_matches(['v', 'V']).trim();
    let cur_no_build = cur_trimmed.split('+').next().unwrap_or("");
    let (cur_core, cur_has_prerelease) = match cur_no_build.split_once('-') {
        Some((base, pre)) => (base, !pre.is_empty()),
        None => (cur_no_build, false),
    };
    let Some(cur_ver) = parse_strict_semver(cur_core) else {
        return false;
    };
    cand_ver > cur_ver || (cand_ver == cur_ver && cur_has_prerelease)
}

pub fn update_state_path() -> PathBuf {
    HyperConfig::data_dir().join("update_state.json")
}

pub fn load_update_state(current_version: &str) -> UpdateState {
    load_update_state_from(&update_state_path(), current_version)
}

pub fn load_update_state_from(path: &Path, current_version: &str) -> UpdateState {
    let mut state: UpdateState = fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    state.sync_current_version(current_version);
    state
}

pub fn save_update_state(state: &UpdateState) -> Result<()> {
    save_update_state_to(&update_state_path(), state)
}

pub fn save_update_state_to(path: &Path, state: &UpdateState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create update state dir {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(state)?;
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

pub fn dismiss_update_version(dismiss_raw: &str, current_version: &str) -> Result<UpdateState> {
    dismiss_update_version_at(&update_state_path(), dismiss_raw, current_version)
}

pub fn dismiss_update_version_at(
    path: &Path,
    dismiss_raw: &str,
    current_version: &str,
) -> Result<UpdateState> {
    let mut st = load_update_state_from(path, current_version);
    let cleaned = dismiss_raw.trim();
    if cleaned.is_empty()
        || cleaned.eq_ignore_ascii_case("clear")
        || cleaned.eq_ignore_ascii_case("none")
    {
        st.dismissed_version = None;
    } else {
        st.dismissed_version = Some(normalize_semver_str(cleaned));
    }
    save_update_state_to(path, &st)?;
    Ok(st)
}

pub fn parse_release_tag_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let (_, after) = trimmed.split_once("/releases/tag/")?;
    let tag = after
        .split(['?', '#', '/'])
        .next()
        .unwrap_or("")
        .trim();
    let ver = normalize_semver_str(tag);
    parse_strict_semver(&ver)?;
    Some(ver)
}

fn parse_github_release_api_json(
    raw: &str,
    repo: &str,
) -> Option<(String, String, Option<String>)> {
    let val: Value = serde_json::from_str(raw).ok()?;
    if val.get("draft").and_then(Value::as_bool).unwrap_or(false)
        || val.get("prerelease").and_then(Value::as_bool).unwrap_or(false)
    {
        return None;
    }
    let tag = val.get("tag_name").and_then(Value::as_str)?;
    let ver = normalize_semver_str(tag);
    parse_strict_semver(&ver)?;
    let release_url = val
        .get("html_url")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| format!("https://github.com/{}/releases/tag/v{}", repo, ver));
    let published_at = val
        .get("published_at")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string());
    Some((ver, release_url, published_at))
}

fn validate_repo_slug(repo: &str) -> Result<String> {
    let cleaned = if repo.trim().is_empty() {
        DEFAULT_GITHUB_REPO
    } else {
        repo.trim()
    };
    let mut parts = cleaned.split('/');
    let Some(owner) = parts.next() else {
        bail!("invalid repository slug '{}'", cleaned);
    };
    let Some(name) = parts.next() else {
        bail!("invalid repository slug '{}'", cleaned);
    };
    if parts.next().is_some()
        || owner.is_empty()
        || name.is_empty()
        || owner == "."
        || owner == ".."
        || name == "."
        || name == ".."
        || !owner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("invalid repository slug '{}'", cleaned);
    }
    Ok(format!("{}/{}", owner, name))
}

fn curl_bin() -> &'static str {
    if cfg!(windows) {
        "curl.exe"
    } else {
        "curl"
    }
}

fn null_device_str() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

/// Checks the latest GitHub release without consuming the 60/hr unauthenticated REST rate limit:
/// 1. Primary: `curl -fsSLI -o /dev/null -w "%{url_effective}" --max-time 4 https://github.com/<repo>/releases/latest`
/// 2. Fallback: GitHub REST `/repos/<repo>/releases/latest`
///
/// Atomically persists `update_state.json` while preserving any user `dismissed_version`.
pub fn check_latest_release(repo: &str, current_version: &str) -> Result<UpdateState> {
    let repo_slug = validate_repo_slug(repo)?;
    let state_path = update_state_path();
    let prev = load_update_state_from(&state_path, current_version);
    let checked_at = now_epoch_ms_u64();

    let mut resolved: Option<(String, String, Option<String>)> = None;
    let mut last_err_msg = String::new();

    let latest_page = format!("https://github.com/{}/releases/latest", repo_slug);
    match Command::new(curl_bin())
        .args([
            "-fsSLI",
            "-o",
            null_device_str(),
            "-w",
            "%{url_effective}",
            "--max-time",
            "4",
            &latest_page,
        ])
        .stdin(Stdio::null())
        .output()
    {
        Ok(out) if out.status.success() => {
            let eff = String::from_utf8_lossy(&out.stdout);
            if let Some(ver) = parse_release_tag_from_url(&eff) {
                let url = format!("https://github.com/{}/releases/tag/v{}", repo_slug, ver);
                resolved = Some((ver, url, None));
            } else {
                last_err_msg = format!(
                    "redirect URL did not resolve to a semver release tag ({})",
                    eff.trim()
                );
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            last_err_msg = format!("redirect check exited {}: {}", out.status, stderr.trim());
        }
        Err(e) => {
            last_err_msg = format!("failed to run {}: {}", curl_bin(), e);
        }
    }

    if resolved.is_none() {
        let api_url = format!("https://api.github.com/repos/{}/releases/latest", repo_slug);
        match Command::new(curl_bin())
            .args([
                "-fsSL",
                "--max-time",
                "4",
                "-H",
                "Accept: application/vnd.github+json",
                &api_url,
            ])
            .stdin(Stdio::null())
            .output()
        {
            Ok(out) if out.status.success() => {
                let body = String::from_utf8_lossy(&out.stdout);
                if let Some(found) = parse_github_release_api_json(&body, &repo_slug) {
                    resolved = Some(found);
                } else if last_err_msg.is_empty() {
                    last_err_msg = "no stable semver GitHub release found".to_string();
                }
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                if last_err_msg.is_empty() {
                    last_err_msg = format!("API check exited {}: {}", out.status, stderr.trim());
                }
            }
            Err(e) => {
                if last_err_msg.is_empty() {
                    last_err_msg = format!("failed to run {}: {}", curl_bin(), e);
                }
            }
        }
    }

    match resolved {
        Some((latest_ver, release_url, published_at)) => {
            let norm_cur = normalize_semver_str(current_version);
            let update_available = is_newer_version(&norm_cur, &latest_ver);
            let next = UpdateState {
                checked_at_ms: checked_at,
                current_version: norm_cur,
                latest_version: Some(latest_ver),
                update_available,
                release_url: Some(release_url),
                published_at: published_at.or(prev.published_at),
                dismissed_version: prev.dismissed_version,
                last_error: None,
            };
            let _ = save_update_state_to(&state_path, &next);
            Ok(next)
        }
        None => {
            let err_text = if last_err_msg.is_empty() {
                "unable to query GitHub releases".to_string()
            } else {
                last_err_msg
            };
            let mut failed_state = prev;
            failed_state.checked_at_ms = checked_at;
            failed_state.sync_current_version(current_version);
            failed_state.last_error = Some(err_text.clone());
            let _ = save_update_state_to(&state_path, &failed_state);
            Err(anyhow!(err_text))
        }
    }
}

pub fn current_release_target() -> Result<(&'static str, &'static str)> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Ok(("aarch64-apple-darwin", "tar.gz"))
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Ok(("x86_64-apple-darwin", "tar.gz"))
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Ok(("x86_64-unknown-linux-musl", "tar.gz"))
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Ok(("aarch64-unknown-linux-musl", "tar.gz"))
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Ok(("x86_64-pc-windows-msvc", "zip"))
    } else {
        bail!(
            "unsupported platform for prebuilt release bundle ({}-{})",
            std::env::consts::ARCH,
            std::env::consts::OS
        )
    }
}

pub fn parse_sha256sums_entry(sums_text: &str, archive_name: &str) -> Option<String> {
    for line in sums_text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut cols = trimmed.split_whitespace();
        let hash = cols.next()?;
        let raw_file = cols.next()?;
        let file = raw_file.trim_start_matches('*');
        if file == archive_name
            && hash.len() == 64
            && hash.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

pub fn compute_file_sha256(path: &Path) -> Result<String> {
    #[cfg(not(windows))]
    {
        if let Ok(out) = Command::new("shasum")
            .arg("-a")
            .arg("256")
            .arg(path)
            .stdin(Stdio::null())
            .output()
        {
            if out.status.success() {
                if let Some(hex) = String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .next()
                    .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
                {
                    return Ok(hex.to_ascii_lowercase());
                }
            }
        }
        if let Ok(out) = Command::new("sha256sum")
            .arg(path)
            .stdin(Stdio::null())
            .output()
        {
            if out.status.success() {
                if let Some(hex) = String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .next()
                    .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
                {
                    return Ok(hex.to_ascii_lowercase());
                }
            }
        }
        bail!("neither 'shasum -a 256' nor 'sha256sum' succeeded when verifying archive")
    }

    #[cfg(windows)]
    {
        let out = Command::new("certutil")
            .arg("-hashfile")
            .arg(path)
            .arg("SHA256")
            .stdin(Stdio::null())
            .output()
            .context("invoke certutil -hashfile SHA256")?;
        if !out.status.success() {
            bail!("certutil -hashfile failed with {}", out.status);
        }
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
            if compact.len() == 64 && compact.chars().all(|c| c.is_ascii_hexdigit()) {
                return Ok(compact.to_ascii_lowercase());
            }
        }
        bail!("failed to parse SHA-256 digest from certutil output")
    }
}

pub fn verify_archive_sha256(
    archive_path: &Path,
    sums_path: &Path,
    archive_name: &str,
) -> Result<String> {
    let sums_text = fs::read_to_string(sums_path)
        .with_context(|| format!("read {}", sums_path.display()))?;
    let expected = parse_sha256sums_entry(&sums_text, archive_name).ok_or_else(|| {
        anyhow!(
            "checksum entry for '{}' missing in SHA256SUMS",
            archive_name
        )
    })?;
    let actual = compute_file_sha256(archive_path)?;
    if !actual.eq_ignore_ascii_case(&expected) {
        bail!(
            "SHA-256 verification FAILED for {}: expected {}, got {}",
            archive_name,
            expected,
            actual
        );
    }
    Ok(actual)
}

fn default_plugin_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("AGY_HYPER_PLUGIN_DIR") {
        if !custom.trim().is_empty() {
            return PathBuf::from(custom.trim());
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".gemini")
        .join("config")
        .join("plugins")
        .join("agy-hyper")
}

fn default_bin_link() -> PathBuf {
    if let Ok(custom) = std::env::var("AGY_HYPER_BIN_LINK") {
        if !custom.trim().is_empty() {
            return PathBuf::from(custom.trim());
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("bin")
        .join("agy-hyper")
}

fn read_plugin_version(plugin_dir: &Path) -> Option<String> {
    let raw = fs::read_to_string(plugin_dir.join("plugin.json")).ok()?;
    let val: Value = serde_json::from_str(&raw).ok()?;
    val.get("version")
        .and_then(Value::as_str)
        .map(normalize_semver_str)
        .filter(|s| !s.is_empty())
}

struct TempDirCleanup(PathBuf);

impl Drop for TempDirCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn validate_version_tag(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("latest") {
        return Ok("latest".to_string());
    }
    if trimmed.contains("..")
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        bail!("invalid version tag '{}'", trimmed);
    }
    if trimmed.starts_with(['v', 'V']) {
        Ok(format!("v{}", &trimmed[1..]))
    } else {
        Ok(format!("v{}", trimmed))
    }
}

fn extract_archive_to_dir(archive_path: &Path, ext: &str, dest_dir: &Path) -> Result<()> {
    fs::create_dir_all(dest_dir)?;
    if ext == "tar.gz" {
        let status = Command::new("tar")
            .arg("-xzf")
            .arg(archive_path)
            .arg("-C")
            .arg(dest_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .context("invoke tar -xzf")?;
        if !status.success() {
            bail!("tar -xzf failed with {}", status);
        }
        return Ok(());
    }
    if ext == "zip" {
        let tar_bin = if cfg!(windows) { "tar.exe" } else { "tar" };
        if let Ok(status) = Command::new(tar_bin)
            .arg("-xf")
            .arg(archive_path)
            .arg("-C")
            .arg(dest_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
        {
            if status.success() {
                return Ok(());
            }
        }
        #[cfg(windows)]
        {
            let ps_cmd = format!(
                "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
                archive_path.display().to_string().replace('\'', "''"),
                dest_dir.display().to_string().replace('\'', "''")
            );
            let status = Command::new("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", &ps_cmd])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .status()
                .context("invoke Expand-Archive")?;
            if !status.success() {
                bail!("Expand-Archive failed with {}", status);
            }
            return Ok(());
        }
        #[cfg(not(windows))]
        {
            let status = Command::new("unzip")
                .arg("-q")
                .arg(archive_path)
                .arg("-d")
                .arg(dest_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .status()
                .context("invoke unzip")?;
            if !status.success() {
                bail!("unzip failed with {}", status);
            }
            return Ok(());
        }
    }
    bail!("unsupported archive extension '{}'", ext)
}

fn sweep_stale_swap_dirs(plugin_parent: &Path) {
    let Ok(entries) = fs::read_dir(plugin_parent) else {
        return;
    };
    let cutoff = SystemTime::now().checked_sub(Duration::from_secs(300));
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        if !(s.starts_with("agy-hyper.bak.") || s.starts_with(".agy-hyper.stage.")) {
            continue;
        }
        let is_old = match (cutoff, entry.metadata().and_then(|m| m.modified())) {
            (Some(c), Ok(mtime)) => mtime < c,
            _ => false,
        };
        if is_old {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn stop_existing_daemon(plugin_dir: &Path) {
    let bin_name = if cfg!(windows) {
        "agy-hyper.exe"
    } else {
        "agy-hyper"
    };
    let existing = plugin_dir.join("bin").join(bin_name);
    if existing.is_file() {
        #[cfg(target_os = "macos")]
        {
            let _ = Command::new(&existing)
                .args(["menubar", "stop"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = Command::new(existing)
            .args(["daemon", "stop"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(target_os = "macos")]
fn is_menubar_running() -> bool {
    let pid_file = HyperConfig::run_dir().join("menubar.pid");
    let Ok(raw) = fs::read_to_string(pid_file) else {
        return false;
    };
    let Ok(pid) = raw.trim().parse::<i32>() else {
        return false;
    };
    pid > 1 && unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(target_os = "macos")]
fn refresh_macos_menubar_if_present(plugin_dir: &Path, bin_link: &Path, was_running: bool) {
    let bar_bin = plugin_dir.join("bin").join("agy-hyper-bar");
    if !bar_bin.is_file() {
        return;
    }
    if let Some(link_parent) = bin_link.parent() {
        if link_parent.is_dir() {
            let bar_link = link_parent.join("agy-hyper-bar");
            let tmp_link = link_parent.join(format!(".agy-hyper-bar.link.{}", std::process::id()));
            let _ = fs::remove_file(&tmp_link);
            if std::os::unix::fs::symlink(&bar_bin, &tmp_link).is_ok() {
                let _ = fs::rename(&tmp_link, &bar_link);
            }
        }
    }
    if std::env::var("AGY_HYPER_NO_MENUBAR")
        .ok()
        .as_deref()
        == Some("1")
    {
        return;
    }
    if let Some(home) = dirs::home_dir() {
        let plist_dir = home.join("Library").join("LaunchAgents");
        if fs::create_dir_all(&plist_dir).is_ok() {
            let plist_path = plist_dir.join("com.agyhyper.menubar.plist");
            let plist = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.agyhyper.menubar</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <false/>
</dict>
</plist>
"#,
                bar_bin.display()
            );
            let _ = fs::write(&plist_path, plist);
        }
    }
    let uid = unsafe { libc::getuid() };
    let has_gui_session = Command::new("launchctl")
        .args(["print", &format!("gui/{}", uid)])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if was_running || has_gui_session {
        let cli_bin = plugin_dir.join("bin").join("agy-hyper");
        if cli_bin.is_file() {
            let _ = Command::new(cli_bin)
                .args(["menubar", "start"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn recycle_sidecar_if_external() {
    let pid_file = HyperConfig::run_dir().join("sidecar.pid");
    let Ok(raw) = fs::read_to_string(pid_file) else {
        return;
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }

    #[cfg(unix)]
    {
        let Ok(pid) = trimmed.parse::<i32>() else {
            return;
        };
        if pid <= 1 {
            return;
        }
        // When Hyper Deck's `POST /api/update/apply` invokes `agy-hyper update --json`,
        // `main.mjs` is our parent process and schedules its own graceful exit after returning
        // the HTTP response. Never kill our own parent mid-execFile.
        let parent_pid = std::os::unix::process::parent_id() as i32;
        if pid == parent_pid || pid == std::process::id() as i32 {
            return;
        }
        let alive = unsafe { libc::kill(pid, 0) == 0 };
        if !alive {
            return;
        }
        if let Ok(out) = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "args="])
            .stdin(Stdio::null())
            .output()
        {
            if String::from_utf8_lossy(&out.stdout).contains("main.mjs") {
                unsafe {
                    let _ = libc::kill(pid, libc::SIGTERM);
                }
            }
        }
    }

    #[cfg(windows)]
    {
        if let Ok(pid) = trimmed.parse::<u32>() {
            if pid > 0 && pid != std::process::id() {
                let _ = Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/F"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

#[cfg(unix)]
fn refresh_cli_symlink_if_present(plugin_dir: &Path, bin_link: &Path) {
    if fs::symlink_metadata(bin_link).is_err() {
        return;
    }
    let Some(link_parent) = bin_link.parent() else {
        return;
    };
    let _ = fs::create_dir_all(link_parent);
    let tmp_link = link_parent.join(format!(".agy-hyper.link.{}", std::process::id()));
    let _ = fs::remove_file(&tmp_link);
    let target_bin = plugin_dir.join("bin").join("agy-hyper");
    if std::os::unix::fs::symlink(&target_bin, &tmp_link).is_ok() {
        let _ = fs::rename(&tmp_link, bin_link);
    }
}

/// Verifies SHA-256 against `SHA256SUMS`, extracts the bundle on the target filesystem
/// (fresh inodes, zero `EXDEV`, zero macOS Mach-O `SIGKILL`), atomically swaps `plugin_dir`,
/// stops old daemon, recycles external `sidecar.pid`, updates CLI symlink if present,
/// and marks `update_state.json` up to date.
#[allow(clippy::too_many_arguments)]
pub fn apply_verified_archive_bundle(
    archive_path: &Path,
    sums_path: &Path,
    archive_name: &str,
    ext: &str,
    plugin_dir: &Path,
    bin_link: &Path,
    state_path: &Path,
    previous_version: &str,
    release_url: &str,
    stop_services: bool,
) -> Result<UpdateOutcome> {
    verify_archive_sha256(archive_path, sums_path, archive_name)?;

    let plugin_parent = plugin_dir
        .parent()
        .ok_or_else(|| anyhow!("invalid plugin directory '{}'", plugin_dir.display()))?;
    fs::create_dir_all(plugin_parent)
        .with_context(|| format!("create plugin parent {}", plugin_parent.display()))?;
    sweep_stale_swap_dirs(plugin_parent);

    // Stage directly beside `plugin_dir` so `fs::rename` stays strictly on the same mount
    // (avoids Linux `/tmp` tmpfs -> `/home` EXDEV errors while guaranteeing fresh Mach-O inodes).
    let stage_root = plugin_parent.join(format!(
        ".agy-hyper.stage.{}.{}",
        std::process::id(),
        now_epoch_ms_u64()
    ));
    let _stage_cleanup = TempDirCleanup(stage_root.clone());
    extract_archive_to_dir(archive_path, ext, &stage_root)?;

    let staged_plugin = stage_root.join("agy-hyper");
    let bin_name = if cfg!(windows) {
        "agy-hyper.exe"
    } else {
        "agy-hyper"
    };
    let staged_bin = staged_plugin.join("bin").join(bin_name);
    if !staged_plugin.join("plugin.json").is_file() || !staged_bin.is_file() {
        bail!(
            "extracted archive is missing agy-hyper/plugin.json or bin/{}",
            bin_name
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&staged_bin, fs::Permissions::from_mode(0o755));
        let staged_bar = staged_plugin.join("bin").join("agy-hyper-bar");
        if staged_bar.is_file() {
            let _ = fs::set_permissions(&staged_bar, fs::Permissions::from_mode(0o755));
        }
    }

    #[cfg(windows)]
    {
        for cfg_name in ["mcp_config.json", "hooks.json"] {
            let p = staged_plugin.join(cfg_name);
            if let Ok(txt) = fs::read_to_string(&p) {
                if txt.contains("./bin/agy-hyper") && !txt.contains("./bin/agy-hyper.exe") {
                    let patched = txt.replace("./bin/agy-hyper", "./bin/agy-hyper.exe");
                    let _ = fs::write(&p, patched);
                }
            }
        }
    }

    let installed_version = read_plugin_version(&staged_plugin)
        .unwrap_or_else(|| normalize_semver_str(crate::version()));

    #[cfg(target_os = "macos")]
    let was_menubar_running = stop_services && is_menubar_running();

    if stop_services {
        stop_existing_daemon(plugin_dir);
    }

    let backup_dir = plugin_parent.join(format!(
        "agy-hyper.bak.{}.{}",
        std::process::id(),
        now_epoch_ms_u64()
    ));
    if backup_dir.exists() {
        let _ = fs::remove_dir_all(&backup_dir);
    }
    if plugin_dir.exists() {
        fs::rename(plugin_dir, &backup_dir).with_context(|| {
            format!(
                "move existing plugin {} -> {}",
                plugin_dir.display(),
                backup_dir.display()
            )
        })?;
    }

    if let Err(swap_err) = fs::rename(&staged_plugin, plugin_dir) {
        if backup_dir.exists() {
            let _ = fs::rename(&backup_dir, plugin_dir);
        }
        return Err(swap_err).context("swap staged plugin into place");
    }

    let _ = fs::remove_dir_all(&backup_dir);

    #[cfg(unix)]
    refresh_cli_symlink_if_present(plugin_dir, bin_link);
    #[cfg(not(unix))]
    let _ = bin_link;

    if stop_services {
        recycle_sidecar_if_external();
        #[cfg(target_os = "macos")]
        refresh_macos_menubar_if_present(plugin_dir, bin_link, was_menubar_running);
    }

    let prev_state = load_update_state_from(state_path, &installed_version);
    let updated_state = UpdateState {
        checked_at_ms: now_epoch_ms_u64(),
        current_version: installed_version.clone(),
        latest_version: Some(installed_version.clone()),
        update_available: false,
        release_url: Some(release_url.to_string()),
        published_at: prev_state.published_at,
        dismissed_version: prev_state.dismissed_version,
        last_error: None,
    };
    let _ = save_update_state_to(state_path, &updated_state);

    Ok(UpdateOutcome {
        previous_version: normalize_semver_str(previous_version),
        installed_version,
        plugin_dir: plugin_dir.display().to_string(),
        release_url: release_url.to_string(),
    })
}

pub fn perform_verified_update(
    repo: &str,
    requested_version: Option<&str>,
) -> Result<UpdateOutcome> {
    let repo_slug = validate_repo_slug(repo)?;
    let (target, ext) = current_release_target()?;
    let plugin_dir = default_plugin_dir();
    let bin_link = default_bin_link();
    let previous_version =
        read_plugin_version(&plugin_dir).unwrap_or_else(|| normalize_semver_str(crate::version()));

    let tag_spec = validate_version_tag(requested_version.unwrap_or("latest"))?;
    let (base_url, release_url) = if tag_spec == "latest" {
        if let Ok(st) = check_latest_release(&repo_slug, &previous_version) {
            if let Some(lat) = st.latest_version {
                let tag = format!("v{}", normalize_semver_str(&lat));
                (
                    format!("https://github.com/{}/releases/download/{}", repo_slug, tag),
                    st.release_url.unwrap_or_else(|| {
                        format!("https://github.com/{}/releases/tag/{}", repo_slug, tag)
                    }),
                )
            } else {
                (
                    format!("https://github.com/{}/releases/latest/download", repo_slug),
                    format!("https://github.com/{}/releases/latest", repo_slug),
                )
            }
        } else {
            (
                format!("https://github.com/{}/releases/latest/download", repo_slug),
                format!("https://github.com/{}/releases/latest", repo_slug),
            )
        }
    } else {
        (
            format!(
                "https://github.com/{}/releases/download/{}",
                repo_slug, tag_spec
            ),
            format!("https://github.com/{}/releases/tag/{}", repo_slug, tag_spec),
        )
    };

    let tmp_dir = std::env::temp_dir().join(format!(
        "agy-hyper-dl-{}-{}",
        std::process::id(),
        now_epoch_ms_u64()
    ));
    fs::create_dir_all(&tmp_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp_dir, fs::Permissions::from_mode(0o700));
    }
    let _dl_cleanup = TempDirCleanup(tmp_dir.clone());

    let archive_name = format!("agy-hyper-{}.{}", target, ext);
    let archive_path = tmp_dir.join(&archive_name);
    let sums_path = tmp_dir.join("SHA256SUMS");

    download_via_curl(&format!("{}/{}", base_url, archive_name), &archive_path)?;
    download_via_curl(&format!("{}/SHA256SUMS", base_url), &sums_path)?;

    let state_path = update_state_path();
    apply_verified_archive_bundle(
        &archive_path,
        &sums_path,
        &archive_name,
        ext,
        &plugin_dir,
        &bin_link,
        &state_path,
        &previous_version,
        &release_url,
        true,
    )
}

fn download_via_curl(url: &str, dest: &Path) -> Result<()> {
    let out = Command::new(curl_bin())
        .arg("-fsSL")
        .arg("--max-time")
        .arg("60")
        .arg("-o")
        .arg(dest)
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("invoke {} for {}", curl_bin(), url))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "download failed for {} ({}): {}",
            url,
            out.status,
            stderr.trim()
        );
    }
    Ok(())
}

pub fn now_epoch_ms_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Spawns `agy-hyper update --check-quiet` as a fully detached background process
/// (all stdio null, own session on Unix, hidden detached process on Windows).
pub fn spawn_detached_update_check(exe: &Path) {
    let mut cmd = Command::new(exe);
    cmd.args(["update", "--check-quiet"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }
    let _ = cmd.spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_semver_strips_v_compares_tuples_and_ignores_prereleases() {
        assert!(is_newer_version("0.4.4", "0.5.0"));
        assert!(is_newer_version("v0.4.4", "v0.5.0"));
        assert!(is_newer_version("0.5.0", "0.10.0"));
        assert!(is_newer_version("0.5.0", "1.0.0"));
        assert!(!is_newer_version("0.5.0", "0.5.0"));
        assert!(!is_newer_version("v0.5.0", "0.5.0"));
        assert!(!is_newer_version("0.5.1", "0.5.0"));

        // Pre-release candidates must be ignored by automatic radar checks
        assert!(!is_newer_version("0.4.4", "0.5.0-rc1"));
        assert!(!is_newer_version("0.4.4", "v0.5.0-beta.2"));
        assert!(!is_newer_version("0.5.0", "0.6.0-alpha"));

        // Local pre-release upgrading to stable of same tuple is recognized
        assert!(is_newer_version("0.5.0-rc1", "0.5.0"));
    }

    #[test]
    fn parses_github_redirect_url_and_api_fallback_cleanly() {
        assert_eq!(
            parse_release_tag_from_url("https://github.com/BobbyCats/agy-hyper/releases/tag/v0.5.2\n"),
            Some("0.5.2".to_string())
        );
        assert_eq!(
            parse_release_tag_from_url("https://github.com/BobbyCats/agy-hyper/releases/tag/v0.5.0-rc1"),
            None
        );
        assert_eq!(
            parse_release_tag_from_url("https://github.com/BobbyCats/agy-hyper/releases"),
            None
        );

        let api_ok = r#"{"tag_name":"v0.5.3","draft":false,"prerelease":false,"html_url":"https://github.com/BobbyCats/agy-hyper/releases/tag/v0.5.3","published_at":"2026-11-25T00:00:00Z"}"#;
        let (ver, url, pub_at) = parse_github_release_api_json(api_ok, DEFAULT_GITHUB_REPO).unwrap();
        assert_eq!(ver, "0.5.3");
        assert_eq!(url, "https://github.com/BobbyCats/agy-hyper/releases/tag/v0.5.3");
        assert_eq!(pub_at.as_deref(), Some("2026-11-25T00:00:00Z"));

        let api_pre = r#"{"tag_name":"v0.6.0","draft":false,"prerelease":true}"#;
        assert!(parse_github_release_api_json(api_pre, DEFAULT_GITHUB_REPO).is_none());
    }

    #[test]
    fn update_state_persistence_actionability_and_dismissal_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let state_file = dir.path().join("update_state.json");

        let initial = UpdateState {
            checked_at_ms: 1_700_000_000_000,
            current_version: "0.5.0".into(),
            latest_version: Some("0.5.2".into()),
            update_available: true,
            release_url: Some("https://github.com/BobbyCats/agy-hyper/releases/tag/v0.5.2".into()),
            published_at: None,
            dismissed_version: None,
            last_error: None,
        };
        save_update_state_to(&state_file, &initial).unwrap();

        let loaded = load_update_state_from(&state_file, "0.5.0");
        assert!(loaded.update_available);
        assert!(loaded.is_actionable("0.5.0"));

        // Dismissing v0.5.2 silences v0.5.2, but NOT a newer v0.5.3
        let dismissed = dismiss_update_version_at(&state_file, "v0.5.2", "0.5.0").unwrap();
        assert_eq!(dismissed.dismissed_version.as_deref(), Some("0.5.2"));
        assert!(!dismissed.is_actionable("0.5.0"));

        // Once binary upgrades to 0.5.2, load_update_state_from auto-clears update_available
        let after_upgrade = load_update_state_from(&state_file, "0.5.2");
        assert_eq!(after_upgrade.current_version, "0.5.2");
        assert!(!after_upgrade.update_available);
        assert!(!after_upgrade.is_actionable("0.5.2"));

        // Clearing dismissal restores None
        let cleared = dismiss_update_version_at(&state_file, "clear", "0.5.0").unwrap();
        assert!(cleared.dismissed_version.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn verified_bundle_install_checks_sha256_swaps_atomically_and_updates_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let build_dir = tmp.path().join("dist/agy-hyper/bin");
        fs::create_dir_all(&build_dir).unwrap();
        fs::write(
            tmp.path().join("dist/agy-hyper/plugin.json"),
            r#"{"name":"agy-hyper","version":"0.5.9"}"#,
        )
        .unwrap();
        fs::write(build_dir.join("agy-hyper"), b"#!/bin/sh\nexit 0\n").unwrap();

        let archive_name = "agy-hyper-test.tar.gz";
        let archive_path = tmp.path().join(archive_name);
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&archive_path)
            .arg("-C")
            .arg(tmp.path().join("dist"))
            .arg("agy-hyper")
            .status()
            .unwrap();
        assert!(status.success());

        let actual_sha = compute_file_sha256(&archive_path).unwrap();
        let bad_sums = tmp.path().join("BAD_SUMS");
        fs::write(&bad_sums, format!("{}  {}\n", "0".repeat(64), archive_name)).unwrap();

        let plugin_dir = tmp.path().join("plugins/agy-hyper");
        fs::create_dir_all(plugin_dir.join("bin")).unwrap();
        fs::write(plugin_dir.join("plugin.json"), r#"{"version":"0.5.0"}"#).unwrap();

        let bin_link = tmp.path().join("local-bin/agy-hyper");
        fs::create_dir_all(bin_link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(plugin_dir.join("bin/agy-hyper"), &bin_link).unwrap();
        let state_path = tmp.path().join("data/update_state.json");

        // Tampered checksum must abort without altering existing plugin_dir
        let err = apply_verified_archive_bundle(
            &archive_path,
            &bad_sums,
            archive_name,
            "tar.gz",
            &plugin_dir,
            &bin_link,
            &state_path,
            "0.5.0",
            "https://github.com/BobbyCats/agy-hyper/releases/tag/v0.5.9",
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("SHA-256 verification FAILED"));
        assert_eq!(read_plugin_version(&plugin_dir).as_deref(), Some("0.5.0"));

        // Valid checksum swaps atomically and updates version + symlink + isolated update_state.json
        let good_sums = tmp.path().join("SHA256SUMS");
        fs::write(&good_sums, format!("{}  {}\n", actual_sha, archive_name)).unwrap();
        let outcome = apply_verified_archive_bundle(
            &archive_path,
            &good_sums,
            archive_name,
            "tar.gz",
            &plugin_dir,
            &bin_link,
            &state_path,
            "0.5.0",
            "https://github.com/BobbyCats/agy-hyper/releases/tag/v0.5.9",
            false,
        )
        .unwrap();
        assert_eq!(outcome.previous_version, "0.5.0");
        assert_eq!(outcome.installed_version, "0.5.9");
        assert_eq!(read_plugin_version(&plugin_dir).as_deref(), Some("0.5.9"));
        assert_eq!(fs::read_link(&bin_link).unwrap(), plugin_dir.join("bin/agy-hyper"));
        let st = load_update_state_from(&state_path, "0.5.9");
        assert!(!st.update_available);
        assert_eq!(st.latest_version.as_deref(), Some("0.5.9"));
    }
}
