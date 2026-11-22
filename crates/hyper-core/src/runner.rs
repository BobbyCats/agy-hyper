use crate::config::HyperConfig;
use crate::diag::{distill_output, DiagnosticDigest, HeadTailBuffer};
use crate::jail::WorkspaceJail;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckPreset {
    pub id: String,
    pub program: String,
    pub args: Vec<String>,
    pub source: String,
    pub read_only_safe: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRequest {
    /// Named safe preset discovered from workspace manifests (e.g. "cargo:check", "npm:test", "pytest").
    #[serde(default)]
    pub preset: Option<String>,
    /// Explicit argv array (executed strictly with `shell: false`, never through `/bin/sh -c`).
    /// Only allowed when config.allow_custom_argv_checks == true and program is in safe allowlist.
    #[serde(default)]
    pub argv: Option<Vec<String>>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Optional no-output idle watchdog in seconds (`Some(0)` disables; `None` applies smart default:
    /// disabled for compile/static presets, 45s for test/script runs).
    #[serde(default)]
    pub idle_timeout_secs: Option<u64>,
    /// Optional dual-condition fatal stderr hang detector (`None` enables for non-compile test/script runs).
    /// Only aborts after a fatal signature AND 4s of subsequent silence while the process stays stuck.
    #[serde(default)]
    pub fail_fast: Option<bool>,
}

fn default_timeout_secs() -> u64 {
    60
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRunResult {
    pub command_display: String,
    pub exit_code: i32,
    pub timed_out: bool,
    #[serde(default)]
    pub idle_timed_out: bool,
    #[serde(default)]
    pub fail_fast_triggered: bool,
    pub cancelled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort_reason: Option<String>,
    pub duration_ms: u64,
    pub total_output_bytes: usize,
    pub digest: DiagnosticDigest,
    /// Full captured head+tail text (only kept in daemon handle LRU, never dumped inline raw).
    #[serde(skip)]
    pub full_captured_log: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spill_handle: Option<String>,
}

/// Safe executable allowlist for explicit `argv` (always `shell: false` — zero shell injection).
const SAFE_CHECK_PROGRAMS: &[&str] = &[
    "cargo", "rustc", "rustfmt", "npm", "pnpm", "yarn", "bun", "node", "tsc", "eslint",
    "biome", "oxlint", "vitest", "jest", "playwright", "turbo", "nx", "vite", "svelte-check",
    "astro", "pytest", "python3", "python", "ruff", "mypy", "pyright", "go",
    "golangci-lint", "swift", "xcodebuild", "make", "just", "gradle", "mvn", "dotnet", "zig",
];

/// Auto-discover safe project check presets from manifest files in the workspace root.
pub fn discover_presets(root: &Path) -> Vec<CheckPreset> {
    let mut presets = Vec::new();

    // Cargo.toml
    if root.join("Cargo.toml").exists() {
        presets.push(CheckPreset {
            id: "cargo:check".into(),
            program: "cargo".into(),
            args: vec!["check".into(), "--workspace".into()],
            source: "Cargo.toml".into(),
            read_only_safe: true,
        });
        presets.push(CheckPreset {
            id: "cargo:test".into(),
            program: "cargo".into(),
            args: vec!["test".into(), "--workspace".into()],
            source: "Cargo.toml".into(),
            read_only_safe: false,
        });
        presets.push(CheckPreset {
            id: "cargo:clippy".into(),
            program: "cargo".into(),
            args: vec!["clippy".into(), "--workspace".into()],
            source: "Cargo.toml".into(),
            read_only_safe: true,
        });
    }

    // package.json
    let pkg_path = root.join("package.json");
    if let Ok(raw) = fs::read_to_string(&pkg_path) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) {
            let pm = if root.join("pnpm-lock.yaml").exists() {
                "pnpm"
            } else if root.join("bun.lockb").exists() || root.join("bun.lock").exists() {
                "bun"
            } else if root.join("yarn.lock").exists() {
                "yarn"
            } else {
                "npm"
            };
            if let Some(scripts) = val.get("scripts").and_then(|v| v.as_object()) {
                for script_name in [
                    "typecheck",
                    "type-check",
                    "check-types",
                    "check",
                    "lint",
                    "verify:fast",
                    "verify",
                    "test:unit",
                    "test",
                    "build",
                    "check:ui",
                ] {
                    if scripts.contains_key(script_name) {
                        let ro = matches!(
                            script_name,
                            "typecheck"
                                | "type-check"
                                | "check-types"
                                | "check"
                                | "lint"
                                | "check:ui"
                        );
                        presets.push(CheckPreset {
                            id: format!("pkg:{}", script_name),
                            program: pm.into(),
                            args: vec!["run".into(), script_name.into()],
                            source: "package.json".into(),
                            read_only_safe: ro,
                        });
                    }
                }
            }

            let is_monorepo = root.join("pnpm-workspace.yaml").exists()
                || val.get("workspaces").is_some();
            if is_monorepo {
                discover_monorepo_pkg_presets(root, pm, &mut presets);
            }
        }
    }

    // pyproject.toml / pytest
    if root.join("pyproject.toml").exists() || root.join("pytest.ini").exists() {
        presets.push(CheckPreset {
            id: "py:pytest".into(),
            program: "pytest".into(),
            args: vec!["-q".into()],
            source: "pyproject.toml".into(),
            read_only_safe: false,
        });
        presets.push(CheckPreset {
            id: "py:ruff".into(),
            program: "ruff".into(),
            args: vec!["check".into(), ".".into()],
            source: "pyproject.toml".into(),
            read_only_safe: true,
        });
    }

    // go.mod
    if root.join("go.mod").exists() {
        presets.push(CheckPreset {
            id: "go:vet".into(),
            program: "go".into(),
            args: vec!["vet".into(), "./...".into()],
            source: "go.mod".into(),
            read_only_safe: true,
        });
        presets.push(CheckPreset {
            id: "go:test".into(),
            program: "go".into(),
            args: vec!["test".into(), "./...".into()],
            source: "go.mod".into(),
            read_only_safe: false,
        });
    }

    presets
}

fn discover_monorepo_pkg_presets(root: &Path, pm: &str, presets: &mut Vec<CheckPreset>) {
    let mut pkg_dirs = Vec::new();
    for group in ["packages", "apps"] {
        let dir = root.join(group);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut children: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        children.sort();
        for p in children {
            if p.is_dir() && p.join("package.json").is_file() {
                pkg_dirs.push(p);
                if pkg_dirs.len() >= 16 {
                    break;
                }
            }
        }
        if pkg_dirs.len() >= 16 {
            break;
        }
    }

    for pkg_dir in pkg_dirs {
        let Ok(raw) = fs::read_to_string(pkg_dir.join("package.json")) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(scripts) = val.get("scripts").and_then(|v| v.as_object()) else {
            continue;
        };
        let Some(dir_name) = pkg_dir.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let pkg_name = val
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(dir_name);
        let rel_dir = pkg_dir
            .strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| dir_name.to_string());

        for script_name in ["typecheck", "lint", "test", "build"] {
            if !scripts.contains_key(script_name) {
                continue;
            }
            let args = match pm {
                "pnpm" => vec![
                    "--filter".into(),
                    pkg_name.into(),
                    "run".into(),
                    script_name.into(),
                ],
                "yarn" => vec![
                    "workspace".into(),
                    pkg_name.into(),
                    "run".into(),
                    script_name.into(),
                ],
                "bun" => vec![
                    "--filter".into(),
                    pkg_name.into(),
                    "run".into(),
                    script_name.into(),
                ],
                _ => vec![
                    "--workspace".into(),
                    rel_dir.clone(),
                    "run".into(),
                    script_name.into(),
                ],
            };
            let ro = matches!(script_name, "typecheck" | "lint");
            presets.push(CheckPreset {
                id: format!("pkg:{}:{}", dir_name, script_name),
                program: pm.into(),
                args,
                source: format!("{}/package.json", rel_dir),
                read_only_safe: ro,
            });
        }
    }
}

