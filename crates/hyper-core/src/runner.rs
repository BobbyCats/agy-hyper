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
use std::sync::atomic::{AtomicBool, Ordering};
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
}

fn default_timeout_secs() -> u64 {
    60
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRunResult {
    pub command_display: String,
    pub exit_code: i32,
    pub timed_out: bool,
    pub cancelled: bool,
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
    "biome", "vitest", "jest", "pytest", "python3", "python", "ruff", "mypy", "pyright", "go",
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
                    "check",
                    "lint",
                    "test",
                    "build",
                    "check:ui",
                ] {
                    if scripts.contains_key(script_name) {
                        let ro = matches!(
                            script_name,
                            "typecheck" | "type-check" | "check" | "lint" | "check:ui"
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

    let (program, args) = if let Some(preset_id) = &req.preset {
        let p = preset_map.get(preset_id.as_str()).ok_or_else(|| {
            let available: Vec<&str> = presets.iter().map(|x| x.id.as_str()).collect();
            anyhow!(
                "unknown preset '{}'; available in this repo: [{}]",
                preset_id,
                available.join(", ")
            )
        })?;
        (p.program.clone(), p.args.clone())
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
        (prog.to_string(), argv[1..].to_vec())
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

    let out_handle = std::thread::spawn(move || {
        let mut buf = HeadTailBuffer::default();
        if let Some(mut r) = stdout_pipe.take() {
            let mut tmp = [0u8; 8192];
            while let Ok(n) = r.read(&mut tmp) {
                if n == 0 {
                    break;
                }
                buf.push(&tmp[..n]);
            }
        }
        buf
    });

    let err_handle = std::thread::spawn(move || {
        let mut buf = HeadTailBuffer::default();
        if let Some(mut r) = stderr_pipe.take() {
            let mut tmp = [0u8; 8192];
            while let Ok(n) = r.read(&mut tmp) {
                if n == 0 {
                    break;
                }
                buf.push(&tmp[..n]);
            }
        }
        buf
    });

    let timeout = Duration::from_secs(req.timeout_secs.clamp(2, 300));
    let mut timed_out = false;
    let mut cancelled = false;
    let exit_code: i32;

    loop {
        if let Some(status) = child.try_wait()? {
            exit_code = status.code().unwrap_or(-1);
            break;
        }
        if let Some(flag) = &cancel_flag {
            if flag.load(Ordering::Relaxed) {
                cancelled = true;
                kill_process_tree(child_pid);
                let _ = child.kill();
                let _ = child.wait();
                exit_code = -2;
                break;
            }
        }
        if started.elapsed() >= timeout {
            timed_out = true;
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
        cancelled,
        duration_ms,
        total_output_bytes,
        digest,
        full_captured_log,
        spill_handle: None,
    })
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
            },
            Some(cancel),
        )
        .unwrap();
        assert!(res.cancelled);
        assert!(res.duration_ms < 2000);
    }
}
