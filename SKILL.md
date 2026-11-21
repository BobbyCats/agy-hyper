---
name: agy-hyper
description: Install, verify, configure, inspect usage/telemetry, rollback WAL file edits, or cleanly uninstall AGY Hyper (native Rust acceleration engine, WAL atomic edits, subagent tree monitor, multi-currency usage ledger, and Hyper Deck sidecar cockpit for Antigravity) on macOS, Linux, and Windows.
---

# Agent Operations Guide: AGY Hyper

Use this guide when a user asks you to install, upgrade, verify, configure, troubleshoot, or uninstall **AGY Hyper** (`BobbyCats/agy-hyper`).

> **Architecture & Safety Guarantees**:
> - Uses **100% official Antigravity plugin protocols** (MCP, Hooks, Rules, `SIDECAR_UI_ENTRYPOINT_AUX_PANE`). Does **not** inject into Electron DOM, open CDP debugging ports, or modify `~/.gemini/config/config.json`.
> - Single ~10MB static Rust binary (`agy-hyper`); zero Rust/Python/`ripgrep`/`git` runtime dependencies required for release installs.
> - Rust core is strictly **offline** (zero background network calls). Optional FX sync only triggers when a user manually clicks **Sync FX** in Hyper Deck.

## 1. Key Paths

| Component | Path |
| :--- | :--- |
| Installed plugin bundle | `~/.gemini/config/plugins/agy-hyper` |
| CLI symlink (macOS/Linux) | `~/.local/bin/agy-hyper` |
| Runtime state, config, ledger & WAL | `~/.gemini/antigravity/agy-hyper` |
| User configuration | `~/.gemini/antigravity/agy-hyper/config.json` |
| Incremental usage ledger | `~/.gemini/antigravity/agy-hyper/usage_ledger.json` |
| BLAKE3 WAL undo snapshots | `~/.gemini/antigravity/agy-hyper/wal/` |
| Local UDS socket & PIDs | `~/.gemini/antigravity/agy-hyper/run/` |

## 2. Install or Upgrade

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

Run the built-in diagnostic check:
```bash
agy-hyper doctor
```

Confirm:
- All `[OK]` checks pass (Tree-sitter grammars, read-only `gix`, workspace jail & Secret Shield, daemon UDS RTT `<1ms`).
- Switch back to Antigravity and open **AGY Hyper Deck** in the side pane (or launch standalone with `agy-hyper ui`).

## 4. Common Operations

```bash
# Daemon lifecycle
agy-hyper daemon status
agy-hyper daemon start
agy-hyper daemon stop

# Usage & multi-currency API-equivalent cost ledger (24h | 7d | 30d | 90d)
agy-hyper usage --window 7d
agy-hyper usage --window 30d --currency CNY

# Safety profile & currency configuration
agy-hyper config show
agy-hyper config profile safe|balanced|hyper
agy-hyper config currency USD|CNY|JPY|EUR|GBP|HKD|SGD|KRW
agy-hyper config fx CNY 7.25

# WAL undo ledger & atomic rollback
agy-hyper tx list
agy-hyper tx rollback <tx_id>

# Waterfall latency benchmark & standalone cockpit
agy-hyper bench --sessions 10
agy-hyper ui --port 47821
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
