#!/usr/bin/env bash
set -euo pipefail

# AGY Hyper — Reproducible Local Micro & Workflow Benchmark
# Usage: ./bench/run-bench.sh [workspace_path]

TARGET_WS="${1:-$(pwd)}"
BIN="${AGY_HYPER_BIN:-}"
if [[ -z "$BIN" ]]; then
  BIN="$(command -v agy-hyper || true)"
fi
if [[ -z "$BIN" ]]; then
  BIN="./target/release/agy-hyper"
fi

if [[ ! -x "$BIN" ]]; then
  echo "agy-hyper binary not found. Build with: cargo build --release"
  exit 1
fi

now_ms() {
  python3 -c 'import time; print(int(time.perf_counter() * 1000))'
}

echo ""
echo "⚡ AGY HYPER · REPRODUCIBLE LOCAL BENCHMARK"
echo "  Workspace : $TARGET_WS"
echo "  Binary    : $BIN"
echo "─────────────────────────────────────────────────────────────────"

# 1. Cold vs Daemon-Cached hyper_pack over real MCP stdio
T0="$(now_ms)"
"$BIN" mcp >/dev/null <<EOF
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"hyper_pack","arguments":{"workspace":"${TARGET_WS}","query":"config"}}}
EOF
T1="$(now_ms)"

T2="$(now_ms)"
"$BIN" mcp >/dev/null <<EOF
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"hyper_pack","arguments":{"workspace":"${TARGET_WS}","query":"config"}}}
EOF
T3="$(now_ms)"

# 2. Pure-Rust hyper_search (embedded ripgrep + gix) vs system git+grep
T4="$(now_ms)"
"$BIN" mcp >/dev/null <<EOF
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"hyper_search","arguments":{"workspace":"${TARGET_WS}","queries":[{"pattern":"fn ","fixed_strings":true,"max_matches":20}],"include_git":true}}}
EOF
T5="$(now_ms)"

T6="$(now_ms)"
(cd "$TARGET_WS" && git status --porcelain >/dev/null 2>&1 || true; git log -n 8 --oneline >/dev/null 2>&1 || true; grep -rn "fn " . --exclude-dir=.git --exclude-dir=target --exclude-dir=node_modules 2>/dev/null | head -n 20 >/dev/null || true)
T7="$(now_ms)"

# 3. Direct UDS Daemon ping RTT
"$BIN" daemon status

printf '\n| Benchmark (Cold Process Spawn Included) | Wall Time |\n'
printf '| :--- | ---: |\n'
printf '| `hyper_pack` (cold AST + ripgrep + gix + 1-hop) | **%d ms** |\n' "$((T1 - T0))"
printf '| `hyper_pack` (warm Daemon in-memory cache HIT) | **%d ms** |\n' "$((T3 - T2))"
printf '| `hyper_search` + `gix` read-only git (1 call) | **%d ms** |\n' "$((T5 - T4))"
printf '| Baseline `git status && git log && grep -rn` | %d ms |\n\n' "$((T7 - T6))"