/// Execute a check safely with:
/// - `shell: false` (direct `execve` / `CreateProcess`, ZERO shell injection)
/// - Sanitized environment (strips obvious secret tokens while preserving toolchain PATH/HOME/LANG)
/// - Dedicated process group (`setsid` on Unix) so timeout OR cancellation kills the ENTIRE process tree
/// - Head 64KB + Tail 512KB ring buffer (never loses fatal tail stderr!)
pub fn run_check(
    jail: &WorkspaceJail,
    cfg: &HyperConfig,
    req: &CheckRequest,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> Result<CheckRunResult> {
    let presets = discover_presets(jail.root());
    let preset_map: HashMap<&str, &CheckPreset> =
        presets.iter().map(|p| (p.id.as_str(), p)).collect();

    let (program, args, is_compile_or_static) = if let Some(preset_id) = &req.preset {
        let p = preset_map.get(preset_id.as_str()).ok_or_else(|| {
            let available: Vec<&str> = presets.iter().map(|x| x.id.as_str()).collect();
            anyhow!(
                "unknown preset '{}'; available in this repo: [{}]",
                preset_id,
                available.join(", ")
            )
        })?;
        let compile_like = p.read_only_safe
            || preset_id.ends_with(":build")
            || preset_id.ends_with(":typecheck")
            || preset_id.ends_with(":check");
        (p.program.clone(), p.args.clone(), compile_like)
    } else if let Some(argv) = &req.argv {
        if !cfg.allow_custom_argv_checks {
            return Err(anyhow!(
                "custom argv checks are disabled under '{}' profile; use a discovered preset or switch to Balanced/Hyper profile",
                cfg.profile.as_str()
            ));
        }
        if argv.is_empty() {
            return Err(anyhow!("argv must not be empty"));
        }
        let prog = argv[0].trim();
        if prog.contains('/') || prog.contains('\\') {
            return Err(anyhow!(
                "program '{}' must be a bare executable name on PATH (paths are disallowed in hyper_check)",
                prog
            ));
        }
        if !SAFE_CHECK_PROGRAMS.contains(&prog) {
            return Err(anyhow!(
                "program '{}' is not in the safe check allowlist (shell execution is disabled by design); allowed tools: {}",
                prog,
                SAFE_CHECK_PROGRAMS.join(", ")
            ));
        }
        // Extra defense: block inline eval/preload flags (exact or attached `-c...` / `--eval=...`) on runtimes
        if matches!(prog, "node" | "python" | "python3" | "bun") {
            for a in &argv[1..] {
                let s = a.as_str();
                if matches!(s, "-e" | "--eval" | "-c" | "-p" | "--print" | "-r" | "--require" | "--import")
                    || s.starts_with("-e")
                    || s.starts_with("-c")
                    || s.starts_with("-p")
                    || s.starts_with("--eval=")
                    || s.starts_with("--print=")
                    || s.starts_with("--require=")
                    || s.starts_with("--import=")
                {
                    return Err(anyhow!(
                        "inline eval/preload flag '{}' is blocked in hyper_check (use a preset or script file)",
                        a
                    ));
                }
            }
        }
        let first_sub = argv.get(1).map(|s| s.as_str()).unwrap_or("");
        let compile_like = matches!(
            prog,
            "rustc" | "tsc" | "svelte-check" | "xcodebuild" | "gradle" | "mvn" | "ruff" | "mypy" | "pyright"
        ) || (prog == "cargo" && matches!(first_sub, "check" | "build" | "clippy"))
            || (prog == "go" && matches!(first_sub, "build" | "vet"));
        (prog.to_string(), argv[1..].to_vec(), compile_like)
    } else {
        return Err(anyhow!("hyper_check requires either 'preset' or 'argv'"));
    };

    let work_dir = match &req.cwd {
        Some(c) if !c.trim().is_empty() => jail.resolve_existing(c)?,
        _ => jail.root().to_path_buf(),
    };

    let command_display = if args.is_empty() {
        program.clone()
    } else {
        format!("{} {}", program, args.join(" "))
    };

    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .current_dir(&work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .env("CI", "1");

    // GUI-spawned MCP servers on macOS/Linux often inherit a minimal PATH missing
    // ~/.cargo/bin, /opt/homebrew/bin, or ~/.local/bin. Append standard toolchain dirs as fallback.
    #[cfg(unix)]
    {
        let mut path_val = std::env::var("PATH").unwrap_or_default();
        let mut extras = vec!["/opt/homebrew/bin".to_string(), "/usr/local/bin".to_string()];
        if let Some(home) = dirs::home_dir() {
            extras.push(home.join(".cargo/bin").display().to_string());
            extras.push(home.join(".local/bin").display().to_string());
        }
        for ex in extras {
            if !path_val.split(':').any(|p| p == ex) && Path::new(&ex).is_dir() {
                if !path_val.is_empty() {
                    path_val.push(':');
                }
                path_val.push_str(&ex);
            }
        }
        if !path_val.is_empty() {
            cmd.env("PATH", path_val);
        }
    }

    // Strip high-risk secret env vars from child check processes by default.
    for (k, _) in std::env::vars() {
        let ku = k.to_ascii_uppercase();
        if ku.contains("SECRET")
            || ku.contains("API_KEY")
            || ku.contains("PRIVATE_KEY")
            || ku.contains("PASSWORD")
            || ku.contains("PASSWD")
            || ku.contains("CREDENTIAL")
            || ku.contains("DATABASE_URL")
            || ku.contains("COOKIE")
            || ku.contains("ACCESS_TOKEN")
            || ku.contains("CSRF_TOKEN")
            || ku.ends_with("_TOKEN")
            || ku.ends_with("_KEY")
        {
            cmd.env_remove(&k);
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                // Start a new session/process group so we can atomically kill the entire tree.
                libc::setsid();
                Ok(())
            });
        }
    }

    let started = Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("failed to spawn '{}': {}", program, e))?;
    let child_pid = child.id();

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let last_output_ms = Arc::new(AtomicU64::new(0));
    let fatal_seen_ms = Arc::new(AtomicU64::new(0));

    let out_ts = last_output_ms.clone();
    let out_started = started;
    let out_handle = std::thread::spawn(move || {
        let mut buf = HeadTailBuffer::default();
        if let Some(mut r) = stdout_pipe.take() {
            let mut tmp = [0u8; 8192];
            while let Ok(n) = r.read(&mut tmp) {
                if n == 0 {
                    break;
                }
                out_ts.store(out_started.elapsed().as_millis() as u64, Ordering::Relaxed);
                buf.push(&tmp[..n]);
            }
        }
        buf
    });

    let err_ts = last_output_ms.clone();
    let err_fatal = fatal_seen_ms.clone();
    let err_started = started;
    let err_handle = std::thread::spawn(move || {
        let mut buf = HeadTailBuffer::default();
        if let Some(mut r) = stderr_pipe.take() {
            let mut tmp = [0u8; 8192];
            let mut tail_carry = String::new();
            while let Ok(n) = r.read(&mut tmp) {
                if n == 0 {
                    break;
                }
                let elapsed_ms = err_started.elapsed().as_millis() as u64;
                err_ts.store(elapsed_ms, Ordering::Relaxed);
                buf.push(&tmp[..n]);
                if err_fatal.load(Ordering::Relaxed) == 0 {
                    let chunk = String::from_utf8_lossy(&tmp[..n]);
                    let combined = if tail_carry.is_empty() {
                        chunk.into_owned()
                    } else {
                        format!("{}{}", tail_carry, chunk)
                    };
                    if contains_fatal_hang_signature(&combined) {
                        err_fatal.store(elapsed_ms.max(1), Ordering::Relaxed);
                    }
                    let keep_from = combined.len().saturating_sub(128);
                    let mut boundary = keep_from;
                    while boundary < combined.len() && !combined.is_char_boundary(boundary) {
                        boundary += 1;
                    }
                    tail_carry = combined[boundary..].to_string();
                }
            }
        }
        buf
    });

    let timeout_secs_clamped = req.timeout_secs.clamp(2, 300);
    let timeout = Duration::from_secs(timeout_secs_clamped);
    // Compile/static presets default to wall-clock only so silent link/typecheck stages never false-kill.
    let idle_timeout_ms: Option<u64> = match req.idle_timeout_secs {
        Some(0) => None,
        Some(s) => Some((s.clamp(2, 300) * 1000).min(timeout_secs_clamped * 1000)),
        None if is_compile_or_static => None,
        None => {
            let default_idle = 45u64;
            (default_idle < timeout_secs_clamped).then_some(default_idle * 1000)
        }
    };
    let fail_fast_enabled = req.fail_fast.unwrap_or(!is_compile_or_static);
    const FATAL_SILENCE_GRACE_MS: u64 = 4_000;

    let mut timed_out = false;
    let mut idle_timed_out = false;
    let mut fail_fast_triggered = false;
    let mut cancelled = false;
    let mut abort_reason: Option<String> = None;
    let exit_code: i32;

    loop {
        if let Some(status) = child.try_wait()? {
            exit_code = status.code().unwrap_or(-1);
            break;
        }
        if let Some(flag) = &cancel_flag {
            if flag.load(Ordering::Relaxed) {
                cancelled = true;
                abort_reason = Some("cancelled".into());
                kill_process_tree(child_pid);
                let _ = child.kill();
                let _ = child.wait();
                exit_code = -2;
                break;
            }
        }
        let elapsed = started.elapsed();
        let now_ms = elapsed.as_millis() as u64;
        let last_ms = last_output_ms.load(Ordering::Relaxed);
        let silence_ms = now_ms.saturating_sub(last_ms);

        if fail_fast_enabled
            && fatal_seen_ms.load(Ordering::Relaxed) > 0
            && silence_ms >= FATAL_SILENCE_GRACE_MS
        {
            fail_fast_triggered = true;
            abort_reason = Some("fatal_hang".into());
            kill_process_tree(child_pid);
            let _ = child.kill();
            let _ = child.wait();
            exit_code = -3;
            break;
        }

        if let Some(idle_ms) = idle_timeout_ms {
            if silence_ms >= idle_ms {
                idle_timed_out = true;
                abort_reason = Some("idle_hang".into());
                kill_process_tree(child_pid);
                let _ = child.kill();
                let _ = child.wait();
                exit_code = -4;
                break;
            }
        }

        if elapsed >= timeout {
            timed_out = true;
            abort_reason = Some("timeout".into());
            kill_process_tree(child_pid);
            let _ = child.kill();
            let _ = child.wait();
            exit_code = -1;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let out_buf = out_handle.join().unwrap_or_default();
    let err_buf = err_handle.join().unwrap_or_default();
    let duration_ms = started.elapsed().as_millis() as u64;
    let total_output_bytes = out_buf.total_bytes() + err_buf.total_bytes();

    let out_str = out_buf.render_lossy();
    let err_str = err_buf.render_lossy();
    let full_captured_log = if err_str.is_empty() {
        out_str.clone()
    } else if out_str.is_empty() {
        err_str.clone()
    } else {
        format!("=== STDOUT ===\n{}\n\n=== STDERR ===\n{}", out_str, err_str)
    };

    let digest = distill_output(
        &out_str,
        &err_str,
        exit_code,
        cfg.max_output_bytes.saturating_sub(1200).max(1500),
    );

    Ok(CheckRunResult {
        command_display,
        exit_code,
        timed_out,
        idle_timed_out,
        fail_fast_triggered,
        cancelled,
        abort_reason,
        duration_ms,
        total_output_bytes,
        digest,
        full_captured_log,
        spill_handle: None,
    })
}

fn contains_fatal_hang_signature(stderr_chunk: &str) -> bool {
    let lower = stderr_chunk.to_ascii_lowercase();
    lower.contains("traceback (most recent call last)")
        || lower.contains("panicked at")
        || lower.contains("fatal error:")
        || lower.contains("exc_bad_access")
        || lower.contains("segmentation fault")
        || lower.contains("abort trap")
        || lower.contains("unhandledpromiserejection")
}

pub fn kill_process_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        // Because we called setsid(), PGID == PID. Negative PID signals the entire process group!
        let pgid = -(pid as libc::pid_t);
        libc::kill(pgid, libc::SIGTERM);
        std::thread::sleep(Duration::from_millis(40));
        libc::kill(pgid, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = pid;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn blocks_arbitrary_shell_and_inline_eval() {
        let dir = tempdir().unwrap();
        let jail = WorkspaceJail::new(dir.path(), false, true).unwrap();
        let cfg = HyperConfig::default();

        let err1 = run_check(
            &jail,
            &cfg,
            &CheckRequest {
                preset: None,
                argv: Some(vec!["sh".into(), "-c".into(), "echo pwned".into()]),
                timeout_secs: 5,
                cwd: None,
                idle_timeout_secs: None,
                fail_fast: None,
            },
            None,
        )
        .unwrap_err();
        assert!(err1.to_string().contains("allowlist"));

        let err2 = run_check(
            &jail,
            &cfg,
            &CheckRequest {
                preset: None,
                argv: Some(vec!["node".into(), "-e".into(), "console.log(1)".into()]),
                timeout_secs: 5,
                cwd: None,
                idle_timeout_secs: None,
                fail_fast: None,
            },
            None,
        )
        .unwrap_err();
        assert!(err2.to_string().contains("inline eval"));

        // Regression: path-spoofed argv[0], attached `-c...`, and `npx` must all be blocked.
        let err3 = run_check(
            &jail,
            &cfg,
            &CheckRequest {
                preset: None,
                argv: Some(vec!["/tmp/evil/node".into(), "a.js".into()]),
                timeout_secs: 5,
                cwd: None,
                idle_timeout_secs: None,
                fail_fast: None,
            },
            None,
        )
        .unwrap_err();
        assert!(err3.to_string().contains("bare executable"));

        let err4 = run_check(
            &jail,
            &cfg,
            &CheckRequest {
                preset: None,
                argv: Some(vec!["python3".into(), "-cimport os".into()]),
                timeout_secs: 5,
                cwd: None,
                idle_timeout_secs: None,
                fail_fast: None,
            },
            None,
        )
        .unwrap_err();
        assert!(err4.to_string().contains("inline eval"));
    }

    #[test]
    fn cancellation_kills_child_process_cleanly() {
        let dir = tempdir().unwrap();
        let script = dir.path().join("slow.mjs");
        fs::write(&script, "setTimeout(() => console.log('done'), 30000);\n").unwrap();
        let jail = WorkspaceJail::new(dir.path(), false, true).unwrap();
        let cfg = HyperConfig::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_bg = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            cancel_bg.store(true, Ordering::Relaxed);
        });

        let res = run_check(
            &jail,
            &cfg,
            &CheckRequest {
                preset: None,
                argv: Some(vec!["node".into(), "slow.mjs".into()]),
                timeout_secs: 15,
                cwd: None,
                idle_timeout_secs: Some(0),
                fail_fast: None,
            },
            Some(cancel),
        )
        .unwrap();
        assert!(res.cancelled);
        assert_eq!(res.abort_reason.as_deref(), Some("cancelled"));
        assert!(res.duration_ms < 2000);
    }

    #[test]
    fn idle_watchdog_kills_silent_hang_early() {
        let dir = tempdir().unwrap();
        let script = dir.path().join("hang.mjs");
        fs::write(&script, "console.log('boot'); setTimeout(() => {}, 30000);\n").unwrap();
        let jail = WorkspaceJail::new(dir.path(), false, true).unwrap();
        let cfg = HyperConfig::default();

        let res = run_check(
            &jail,
            &cfg,
            &CheckRequest {
                preset: None,
                argv: Some(vec!["node".into(), "hang.mjs".into()]),
                timeout_secs: 20,
                cwd: None,
                idle_timeout_secs: Some(2),
                fail_fast: Some(false),
            },
            None,
        )
        .unwrap();
        assert!(res.idle_timed_out);
        assert!(!res.timed_out);
        assert_eq!(res.abort_reason.as_deref(), Some("idle_hang"));
        assert!(res.duration_ms < 6000);
    }

    #[test]
    fn fail_fast_does_not_false_kill_normal_exiting_traceback() {
        let dir = tempdir().unwrap();
        let script = dir.path().join("norm_err.mjs");
        fs::write(
            &script,
            "console.error('Traceback (most recent call last): test case'); setTimeout(() => { console.log('done'); process.exit(0); }, 200);\n",
        )
        .unwrap();
        let jail = WorkspaceJail::new(dir.path(), false, true).unwrap();
        let cfg = HyperConfig::default();

        let res = run_check(
            &jail,
            &cfg,
            &CheckRequest {
                preset: None,
                argv: Some(vec!["node".into(), "norm_err.mjs".into()]),
                timeout_secs: 10,
                cwd: None,
                idle_timeout_secs: Some(8),
                fail_fast: Some(true),
            },
            None,
        )
        .unwrap();
        assert_eq!(res.exit_code, 0);
        assert!(!res.fail_fast_triggered);
        assert!(res.abort_reason.is_none());
    }

    #[test]
    fn discovers_monorepo_workspace_presets() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
        fs::write(dir.path().join("pnpm-workspace.yaml"), "packages:\n  - 'packages/*'\n").unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"name":"root","scripts":{"verify:fast":"turbo run test"}}"#,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("packages/core")).unwrap();
        fs::write(
            dir.path().join("packages/core/package.json"),
            r#"{"name":"@acme/core","scripts":{"typecheck":"tsc --noEmit","test":"vitest run"}}"#,
        )
        .unwrap();

        let presets = discover_presets(dir.path());
        let ids: Vec<&str> = presets.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"pkg:verify:fast"));
        assert!(ids.contains(&"pkg:core:typecheck"));
        assert!(ids.contains(&"pkg:core:test"));
        let tc = presets.iter().find(|p| p.id == "pkg:core:typecheck").unwrap();
        assert_eq!(tc.program, "pnpm");
        assert_eq!(tc.args, vec!["--filter", "@acme/core", "run", "typecheck"]);
        assert!(tc.read_only_safe);
    }
}
