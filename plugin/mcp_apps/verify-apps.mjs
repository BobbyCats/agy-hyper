#!/usr/bin/env node
/**
 * Zero-dependency Node.js smoke & SEP-1865 protocol verification suite
 * for AGY Hyper MCP Apps (`tx-diff.html` and `cockpit.html`).
 */

import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const MAX_APP_BYTES = 1024 * 1024; // 1 MiB ceiling

function extractSingleTag(html, tag) {
  const re = new RegExp(`<${tag}[^>]*>([\\s\\S]*?)<\\/${tag}>`, "gi");
  const matches = [...html.matchAll(re)];
  assert.equal(matches.length, 1, `Expected exactly 1 <${tag}> block, found ${matches.length}`);
  return matches[0][1];
}

function verifyStaticHygiene(filename) {
  const filePath = path.join(__dirname, filename);
  assert.ok(fs.existsSync(filePath), `${filename} must exist`);
  const html = fs.readFileSync(filePath, "utf8");
  const bytes = Buffer.byteLength(html, "utf8");
  assert.ok(bytes > 1024, `${filename} looks unexpectedly small (${bytes}B)`);
  assert.ok(bytes < MAX_APP_BYTES, `${filename} exceeds 1 MiB (${bytes}B)`);
  assert.ok(/^<!DOCTYPE html>/i.test(html.trim()), `${filename} must start with <!DOCTYPE html>`);
  assert.ok(/<\/html>\s*$/i.test(html), `${filename} must end with </html>`);

  const css = extractSingleTag(html, "style");
  const js = extractSingleTag(html, "script");

  // Verify zero external network resource references in HTML/CSS
  const htmlNoScript = html.replace(/<script[\s\S]*?<\/script>/gi, "");
  assert.ok(
    !/(?:src|href)\s*=\s*["']https?:\/\//i.test(htmlNoScript),
    `${filename} must not reference external http(s) src/href`
  );
  assert.ok(!/@import\s+/i.test(css), `${filename} CSS must not contain @import`);
  assert.ok(!/url\(\s*["']?https?:\/\//i.test(css), `${filename} CSS must not load remote url()`);

  // Validate inline JS syntax without executing eval
  new vm.Script(js, { filename });

  return { html, css, js, bytes };
}

function verifySourceHygiene() {
  const srcDir = path.join(__dirname, "src");
  for (const name of fs.readdirSync(srcDir)) {
    const raw = fs.readFileSync(path.join(srcDir, name), "utf8");
    const codeOnly = raw
      .replace(/\/\*[\s\S]*?\*\//g, "")
      .replace(/^\s*\/\/.*$/gm, "");
    assert.ok(
      !/\b(localStorage|sessionStorage|indexedDB|document\.cookie)\b/.test(codeOnly),
      `src/${name} must not rely on browser storage`
    );
    assert.ok(
      !/\b(fetch\s*\(|new\s+WebSocket\s*\()/.test(codeOnly),
      `src/${name} must not make direct fetch/WebSocket network calls`
    );
    assert.ok(
      !/\/Users\/|C:\\Users\\/i.test(raw),
      `src/${name} must not contain personal filesystem paths`
    );
  }
}

/**
 * Minimal DOM + postMessage host harness to execute real bundled HTML scripts
 * and drive SEP-1865 JSON-RPC 2.0 handshakes, notifications, and clicks.
 */
function createDomAndHostHarness({ js, filename, hostCapabilities, hostContext, onHostRequest }) {
  const messageListeners = [];
  const sentToHost = [];
  const intervals = [];
  const timeouts = [];

  class FakeElement {
    constructor(tagName, id = "") {
      this.tagName = tagName.toUpperCase();
      this.id = id;
      this.attrs = new Map();
      this.classSet = new Set();
      this.styleProps = new Map();
      this._innerHTML = "";
      this.textContent = "";
      this.listeners = new Map();
      this.scrollHeight = 420;
      this.style = new Proxy(
        {},
        {
          set: (_t, prop, val) => {
            this.styleProps.set(String(prop), String(val));
            return true;
          },
          get: (_t, prop) => {
            if (prop === "setProperty") {
              return (k, v) => this.styleProps.set(k, String(v));
            }
            if (prop === "getPropertyValue") {
              return (k) => this.styleProps.get(k) || "";
            }
            return this.styleProps.get(String(prop)) || "";
          },
        }
      );
      this.classList = {
        add: (...c) => c.forEach((x) => this.classSet.add(x)),
        remove: (...c) => c.forEach((x) => this.classSet.delete(x)),
        contains: (c) => this.classSet.has(c),
        toggle: (c, force) => {
          const on = force === undefined ? !this.classSet.has(c) : Boolean(force);
          if (on) this.classSet.add(c);
          else this.classSet.delete(c);
        },
      };
    }
    set innerHTML(val) {
      this._innerHTML = String(val ?? "");
    }
    get innerHTML() {
      return this._innerHTML;
    }
    setAttribute(k, v) {
      this.attrs.set(k, String(v));
    }
    getAttribute(k) {
      return this.attrs.has(k) ? this.attrs.get(k) : null;
    }
    addEventListener(type, fn) {
      if (!this.listeners.has(type)) this.listeners.set(type, []);
      this.listeners.get(type).push(fn);
    }
    removeEventListener(type, fn) {
      const list = this.listeners.get(type);
      if (!list) return;
      const idx = list.indexOf(fn);
      if (idx !== -1) list.splice(idx, 1);
    }
    dispatchClick(attrName, attrValue) {
      const target = {
        closest(selector) {
          const m = selector.match(/^\[([a-z0-9-]+)\]$/i);
          if (m && m[1] === attrName) {
            return {
              getAttribute: (k) => (k === attrName ? String(attrValue) : null),
            };
          }
          return null;
        },
      };
      for (const fn of this.listeners.get("click") || []) {
        fn({ target });
      }
    }
    appendChild() {}
    remove() {}
    select() {}
    getBoundingClientRect() {
      return { width: 640, height: this.scrollHeight };
    }
  }

  const documentElement = new FakeElement("html");
  documentElement.scrollHeight = 1480; // Exceeds 800px to verify inline cap
  const head = new FakeElement("head");
  const body = new FakeElement("body");
  body.scrollHeight = 1480;
  const appEl = new FakeElement("div", "app");
  const toastEl = new FakeElement("div", "toast");
  toastEl.classList.add("hidden");

  const elementsById = new Map([
    ["app", appEl],
    ["toast", toastEl],
  ]);

  const hostParent = {
    postMessage(msg) {
      sentToHost.push(msg);
      if (msg && msg.jsonrpc === "2.0" && msg.id !== undefined && msg.method) {
        queueMicrotask(async () => {
          try {
            if (msg.method === "ui/initialize") {
              deliverFromHost({
                jsonrpc: "2.0",
                id: msg.id,
                result: {
                  protocolVersion: "2026-01-26",
                  hostInfo: { name: "antigravity-test-host", version: "1.0.0" },
                  hostCapabilities,
                  hostContext,
                },
              });
              return;
            }
            const reply = await onHostRequest(msg);
            if (reply && reply.error) {
              deliverFromHost({ jsonrpc: "2.0", id: msg.id, error: reply.error });
            } else {
              deliverFromHost({ jsonrpc: "2.0", id: msg.id, result: reply ?? {} });
            }
          } catch (err) {
            deliverFromHost({
              jsonrpc: "2.0",
              id: msg.id,
              error: { code: -32603, message: String(err?.message || err) },
            });
          }
        });
      }
    },
  };

  function deliverFromHost(data) {
    const ev = { data, source: hostParent, origin: "null" };
    for (const fn of [...messageListeners]) {
      fn(ev);
    }
  }

  const quietConsole = {
    log: () => {},
    debug: () => {},
    info: () => {},
    warn: (...a) => console.warn(...a),
    error: (...a) => console.error(...a),
  };

  const sandbox = {
    console: quietConsole,
    URL,
    Date,
    Math,
    JSON,
    Map,
    Set,
    WeakMap,
    WeakSet,
    Promise,
    Error,
    TypeError,
    RegExp,
    Array,
    Object,
    String,
    Number,
    Boolean,
    Symbol,
    Intl,
    Uint8Array,
    TextEncoder,
    TextDecoder,
    AbortController,
    AbortSignal,
    queueMicrotask,
    setTimeout: (fn, ms) => {
      const id = setTimeout(fn, ms);
      timeouts.push(id);
      return id;
    },
    clearTimeout: (id) => clearTimeout(id),
    setInterval: (fn, ms) => {
      const id = setInterval(fn, ms);
      intervals.push(id);
      return id;
    },
    clearInterval: (id) => clearInterval(id),
    requestAnimationFrame: (fn) => {
      const id = setTimeout(() => fn(Date.now()), 0);
      timeouts.push(id);
      return id;
    },
    ResizeObserver: class {
      observe() {}
      disconnect() {}
    },
    navigator: {
      language: hostContext?.locale || "en-US",
      clipboard: {
        writeText: async () => {},
      },
    },
    document: {
      documentElement,
      head,
      body,
      getElementById: (id) => elementsById.get(id) || null,
      createElement: (tag) => new FakeElement(tag),
      execCommand: () => true,
    },
  };

  sandbox.window = {
    parent: hostParent,
    innerWidth: 640,
    innerHeight: 600,
    addEventListener(type, fn) {
      if (type === "message") messageListeners.push(fn);
    },
    removeEventListener(type, fn) {
      if (type === "message") {
        const idx = messageListeners.indexOf(fn);
        if (idx !== -1) messageListeners.splice(idx, 1);
      }
    },
    document: sandbox.document,
    navigator: sandbox.navigator,
  };
  sandbox.self = sandbox.window;
  sandbox.globalThis = sandbox;

  // Enforce strict CSP runtime parity (forbid eval / new Function / wasm codegen)
  const ctx = vm.createContext(sandbox, {
    codeGeneration: { strings: false, wasm: false },
  });
  vm.runInContext(js, ctx, { filename });

  return {
    appEl,
    toastEl,
    documentElement,
    sentToHost,
    deliverFromHost,
    cleanup() {
      intervals.forEach((id) => clearInterval(id));
      timeouts.forEach((id) => clearTimeout(id));
    },
  };
}

const tick = (ms = 25) => new Promise((r) => setTimeout(r, ms));

async function testTxDiff(js) {
  let togglePaneRejectWith32601 = false;
  const h = createDomAndHostHarness({
    js,
    filename: "tx-diff.html",
    hostCapabilities: { serverTools: {} },
    hostContext: {
      theme: "dark",
      locale: "en-US",
      containerDimensions: { maxHeight: 800 },
      styles: {
        variables: {
          "--color-background-primary": "#0b0e14",
          "--color-text-primary": "#f1f5f9",
        },
      },
    },
    onHostRequest(msg) {
      if (msg.method === "tools/call") {
        const { name, arguments: args } = msg.params;
        if (name === "hyper_app_tx_diff") {
          return {
            content: [{ type: "text", text: "diff" }],
            structuredContent: {
              tx_id: args.tx_id,
              diff_text: [
                "--- a/src/main.rs",
                "+++ b/src/main.rs",
                "@@ -10,3 +10,4 @@",
                " fn main() {",
                "-    old_call();",
                "+    new_call();",
                "+    audit_ok();",
                " }",
              ].join("\n"),
              files: [{ path: "src/main.rs", abs_path: "/workspace/src/main.rs", added: 2, removed: 1 }],
            },
          };
        }
        if (name === "hyper_app_tx_rollback") {
          return {
            content: [{ type: "text", text: "rolled back" }],
            structuredContent: {
              ok: true,
              tx_id: args.tx_id,
              message: "Restored 1 file atomically.",
              restored_files: ["src/main.rs"],
            },
          };
        }
      }
      if (msg.method === "com.google.antigravity/toggle-aux-pane") {
        if (togglePaneRejectWith32601) {
          return { error: { code: -32601, message: "Method not found" } };
        }
        return { isError: false };
      }
      return {};
    },
  });

  try {
    await tick(30);
    assert.ok(h.sentToHost.some((m) => m.method === "ui/initialize"), "tx-diff must send ui/initialize");
    assert.ok(
      h.sentToHost.some((m) => m.method === "ui/notifications/initialized"),
      "tx-diff must send ui/notifications/initialized"
    );
    assert.equal(h.documentElement.getAttribute("data-theme"), "dark");
    assert.equal(h.documentElement.style.getPropertyValue("--color-background-primary"), "#0b0e14");

    // Tool input loading state
    h.deliverFromHost({
      jsonrpc: "2.0",
      method: "ui/notifications/tool-input",
      params: { arguments: { action: "list" } },
    });
    await tick(10);
    assert.ok(h.appEl.innerHTML.includes("Loading WAL transactions"), "tx-diff must render loading state");

    // Tool result without preloaded diff -> should auto-call hyper_app_tx_diff
    h.deliverFromHost({
      jsonrpc: "2.0",
      method: "ui/notifications/tool-result",
      params: {
        content: [{ type: "text", text: "2 transactions" }],
        structuredContent: {
          action: "list",
          workspace: "/workspace/demo-repo",
          transactions: [
            {
              tx_id: "tx-1001",
              created_at: new Date().toISOString(),
              source: "hyper_edit",
              summary: "Refactor main call",
              files: [{ path: "src/main.rs", abs_path: "/workspace/src/main.rs", added: 2, removed: 1 }],
              rolled_back: false,
            },
            {
              tx_id: "tx-1000",
              created_at: new Date(Date.now() - 120_000).toISOString(),
              source: "native",
              summary: "Update config",
              files: [{ path: "Cargo.toml", abs_path: "/workspace/Cargo.toml", added: 1, removed: 0 }],
              rolled_back: false,
            },
          ],
          selected_tx_id: "tx-1001",
        },
      },
    });
    await tick(40);

    const diffCall = h.sentToHost.find(
      (m) => m.method === "tools/call" && m.params?.name === "hyper_app_tx_diff"
    );
    assert.ok(diffCall, "tx-diff must auto-call hyper_app_tx_diff when diff not in initial payload");
    assert.equal(diffCall.params.arguments.tx_id, "tx-1001");
    assert.ok(h.appEl.innerHTML.includes("diff-row-add"), "tx-diff must render added diff lines");
    assert.ok(h.appEl.innerHTML.includes("diff-row-del"), "tx-diff must render deleted diff lines");
    assert.ok(h.appEl.innerHTML.includes("new_call();"), "tx-diff must render unified diff text");

    // Verify size-changed emitted and capped <= 800
    const sizeMsg = h.sentToHost.find((m) => m.method === "ui/notifications/size-changed");
    assert.ok(sizeMsg, "tx-diff must emit ui/notifications/size-changed");
    assert.ok(sizeMsg.params.height <= 800, `size-changed height (${sizeMsg.params.height}) must be <= 800`);

    // Click file button -> com.google.antigravity/toggle-aux-pane
    h.appEl.dispatchClick("data-open-file", "/workspace/src/main.rs");
    await tick(20);
    const paneReq = h.sentToHost.find((m) => m.method === "com.google.antigravity/toggle-aux-pane");
    assert.ok(paneReq, "tx-diff must send com.google.antigravity/toggle-aux-pane on file click");
    assert.equal(paneReq.params.tabType, "fileView");
    assert.equal(paneReq.params.uri, "file:///workspace/src/main.rs");

    // Graceful -32601 ignore
    togglePaneRejectWith32601 = true;
    h.appEl.dispatchClick("data-open-file", "/workspace/src/main.rs");
    await tick(20);

    // Two-step rollback confirmation -> hyper_app_tx_rollback
    h.appEl.dispatchClick("data-arm-rb", "tx-1001");
    await tick(10);
    assert.ok(h.appEl.innerHTML.includes("data-confirm-rb"), "Step 1 rollback must show confirmation step");
    h.appEl.dispatchClick("data-confirm-rb", "tx-1001");
    await tick(30);
    const rbCall = h.sentToHost.find(
      (m) => m.method === "tools/call" && m.params?.name === "hyper_app_tx_rollback"
    );
    assert.ok(rbCall, "Step 2 rollback must call hyper_app_tx_rollback");
    assert.equal(rbCall.params.arguments.tx_id, "tx-1001");
    assert.ok(h.appEl.innerHTML.includes("Rolled Back"), "UI must update live to Rolled Back");
    assert.ok(h.appEl.innerHTML.includes("Restored 1 file atomically."), "UI must show rollback banner");

    // Live theme & locale switch via host-context-changed
    h.deliverFromHost({
      jsonrpc: "2.0",
      method: "ui/notifications/host-context-changed",
      params: { theme: "light", locale: "zh-CN" },
    });
    await tick(20);
    assert.equal(h.documentElement.getAttribute("data-theme"), "light");
    assert.ok(h.appEl.innerHTML.includes("WAL 事务 Diff 与回滚"), "tx-diff must switch locale to zh");

    // Tool cancelled notification
    h.deliverFromHost({
      jsonrpc: "2.0",
      method: "ui/notifications/tool-cancelled",
      params: { reason: "user" },
    });
    await tick(10);
    assert.ok(h.appEl.innerHTML.includes("事务查询已取消"));
  } finally {
    h.cleanup();
  }
}

function sampleCockpitData(window = "7d") {
  return {
    version: "0.6.0",
    profile: "balanced",
    workspace: "/workspace/agy-hyper",
    health: {
      level: "warming",
      context_tokens: 148200,
      context_limit: 200000,
      cache_hit_pct: 84.6,
      cache_ttl_secs: 300,
      cache_ttl_source: "empirical",
      cache_age_secs: 54,
      active_model: "claude-sonnet-4-5",
    },
    subagents: [
      {
        id: "sub-stall-01",
        role: "verifier",
        state: "stalled",
        stalled: true,
        crash_loop_count: 0,
        context_tokens: 62000,
        rescue_prompt: "Inspect stalled verifier #sub-stall-01 and restart remaining check.",
      },
      {
        id: "sub-loop-02",
        role: "builder",
        state: "error_loop",
        stalled: false,
        crash_loop_count: 4,
        context_tokens: 91500,
        rescue_prompt: "Stop crash-looping builder #sub-loop-02 after 4 failures.",
      },
    ],
    usage: {
      window,
      total_input: 980000,
      total_cache_read: 6420000,
      total_output: 245000,
      est_cost_usd: 11.42,
      currency: "USD",
      daily: [
        { date: "2026-11-21", tokens: 1200000, cache_hit_pct: 79.0, cost_usd: 2.1 },
        { date: "2026-11-22", tokens: 1950000, cache_hit_pct: 85.4, cost_usd: 3.4 },
        { date: "2026-11-23", tokens: 2400000, cache_hit_pct: 88.1, cost_usd: 4.12 },
        { date: "2026-11-24", tokens: 1095000, cache_hit_pct: 82.0, cost_usd: 1.8 },
      ],
    },
    audit: {
      grade: "WATCH",
      cache_hit_pct: 84.6,
      first_edit_p50_tokens: 64000,
      split_cache_drop_pct: 18.2,
      top_findings: [
        "Split-session handoffs dropped prompt cache by 18.2%.",
        "First-edit context P50 stayed lean at 64k tokens.",
      ],
    },
    recent_txs: [
      {
        tx_id: "tx-204",
        created_at: new Date().toISOString(),
        source: "hyper_edit",
        summary: "Add MCP resources handler",
        files_count: 2,
        rolled_back: false,
      },
    ],
  };
}

async function testCockpitAuxPane(js) {
  const h = createDomAndHostHarness({
    js,
    filename: "cockpit.html",
    hostCapabilities: {
      serverTools: {},
      message: { text: {} },
      updateModelContext: { text: {} },
    },
    hostContext: {
      theme: "dark",
      locale: "en-US",
      styles: { variables: { "--color-background-primary": "#0a0d13" } },
    },
    onHostRequest(msg) {
      if (msg.method === "tools/call") {
        const { name, arguments: args } = msg.params;
        if (name === "hyper_app_set_profile") {
          return {
            content: [{ type: "text", text: "ok" }],
            structuredContent: { ok: true, profile: args.profile },
          };
        }
        if (name === "hyper_app_cockpit_data") {
          return {
            content: [{ type: "text", text: "ok" }],
            structuredContent: sampleCockpitData(args.usage_window || "7d"),
          };
        }
        if (name === "hyper_app_tx_diff") {
          return {
            content: [{ type: "text", text: "diff" }],
            structuredContent: {
              tx_id: args.tx_id,
              diff_text: "--- a/src/mcp.rs\n+++ b/src/mcp.rs\n@@ -1,1 +1,2 @@\n pub fn mcp() {}\n+pub fn resources() {}",
              files: [{ path: "src/mcp.rs", abs_path: "/workspace/src/mcp.rs", added: 1, removed: 0 }],
            },
          };
        }
        if (name === "hyper_app_tx_rollback") {
          return {
            content: [{ type: "text", text: "ok" }],
            structuredContent: { ok: true, tx_id: args.tx_id, message: "Restored", restored_files: ["src/mcp.rs"] },
          };
        }
      }
      return {};
    },
  });

  try {
    await tick(30);
    assert.ok(h.sentToHost.some((m) => m.method === "ui/initialize"));
    assert.ok(h.sentToHost.some((m) => m.method === "ui/notifications/initialized"));

    h.deliverFromHost({
      jsonrpc: "2.0",
      method: "ui/notifications/tool-result",
      params: {
        content: [{ type: "text", text: "cockpit summary" }],
        structuredContent: sampleCockpitData("7d"),
      },
    });
    await tick(25);

    // Core sections & aux_pane capability-gated controls
    assert.ok(h.appEl.innerHTML.includes("usage-svg"), "cockpit must render inline SVG usage chart");
    assert.ok(h.appEl.innerHTML.includes("WATCH"), "cockpit must render audit grade pill");
    assert.ok(h.appEl.innerHTML.includes("empirical"), "cockpit must render TTL source badge");
    assert.ok(h.appEl.innerHTML.includes("data-send-compact"), "aux_pane must show Send /compact");
    assert.ok(h.appEl.innerHTML.includes("data-rescue-send"), "aux_pane must show 1-Click Rescue");
    assert.ok(h.appEl.innerHTML.includes("data-rescue-attach"), "aux_pane must show Attach Diag");

    // Profile switch -> hyper_app_set_profile
    h.appEl.dispatchClick("data-profile", "hyper");
    await tick(25);
    const profCall = h.sentToHost.find(
      (m) => m.method === "tools/call" && m.params?.name === "hyper_app_set_profile"
    );
    assert.ok(profCall, "cockpit must call hyper_app_set_profile");
    assert.equal(profCall.params.arguments.profile, "hyper");

    // Window switch -> hyper_app_cockpit_data
    h.appEl.dispatchClick("data-uwin", "30d");
    await tick(25);
    const dataCall = h.sentToHost.find(
      (m) => m.method === "tools/call" && m.params?.name === "hyper_app_cockpit_data"
    );
    assert.ok(dataCall, "cockpit must call hyper_app_cockpit_data on window switch");
    assert.equal(dataCall.params.arguments.usage_window, "30d");
    assert.equal(dataCall.params.arguments.audit_days, 14);

    // Send /compact -> ui/message
    h.appEl.dispatchClick("data-send-compact", "1");
    await tick(20);
    const compactMsg = h.sentToHost.find(
      (m) => m.method === "ui/message" && m.params?.content?.[0]?.text === "/compact"
    );
    assert.ok(compactMsg, "Send /compact must send ui/message with /compact");

    // 1-Click Rescue -> ui/message
    h.appEl.dispatchClick("data-rescue-send", "0");
    await tick(20);
    const rescueMsg = h.sentToHost.find(
      (m) => m.method === "ui/message" && m.params?.content?.[0]?.text?.includes("sub-stall-01")
    );
    assert.ok(rescueMsg, "1-Click Rescue must send ui/message with rescue prompt");

    // Attach diagnostic context -> ui/update-model-context
    h.appEl.dispatchClick("data-rescue-attach", "1");
    await tick(20);
    const ctxMsg = h.sentToHost.find((m) => m.method === "ui/update-model-context");
    assert.ok(ctxMsg, "Attach Diag must send ui/update-model-context");
    assert.ok(ctxMsg.params.content[0].text.includes("sub-loop-02"));

    // Inline WAL diff & two-step rollback inside cockpit
    h.appEl.dispatchClick("data-toggle-tx-diff", "tx-204");
    await tick(25);
    assert.ok(h.appEl.innerHTML.includes("pub fn resources()"), "cockpit must render inline TX diff");
    h.appEl.dispatchClick("data-arm-tx-rb", "tx-204");
    await tick(10);
    h.appEl.dispatchClick("data-confirm-tx-rb", "tx-204");
    await tick(25);
    const txRbCall = h.sentToHost.find(
      (m) => m.method === "tools/call" && m.params?.name === "hyper_app_tx_rollback"
    );
    assert.ok(txRbCall, "cockpit must support WAL rollback");
    assert.ok(h.appEl.innerHTML.includes("Rolled Back"));
  } finally {
    h.cleanup();
  }
}

async function testCockpitGlobalSurface(js) {
  const h = createDomAndHostHarness({
    js,
    filename: "cockpit.html",
    hostCapabilities: { serverTools: {} }, // Global view: NO message, NO updateModelContext
    hostContext: { theme: "light", locale: "zh-CN" },
    onHostRequest() {
      return {};
    },
  });

  try {
    await tick(30);
    h.deliverFromHost({
      jsonrpc: "2.0",
      method: "ui/notifications/tool-result",
      params: {
        content: [{ type: "text", text: "cockpit" }],
        structuredContent: sampleCockpitData("7d"),
      },
    });
    await tick(25);

    assert.equal(h.documentElement.getAttribute("data-theme"), "light");
    assert.ok(h.appEl.innerHTML.includes("全局视图"), "global view should auto-detect zh locale");
    assert.ok(!h.appEl.innerHTML.includes("data-send-compact"), "global view must hide Send /compact");
    assert.ok(!h.appEl.innerHTML.includes("data-attach-health"), "global view must hide Attach Context");
    assert.ok(!h.appEl.innerHTML.includes("data-rescue-send"), "global view must hide 1-Click Rescue");
    assert.ok(!h.appEl.innerHTML.includes("data-rescue-attach"), "global view must hide Attach Diag");
    assert.ok(h.appEl.innerHTML.includes("data-rescue-copy"), "global view should keep Copy fallback button");
  } finally {
    h.cleanup();
  }
}

async function main() {
  verifySourceHygiene();
  const tx = verifyStaticHygiene("tx-diff.html");
  const cp = verifyStaticHygiene("cockpit.html");

  await testTxDiff(tx.js);
  await testCockpitAuxPane(cp.js);
  await testCockpitGlobalSurface(cp.js);

  console.log("PASS: static hygiene, CSP compliance, and JS syntax checks");
  console.log(`  tx-diff.html : ${(tx.bytes / 1024).toFixed(1)} KiB`);
  console.log(`  cockpit.html : ${(cp.bytes / 1024).toFixed(1)} KiB`);
  console.log("PASS: tx-diff.html SEP-1865 handshake, auto-diff, toggle-aux-pane (+ -32601), 2-step rollback, theme/locale, size-changed <= 800px");
  console.log("PASS: cockpit.html aux_pane (profile, window switch, SVG chart, /compact, 1-click rescue, update-model-context, inline TX diff/rollback)");
  console.log("PASS: cockpit.html global view capability gating (conversation-only buttons hidden, zh-CN locale auto-detected)");
}

main().catch((err) => {
  console.error("VERIFY FAILED:", err);
  process.exit(1);
});
