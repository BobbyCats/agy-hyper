---
name: agy-hyper
description: Install, verify, configure, inspect usage/telemetry, rollback WAL file edits, or cleanly uninstall AGY Hyper (native Rust acceleration engine, WAL atomic edits, subagent tree monitor, multi-currency usage ledger, and Hyper Deck sidecar cockpit for Antigravity) on macOS, Linux, and Windows.
---

# Agent Operations Guide: AGY Hyper

Use this guide when a user asks you to install, upgrade, verify, configure, troubleshoot, or uninstall **AGY Hyper** (`BobbyCats/agy-hyper`).

> **Architecture & Safety Guarantees (`v0.5.1`)**:
> - Uses **100% official Antigravity plugin protocols** (MCP, Hooks, Rules, `SIDECAR_UI_ENTRYPOINT_AUX_PANE`). Does **not** inject into Electron DOM, open CDP debugging ports, or modify `~/.gemini/config/config.json`.
> - Single ~10MB static Rust binary (`agy-hyper`); zero Rust/Python/`ripgrep`/`git` runtime dependencies required for release installs.
> - Zero telemetry uploads and zero synchronous hook network calls. `Release Update Radar` uses rate-limit-free GitHub `Location` redirects (`enable_update_check`, or disable with `AGY_HYPER_NO_UPDATE_CHECK=1`) and **never** auto-installs binaries without explicit `agy-hyper update` / **Update Now** (`SHA256SUMS` verified).

## 1. Key Paths

| Component | Path |
| :--- | :--- |
| Installed plugin bundle | `~/.gemini/config/plugins/agy-hyper` |
| CLI symlink (macOS/Linux) | `~/.local/bin/agy-hyper` |
| Runtime state, config, ledger & WAL | `~/.gemini/antigravity/agy-hyper` |
| User configuration | `~/.gemini/antigravity/agy-hyper/config.json` |
| Release update radar state | `~/.gemini/antigravity/agy-hyper/update_state.json` |
| Incremental usage ledger | `~/.gemini/antigravity/agy-hyper/usage_ledger.json` |
| BLAKE3 WAL undo snapshots | `~/.gemini/antigravity/agy-hyper/wal/` |
| Local UDS socket & PIDs | `~/.gemini/antigravity/agy-hyper/run/` |

## 2. Install or Upgrade

### Fast In-Place Verified Upgrade (`v0.5.0+` already installed)
```bash
agy-hyper update --check   # inspect latest release without installing
agy-hyper update           # download release + SHA256SUMS, verify & atomically swap
```

### macOS & Linux (SHA-256 verified release)
```bash
curl -fsSL https://raw.githubusercontent.com/BobbyCats/agy-hyper/main/install.sh | bash
```

### Windows (PowerShell, SHA-256 verified release)
```powershell
irm https://raw.githubusercontent.com/BobbyCats/agy-hyper/main/install.ps1 | iex
```

### From Local Source Checkout
```bash
git clone --depth=1 https://github.com/BobbyCats/agy-hyper.git
cd agy-hyper
./install.sh
```

## 3. Verify Installation & Health

Run the built-in environment & conflict diagnostic check:
```bash
agy-hyper doctor
agy-hyper doctor --net   # optional: probe dual-stack (IPv4 vs auto) TLS/TTFB for IPv6 blackholes or stream lag
```

Confirm:
- Core `[OK]` checks pass (Tree-sitter grammars, read-only `gix`, workspace jail & Secret Shield, daemon UDS RTT `<1ms`).
- Review any `[WARN]` findings on competing plugin `PreInvocation`/`PostToolUse` hooks or anti-MCP rules in `~/.gemini/config/plugins/*`, active Antigravity `SKILL.md` counts (`>120`, separating `.git`-bounded `~/.agents/skills` cross-CLI mirrors) or canonical same-target skill aliases, `$HOME`/`/` root workspaces, or pending release updates.
- Switch back to Antigravity and open **AGY Hyper Deck** in the side pane (or launch standalone with `agy-hyper ui`).

## 4. Common Operations

```bash
# Release update radar & SHA-256 verified 1-click upgrade
agy-hyper update --check
agy-hyper update
agy-hyper update --dismiss 0.5.1

# Daemon lifecycle
agy-hyper daemon status
agy-hyper daemon start
agy-hyper daemon stop

# Local burn audit (pre-edit bloat delta, stream EOF retry stalls, split-cache drops & redacted Share Card)
agy-hyper audit --days 14 --share
agy-hyper audit --days 14 --share-png share-card.png --theme dark
agy-hyper audit --days 14 --share-svg share-card.svg --theme light
agy-hyper audit --days 14 --png audit-card.png
agy-hyper audit --days 14 --svg audit-card.svg

# Usage & multi-currency API-equivalent cost ledger (24h | 7d | 30d | 90d)
agy-hyper usage --window 7d
agy-hyper usage --window 30d --currency CNY

# Layered profile & granular MCP / radar switches (observe = cockpit-only zero-intrusion)
agy-hyper config show
agy-hyper config profile observe|safe|balanced|hyper
agy-hyper config toggle enable_mcp_edit false
agy-hyper config toggle enable_update_check true
agy-hyper config currency USD|CNY|JPY|EUR|GBP|HKD|SGD|KRW
agy-hyper config fx CNY 7.25

# WAL undo ledger, inline Unified Diff & atomic rollback
agy-hyper tx list
agy-hyper tx diff <tx_id>
agy-hyper tx rollback <tx_id>

# Waterfall latency benchmark, empirical per-model cache TTL radar & standalone cockpit
agy-hyper bench --sessions 10
agy-hyper bench --cache-ttl
agy-hyper ui --port 47821

# Antigravity CLI native statusLine & plain-text title (optional opt-in)
agy-hyper cli-setup
agy-hyper cli-setup --apply --format compact
```

## 5. Clean Uninstall

### macOS & Linux
```bash
# Keep config, usage ledger, and WAL undo snapshots
curl -fsSL https://raw.githubusercontent.com/BobbyCats/agy-hyper/main/install.sh | bash -s -- --uninstall

# Or remove everything including runtime state and WAL snapshots
curl -fsSL https://raw.githubusercontent.com/BobbyCats/agy-hyper/main/install.sh | bash -s -- --uninstall --purge
```

### Windows (PowerShell)
```powershell
# Keep config, usage ledger, and WAL undo snapshots
$env:AGY_HYPER_UNINSTALL = "1"; irm https://raw.githubusercontent.com/BobbyCats/agy-hyper/main/install.ps1 | iex

# Or purge state as well
$env:AGY_HYPER_UNINSTALL = "1"; $env:AGY_HYPER_PURGE = "1"; irm https://raw.githubusercontent.com/BobbyCats/agy-hyper/main/install.ps1 | iex
```

## 6. Contributor Verification Checklist

After modifying Rust or Sidecar sources:
```bash
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets
node --check plugin/sidecars/hyper_deck/main.mjs \
  plugin/sidecars/hyper_deck/app.js \
  plugin/sidecars/hyper_deck/modules/api.js \
  plugin/sidecars/hyper_deck/modules/charts.js \
  plugin/sidecars/hyper_deck/modules/render.js
cargo build --release && ./install.sh
```
