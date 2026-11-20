<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="./assets/hero-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="./assets/hero-light.svg">
  <img alt="AGY Hyper — Native Rust Acceleration Engine & Telemetry Cockpit for Antigravity" src="./assets/hero-dark.svg" width="100%">
</picture>

<br/>

[![License: MIT](https://img.shields.io/badge/License-MIT-0EA5E9.svg?style=flat-square)](./LICENSE)
[![Rust](https://img.shields.io/badge/Single_Binary-Rust_10MB-111827.svg?style=flat-square&logo=rust&logoColor=white)](#architecture)
[![UDS RTT](https://img.shields.io/badge/Daemon_RTT-0.12ms-16A34A.svg?style=flat-square)](#reproducible-benchmarks)
[![Tests](https://img.shields.io/badge/Unit_%26_Chaos_Tests-29%2F29_Passing-6366F1.svg?style=flat-square)](#architecture)
[![Follow on X](https://img.shields.io/badge/Follow-@BobbyCat2025-000000.svg?style=flat-square&logo=x&logoColor=white)](https://x.com/BobbyCat2025)

**English** · [简体中文](./README.zh-CN.md) · [Contributing](./CONTRIBUTING.md)

</div>

---

You asked your AI agent to trace one auth middleware. It ran `glob`, then `grep`, read half a file that spilled past 10KB to disk, then guessed the next file — five serial round-trips and 38 seconds gone before writing a single line of code. Most of that wait is queuing for five separate model first-token floors.

**AGY Hyper cuts out the dead air.** One ~10MB static Rust engine collapses multi-turn codebase exploration into a single `14ms` cached pack, guards every multi-file edit behind crash-safe WAL 1-click Undo, and docks a live telemetry cockpit right inside Antigravity's side pane.

---

## Understand the Difference in 10 Seconds

Even if you never touch systems programming, three everyday comparisons tell the whole story:

### 1. Speed: Five Trips Back to the Shed vs. Walking In with the Blueprint
- **Without AGY Hyper**: You ask a handyman to fix a leak. He walks to the shed to inspect one pipe (wait 6s), walks back to ask where the wrench is (wait 6s), tries to carry a toolbox that's too heavy and drops it halfway (10KB output spill, wait 6s) — five round-trips and 38 seconds burned riding the elevator before touching a single bolt.
- **With AGY Hyper**: He walks in holding the full floorplan, meter readings, and the exact parts he needs in one hand. One `14ms` pass maps the neighborhood and gets straight to work.

### 2. File Safety: Permanent Marker on the Original vs. Vault Backup + Instant Undo
- **Without AGY Hyper**: Asking an AI to edit 5 files is like editing a signed contract in permanent marker. If page 3 crashes halfway, pages 1 and 2 are already mangled — and an overeager search might peek inside your `.env` secrets drawer.
- **With AGY Hyper**: Original bytes are locked into a local WAL vault first. Edits are drafted on temp scratch paper, syntax-checked in `<1ms`, and swapped in atomically. Bad edit? Hit **Undo** in the side pane and every file snaps back byte-for-byte. Secret files stay sealed by default.

### 3. Long Sessions: Driving Without a Dashboard vs. Full Telemetry Cockpit
- **Without AGY Hyper**: You have no idea when your 60-second prompt cache just went cold or when your context window is about to double in price and stall out — you only find out when the spinner freezes.
- **With AGY Hyper**: A live dashboard sits in Antigravity's side pane showing exact context load, cache warmth countdown, LLM vs. tool bottlenecks, and a 1-click `/compact` button before the stall hits.

---

## Install in One Line

No Rust toolchain, no `ripgrep`, no Python, no `git` CLI required. Release archives are **SHA-256 verified** before atomic swap and never touch `~/.gemini/config/config.json`.

```bash
# macOS & Linux
curl -fsSL https://raw.githubusercontent.com/BobbyCats/agy-hyper/main/install.sh | bash
```

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/BobbyCats/agy-hyper/main/install.ps1 | iex
```

```bash
# Build from source
git clone https://github.com/BobbyCats/agy-hyper.git && cd agy-hyper && ./install.sh
```

Switch back to Antigravity and everything activates automatically. You can also run diagnostics, session waterfalls, or the standalone web cockpit from any terminal:

```bash
agy-hyper doctor   # verify Tree-sitter grammars, gix, daemon RTT & security posture
agy-hyper ui       # launch AGY Hyper Deck standalone at http://127.0.0.1:47821
agy-hyper bench    # compute paired-TPS waterfall telemetry on your recent sessions
```

---

## AGY Hyper Deck (Native Sidecar Cockpit)

Docked directly in Antigravity's auxiliary pane via `SIDECAR_UI_ENTRYPOINT_AUX_PANE`, powered 100% offline by **Apache ECharts 6** and **i18next** (`English / 简体中文 / 日本語`, zero CDN). Luminance-synced with your host Light/Dark theme.

- **Simple Mode** keeps daily essentials front and center: context level, Turn P50, cache hit rate, 60s warmth bar, safety profile, and 1-click WAL Undo.
- **Pro Mode** opens the hood: turn-by-turn execution waterfall, dual-axis context & cache curves, in-memory AST/pack cache hit rates, and live security audit stream.

<div align="center">
<table>
  <tr>
    <td align="center" width="50%">
      <strong>Simple · Context Trajectory &amp; Instant Undo (Dark)</strong><br/>
      <sub>Model-aware guardlines · 60s cache warmth bar · 1-click WAL Undo</sub><br/><br/>
      <img src="./assets/cockpit-simple-dark.png" alt="AGY Hyper Deck Simple Dark" width="100%"/>
    </td>
    <td align="center" width="50%">
      <strong>Simple · Engine Takeover &amp; Cache Hits (Light · 中文)</strong><br/>
      <sub>Hyper vs Standard tool ratio · Live AST &amp; Pack hits · Native i18next</sub><br/><br/>
      <img src="./assets/cockpit-simple-light.png" alt="AGY Hyper Deck Simple Light" width="100%"/>
    </td>
  </tr>
  <tr>
    <td align="center" width="50%">
      <strong>Pro · Turn Execution Waterfall</strong><br/>
      <sub>Disjoint LLM vs Hyper vs Standard tool spans · P50 / P90 · Paired TPS</sub><br/><br/>
      <img src="./assets/cockpit-pro-dark.png" alt="AGY Hyper Deck Pro Waterfall" width="100%"/>
    </td>
    <td align="center" width="50%">
      <strong>Pro · Dual-Axis Context &amp; Cache Efficiency</strong><br/>
      <sub>Cached vs uncached area · Cache hit curve · 180k / 280k cliffs</sub><br/><br/>
      <img src="./assets/cockpit-context-dark.png" alt="AGY Hyper Deck Pro Context" width="100%"/>
    </td>
  </tr>
</table>
</div>

---

## How It Works

<div align="center">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="./assets/mechanics-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="./assets/mechanics-light.svg">
  <img alt="AGY Hyper Core Mechanics — Semantic Packing, 3-Phase Atomic WAL, Zero-Trust Sandbox, Adaptive Telemetry" src="./assets/mechanics-dark.svg" width="100%">
</picture>
</div>

<br/>

1. **Single-Pass Semantic Pack (`hyper_pack`)**: One parallel scan combines embedded `ripgrep`, 7 compiled Tree-sitter grammars (Rust, TypeScript/TSX, JavaScript, Python, Go, JSON), multi-language 1-hop import neighbors, `gix` hotspots, and inline source slices for top matches. Payloads stay strictly `<= 8.5KB` so they never spill past Antigravity's 10KB disk threshold; daemon true-LRU hits return in `14ms`.
2. **3-Phase Atomic WAL (`hyper_edit`)**: Acquires an exclusive `flock` with BLAKE3 ETag CAS and BOM/CRLF preservation, `fsync`s pre-image bytes to the WAL journal, then stages temp files and commits via atomic `rename`. On `ENOSPC` or permission errors, temp files are dropped and live files remain untouched. Every commit runs `<1ms` Tree-sitter syntax verification, and `PostToolUse` logs verified native `edit_file` / `write_file` pre-images into the same 1-click Undo ledger.
3. **Compile-Time Zero-Trust Sandbox**: `gix` is linked with write-ref and network transports compiled out (zero `git` subprocesses). A strict `realpath` jail blocks `../`, symlink escapes, and dangling directory symlinks; Secret Shield blocks `.env`, `*.pem`, `*.key`, and `id_rsa` by default while allowing `.env.example`. `hyper_check` runs with `shell: false`, strips credential env vars, keeps a 64KB head + 512KB tail ring buffer, and kills entire process trees (`setsid` / `taskkill /F /T`) on timeout or cancellation.
4. **Lock-Free Telemetry & Adaptive Guardlines**: Snapshot polling and rollbacks run outside the daemon state mutex with `(mtime, size)`-cached `transcript.jsonl` parsing, so 2.5s dashboard refreshes never stall MCP calls. Context guardlines automatically adapt to the active model: `96k / 115k` (128k), `140k / 175k` (200k), and `180k / 280k` (1M, ahead of the `200k` 2x pricing cliff).

---

## Architecture

<div align="center">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="./assets/architecture-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="./assets/architecture-light.svg">
  <img alt="AGY Hyper System Architecture" src="./assets/architecture-dark.svg" width="100%">
</picture>
</div>

### Five Focused MCP Tools (`<= 8.5KB` Inline Contract)

Compact tool schemas are pre-inlined in `plugin/rules/AGENTS.md` so the agent invokes them immediately without spending a schema-discovery turn.

| Tool | Mode | Purpose |
| :--- | :---: | :--- |
| `hyper_pack` | `read-only` | Single-pass ripgrep + Tree-sitter AST outlines & matched symbol slices + 1-hop imports + `gix` hotspots + BLAKE3 `etag`s. |
| `hyper_search` | `read-only` | Multi-pattern parallel ripgrep, gitignore-aware file globbing, and zero-subprocess `gix` read-only Git inspection. |
| `hyper_edit` | `atomic WAL` | Multi-file exact or Tree-sitter `symbol` edits with BLAKE3 ETag CAS, BOM/CRLF preservation, WAL journal, and `<1ms` syntax check. |
| `hyper_check` | `sandboxed` | Auto-discovered safe presets (`cargo:check`, `pkg:test`, `py:pytest`, `go:vet`) with head+tail ring buffer, Tier-2 pre-warm, and `h_xxxx` spill handles. |
| `hyper_tx` | `undo / drill` | List WAL edit ledger, atomically `rollback` any `tx_id`, or `1ms` slice/grep into spilled check handles. |

---

## Reproducible Benchmarks

Measured on Apple Silicon (release LTO build, cold MCP process spawn included) via `./bench/run-bench.sh`:

| Operation | Measured Wall Time | Notes |
| :--- | ---: | :--- |
| Daemon RPC ping (`agy-hyper daemon status`) | **0.12 ms** | Singleton UDS on Unix · token-authenticated loopback TCP on Windows |
| `hyper_pack` (warm Daemon AST + pack cache HIT) | **14 ms** | Full MCP stdio spawn + initialize + response |
| `hyper_search` + `gix` read-only Git (1 call) | **20 ms** | vs **43 ms** baseline `git status && git log && grep -rn` (**2.2x faster**) |
| `hyper_pack` (cold single-pass ripgrep + AST + slices + `gix` + 1-hop) | **58 ms** | Zero external `rg` or `git` binaries |
| Full Rust unit & security chaos suite (`29/29`) | **0.17 s** | Covers ENOSPC staging, dangling dir symlinks, 4-lang 1-hop, cancellation, BOM/CRLF, verified native WAL |

Reproduce on your own repository:

```bash
./bench/run-bench.sh /path/to/your/repo
```

<details>
<summary><strong>CLI Reference, Telemetry Methodology &amp; Supply-Chain Discipline</strong></summary>

```bash
agy-hyper doctor                        # Tree-sitter, gix, daemon RTT & security self-check
agy-hyper daemon start|stop|status      # Manage the shared local Rust daemon
agy-hyper ui [--port 47821]             # Launch AGY Hyper Deck standalone in browser
agy-hyper bench [--sessions 10] [--svg waterfall.svg] [--json]
agy-hyper tx list                       # Inspect BLAKE3 WAL transaction ledger
agy-hyper tx rollback <tx_id> [--force] # Atomically restore files to pre-edit bytes
agy-hyper config profile safe|balanced|hyper
```

- **Ground-Truth Context & Compaction**: reads live `input_tokens + cache_read_tokens` directly from Antigravity's latest `PLANNER_RESPONSE`, resets counters on `/compact`, and tags transient network events (`proxy_reset`, `stream_timeout`) explicitly.
- **Honest Paired TPS & Disjoint Waterfalls**: `llm_inference_ms` measures planner start -> first tool dispatch; `tool_exec_ms` measures first tool dispatch -> final tool completion. Excluded idle spans (`>180s`) are removed from both numerator and denominator.
- **Supply-Chain Discipline**: zero silent background self-updates; every release ships with `SHA256SUMS` and Sigstore/Cosign build provenance attestation.

</details>

---

## Contributing & Acknowledgments

- **MIT Licensed & PRs Welcome**: AGY Hyper is 100% open source under the **MIT License** — free for personal and commercial use on any work machine. Contributions for additional Tree-sitter grammars, preset runners, translations, and Windows/Linux testing are very welcome; see [CONTRIBUTING.md](./CONTRIBUTING.md) to get started.
- **Acknowledgments**: Initial exploration into Antigravity latency optimization was sparked by [INK-Q/agy-fast](https://github.com/INK-Q/agy-fast). Big thanks to the open-source community for pushing AI tooling forward.
- **Stay Connected**: Built in public by [@BobbyCat2025](https://x.com/BobbyCat2025). Follow on X for Rust AI tooling, Antigravity workflow breakdowns, and release updates: [x.com/BobbyCat2025](https://x.com/BobbyCat2025).

---

MIT © [BobbyCats](https://github.com/BobbyCats) & AGY Hyper Contributors.
