# AGY Hyper — Zero-Tax Native Acceleration Protocol

You have the `agy-hyper` native Rust MCP engine active (sub-millisecond local UDS daemon, Tree-sitter AST, pure-Rust `gix` read-only Git, crash-safe WAL atomic edits, strict workspace jail, and live Sidecar Cockpit).

## Turn-Reduction Routing Rules

1. **Explore in 1 turn, never 5**: Start unfamiliar codebase exploration with `hyper_pack` (combines single-pass ripgrep + Tree-sitter AST outlines + inline source slices for top matched symbols + 1-hop import neighbors + `gix` git signals in one ≤8.5KB inline payload).
2. **Hybrid Read Strategy (Anti-Blanket-Read)**:
   - Use `hyper_pack` / `hyper_search` first to locate symbols, line ranges, and `etag`s before opening files.
   - Use built-in `read_file` with explicit `StartLine`/`EndLine` on files >200 lines; avoid launching >3 speculative full-file `read_file` calls in one turn, and bound noisy shell output so context stays lean before your first edit.
3. **Use `hyper_edit` When Enabled by the Active Profile**:
   - In `Balanced` / `Hyper` modes (when `hyper_edit` is enabled in `tools/list`), prefer `hyper_edit` for workspace source/config edits with `expected_etag` or `symbol` for crash-safe BLAKE3 WAL snapshots and `<1ms` syntax verification. In `Observe` / `Safe` modes, use native editing tools cleanly to preserve Antigravity's native inline diffs (verified native edits are still passively captured into the WAL ledger via `PostToolUse`).
   - Reserve built-in `write_file` / `edit_file` for Antigravity artifact files outside the workspace or binary files.
   - To inspect or undo any edit cleanly, call `hyper_tx(action="rollback", tx_id="...")` or expand the inline Diff drawer in Hyper Deck.
4. **Cache-Preserving Sync Verification (`hyper_check`)**:
   - When `hyper_check` is enabled, prefer `hyper_check(preset="...")` or `hyper_check(argv=[...])` for builds, typechecks, and tests up to 60s instead of `run_command` (which backgrounds past 10s and risks prompt-cache expiry on wake-up).
5. **Context, Subagent & Cache Hygiene**:
   - Respect `[AGY Hyper Guard]` / `[AGY Hyper Redline Guard]` watermark signals: offload broad multi-file scans to `research` subagents and suggest `/compact` when approaching 280k tokens.
   - Avoid spawning short-lived subagents for targeted lookups that `hyper_pack` or `hyper_search` resolves in 1 warm-cache turn. Reuse an existing subagent via `send_message` only for close, warm follow-ups: once a `self` subagent exceeds ~250k context or has been idle past the model's live cache window (subagents cannot run `/compact`), do not wake it—spawn a fresh subagent with a compact handoff summary instead.
   - When the user asks to view live telemetry, cache warmth, subagent status, or rollback history, surface `[Open Hyper Deck](sidecar://agy-hyper/hyper_deck/)`.

## Compact Tool Schemas (Inline — No Extra Schema-Read Turn Required)

When tools are lazy-loaded by Antigravity, invoke directly via `call_mcp_tool(ServerName="agy-hyper_core", ToolName="...", Arguments={...})` — no separate schema-file read needed. Always pass the explicit absolute `workspace` project root path for workspace-scoped tools (`hyper_pack`, `hyper_search`, `hyper_edit`, `hyper_check`):

- `hyper_pack`: `{ workspace?, query?, focus_paths?: string[], include_outlines?: true, include_git?: true, max_bytes?: 8200 }`
- `hyper_search`: `{ workspace?, queries?: [{ pattern, path?, glob?, fixed_strings?, case_sensitive?, max_matches? }], glob?, path?, include_git?: false }`
- `hyper_edit`: `{ workspace?, edits: [{ path, old_text?, new_text?, symbol?, create_content?, expected_etag?, replace_all? }] }`
- `hyper_check`: `{ workspace?, list_presets?: false, preset?, argv?: string[], timeout_secs?: 60, idle_timeout_secs?, fail_fast?, cwd? }`
- `hyper_tx`: `{ action: "list"|"rollback"|"drill", tx_id?, force?, handle_id?, offset_lines?, limit_lines?, grep?, limit? }`
