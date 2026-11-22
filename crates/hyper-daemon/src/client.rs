use crate::protocol::{DaemonRequest, DaemonResponse};
use crate::server::socket_path;
use anyhow::{anyhow, Result};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::net::UnixStream;

/// Synchronous ultra-fast (`<0.5ms`) daemon RPC client used by Hooks, MCP, and CLI.
pub fn send_request(req: &DaemonRequest) -> Result<DaemonResponse> {
    send_request_with_timeout(req, 2000)
}

#[cfg(unix)]
pub fn send_request_with_timeout(
    req: &DaemonRequest,
    read_timeout_ms: u64,
) -> Result<DaemonResponse> {
    let sock = socket_path();
    let mut stream = UnixStream::connect(&sock)?;
    let clamped = read_timeout_ms.clamp(10, 10_000);
    stream.set_read_timeout(Some(Duration::from_millis(clamped)))?;
    stream.set_write_timeout(Some(Duration::from_millis(clamped.min(500))))?;

    let mut payload = serde_json::to_string(req)?;
    payload.push('\n');
    stream.write_all(payload.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim().is_empty() {
        return Err(anyhow!("empty daemon response"));
    }
    let resp: DaemonResponse = serde_json::from_str(line.trim())?;
    Ok(resp)
}

#[cfg(not(unix))]
pub fn send_request_with_timeout(
    req: &DaemonRequest,
    read_timeout_ms: u64,
) -> Result<DaemonResponse> {
    use hyper_core::config::HyperConfig;
    use std::net::TcpStream;
    let run_dir = HyperConfig::run_dir();
    let addr = std::fs::read_to_string(run_dir.join("agy-hyper-v1.addr"))?;
    let token = std::fs::read_to_string(run_dir.join("agy-hyper-v1.token"))?;
    let mut stream = TcpStream::connect(addr.trim())?;
    let clamped = read_timeout_ms.clamp(10, 10_000);
    stream.set_read_timeout(Some(Duration::from_millis(clamped)))?;
    let payload = format!("{}\n{}\n", token.trim(), serde_json::to_string(req)?);
    stream.write_all(payload.as_bytes())?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(serde_json::from_str(line.trim())?)
}

/// Ensure daemon is running (spawning detached if absent) and execute `req`.
/// Returns Err gracefully if spawning fails, allowing caller to execute in-process seamlessly.
pub fn ensure_and_send(req: &DaemonRequest) -> Result<DaemonResponse> {
    if let Ok(r) = send_request(req) {
        return Ok(r);
    }
    let _ = spawn_daemon_detached();
    for _ in 0..12 {
        std::thread::sleep(Duration::from_millis(20));
        if let Ok(r) = send_request(req) {
            return Ok(r);
        }
    }
    Err(anyhow!("daemon unavailable (falling back to in-process execution)"))
}

pub fn spawn_daemon_detached() -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args(["daemon", "serve"])
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
    cmd.spawn()?;
    Ok(())
}
