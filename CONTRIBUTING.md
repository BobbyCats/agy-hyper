# Contributing to AGY Hyper

PRs, bug reports, platform tests, and benchmark reproductions are warmly welcome.

## Development Setup

Requirements: Rust stable toolchain (`1.80+`) and Node.js (`20+`, for checking sidecar syntax).

```bash
git clone https://github.com/BobbyCats/agy-hyper.git
cd agy-hyper
cargo test --workspace --all-targets
cargo build --release
./install.sh
```

## Repository Layout

- `crates/hyper-core/` — Pure library: `jail` (workspace confinement + Secret Shield), `wal` + `tx_edit` + `native_capture` (atomic edits & rollback), `ast` + `search` + `git` + `context_pack` (semantic packing), `runner` + `diag` (shell-free checks), `transcript` + `sessions` + `bench` (telemetry).
- `crates/hyper-daemon/` — Shared local daemon (UDS on Unix, token-authenticated loopback TCP on Windows) with lock-free snapshot I/O, LRU AST/pack cache, and FSEvents/inotify watcher.
- `crates/hyper-cli/` — Single `agy-hyper` binary (`mcp`, `hook`, `daemon`, `bench`, `doctor`, `tx`, `config`, `snapshot`, `ui`).
- `plugin/` — Antigravity plugin bundle (`plugin.json`, `mcp_config.json`, `hooks.json`, `rules/AGENTS.md`, and the zero-npm native Sidecar UI under `sidecars/hyper_deck/`).

## Great First PRs

- **Tree-sitter grammars**: C/C++, Java, Swift, Kotlin, or Ruby support in `crates/hyper-core/src/ast.rs` + 1-hop import rules in `context_pack.rs`.
- **Safe check presets**: Additional auto-discovered presets in `crates/hyper-core/src/runner.rs` (e.g., `swift:test`, `zig:build`, `uv:pytest`).
- **Localizations**: New language JSON bundles under `plugin/sidecars/hyper_deck/locales/`.
- **Cross-platform verification**: Real-machine testing on Linux (musl/glibc) and Windows PowerShell.

## Before Opening a PR

```bash
cargo test --workspace --all-targets
node --check plugin/sidecars/hyper_deck/main.mjs
node --check plugin/sidecars/hyper_deck/app.js
node --check plugin/sidecars/hyper_deck/modules/*.js
bash -n install.sh
```

Keep dependencies minimal, preserve the `<= 8.5KB` inline MCP payload contract, and add a focused unit test for every bug fix or security boundary.
