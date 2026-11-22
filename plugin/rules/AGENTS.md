# AGY Hyper — Zero-Tax Native Acceleration Protocol

You have the `agy-hyper` native Rust MCP engine active (sub-millisecond local UDS daemon, Tree-sitter AST, pure-Rust `gix` read-only Git, crash-safe WAL atomic edits, strict workspace jail, and live Sidecar Cockpit).

## Turn-Reduction Routing Rules

1. **Explore in 1 turn, never 5**: Start unfamiliar codebase exploration with `hyper_pack` (combines single-pass ripgrep + Tree-sitter AST outlines + inline source slices for top matched symbols + 1-hop import neighbors + `gix` git signals in one ≤8.5KB inline payload).
2. **Hybrid Read Strategy (Anti-Blanket-Read)**:
   - Use `hyper_pack` / `hyper_search` first to locate symbols, line ranges, and `etag`s before opening files.
   - Use built-in `read_file` with explicit `StartLine`/`EndLine` on files >200 lines; avoid launching >3 speculative full-file `read_file` calls in one turn, and bound noisy shell output so context stays lean before your first edit.
3. **Default to `hyper_edit` for Workspace File Edits**:
   - Use `hyper_edit` as your default editor for source and config files inside the workspace (both single-file and multi-file edits): pass `expected_etag` for CAS safety or `symbol` for surgical AST block replacement. Every commit writes a crash-safe BLAKE3 WAL snapshot (`tx_id`) for 1-click Undo and returns inline `<1ms` Tree-sitter syntax verification.
   - Reserve built-in `write_file` / `edit_file` for Antigravity artifact files outside the workspace or binary files (verified native workspace edits are also auto-captured into the WAL ledger via `PostToolUse`).
   - To undo any edit cleanly, call `hyper_tx(action="rollback", tx_id="...")`.
4. **Cache-Preserving Sync Verification (`hyper_check`)**:
   - For builds, typechecks, and tests taking up to 60s, prefer `hyper_check(preset="...")` (e.g. `cargo:check`, `pkg:test`, `pkg:<pkg>:test`, `py:pytest`) or `hyper_check(argv=[...])` over `run_command`.
   - `run_command` backgrounds commands past 10s; yielding a turn to wait for background completion risks prompt-cache resets on wake-up. `hyper_check` stays synchronous inside the active tool loop, preserves head+tail fatal errors without dropping stderr, and stores oversized logs in a 1ms drill handle (`h_xxxx`).
5. **Context, Subagent & Cache Hygiene**:
   - Respect `[AGY Hyper Guard]` / `[AGY Hyper Redline Guard]` watermark signals: offload broad multi-file scans to `research` subagents and suggest `/compact` when approaching 280k tokens.
   - Avoid spawning short-lived subagents for targeted lookups that `hyper_pack` or `hyper_search` resolves in 1 warm-cache turn. Reuse an existing subagent via `send_message` only for close, warm follow-ups: once a `self` subagent exceeds ~250k context or has been idle past the 4-minute cache window (subagents cannot run `/compact`), do not wake it—spawn a fresh subagent with a compact handoff summary instead.
   - When the user asks to view live telemetry, cache warmth, subagent status, or rollback history, surface `[Open Hyper Deck](sidecar://agy-hyper/hyper_deck/)`.

## Compact Tool Schemas (Inline — No Extra Schema-Read Turn Required)

When tools are lazy-loaded by Antigravity, invoke directly via `call_mcp_tool(ServerName="agy-hyper_core", ToolName="...", Arguments={...})` — no separate schema-file read needed:

- `hyper_pack`: `{ workspace?, query?, focus_paths?: string[], include_outlines?: true, include_git?: true, max_bytes?: 8200 }`
- `hyper_search`: `{ workspace?, queries?: [{ pattern, path?, glob?, fixed_strings?, case_sensitive?, max_matches? }], glob?, path?, include_git?: false }`
- `hyper_edit`: `{ workspace?, edits: [{ path, old_text?, new_text?, symbol?, create_content?, expected_etag?, replace_all? }] }`
- `hyper_check`: `{ workspace?, list_presets?: false, preset?, argv?: string[], timeout_secs?: 60, idle_timeout_secs?, fail_fast?, cwd? }`
- `hyper_tx`: `{ action: "list"|"rollback"|"drill", tx_id?, force?, handle_id?, offset_lines?, limit_lines?, grep?, limit? }`
