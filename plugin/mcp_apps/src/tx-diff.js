/**
 * AGY Hyper — WAL Transaction Diff & Instant Rollback MCP App
 * URI: ui://agy-hyper/tx-diff.html (inline card for `hyper_tx`)
 */

import { App } from "@modelcontextprotocol/ext-apps/app-with-deps";
import {
  BRAND_LOGO_SVG,
  applyHostContext,
  createToastController,
  detectLocaleLang,
  escapeHtml,
  extractStructured,
  extractTextContent,
  fmtRelTime,
  openFileInAuxPane,
  parseUnifiedDiff,
  pathBasename,
  renderDiffSectionsHtml,
} from "./mcp-helpers.js";

const I18N = {
  en: {
    title: "WAL Transaction Diff",
    loadingTool: "Loading WAL transactions…",
    loadingDiff: "Loading unified diff…",
    cancelled: "Transaction inspection was cancelled.",
    emptyTx: "No WAL transactions recorded in this workspace yet.",
    emptyDiff: "No unified line diff available for this transaction.",
    rolledBack: "Rolled Back",
    sourceHyper: "hyper_edit",
    sourceNative: "native",
    rollbackBtn: "Rollback TX",
    confirmAsk: "Restore files?",
    confirmBtn: "Confirm Rollback",
    cancelBtn: "Cancel",
    rollingBack: "Rolling back…",
    openFileTip: "Open in side pane",
    filesUnit: "f",
    txUnit: "tx",
    rollbackOk: "Rollback complete",
    rollbackFail: "Rollback failed",
    toastOpened: "Opened in side pane",
    toastPaneUnavailable: "Side pane file view unavailable in this surface",
    toastDiffErr: "Failed to load diff",
    toastRbErr: "Rollback failed",
    noSummary: "Atomic WAL snapshot",
    diffUnavailable: "Diff unavailable (server tools disabled)",
  },
  zh: {
    title: "WAL 事务 Diff 与回滚",
    loadingTool: "正在读取 WAL 事务快照…",
    loadingDiff: "正在加载行级 Unified Diff…",
    cancelled: "事务查询已取消。",
    emptyTx: "当前工作区暂无 WAL 文件编辑快照。",
    emptyDiff: "该事务暂无可展示的行级改动。",
    rolledBack: "已回滚",
    sourceHyper: "hyper_edit",
    sourceNative: "原生编辑",
    rollbackBtn: "回滚事务",
    confirmAsk: "确认还原文件？",
    confirmBtn: "确认回滚",
    cancelBtn: "取消",
    rollingBack: "正在回滚…",
    openFileTip: "在侧栏打开文件",
    filesUnit: "文件",
    txUnit: "笔",
    rollbackOk: "回滚成功",
    rollbackFail: "回滚失败",
    toastOpened: "已在侧栏打开文件",
    toastPaneUnavailable: "当前视图不支持侧栏打开文件",
    toastDiffErr: "加载 Diff 失败",
    toastRbErr: "回滚失败",
    noSummary: "原子 WAL 快照",
    diffUnavailable: "无法加载 Diff（宿主未开启工具回调）",
  },
};

const MAX_INLINE_HEIGHT = 800;

const state = {
  lang: "en",
  langManual: false,
  connected: false,
  loadingInitial: true,
  cancelled: false,
  errorText: "",
  fallbackText: "",
  action: "list",
  workspace: "",
  transactions: [],
  selectedTxId: "",
  rollbackBanner: null,
  diffCache: new Map(), // tx_id -> { loading, error, data }
  collapsedFiles: new Set(),
  confirmRollbackTxId: "",
  rollingBackTxId: "",
  maxHeight: MAX_INLINE_HEIGHT,
  capabilities: {},
};

const app = new App(
  { name: "AGY Hyper TX Diff", version: "0.6.0" },
  {},
  { autoResize: false }
);

const rootEl = document.getElementById("app");
const toastEl = document.getElementById("toast");
const showToast = createToastController(toastEl);

function t(key) {
  return I18N[state.lang]?.[key] ?? I18N.en[key] ?? key;
}

let resizeQueued = false;
let lastSentWidth = 0;
let lastSentHeight = 0;

function notifySize() {
  if (!state.connected || resizeQueued) return;
  resizeQueued = true;
  const run = () => {
    resizeQueued = false;
    try {
      const docEl = document.documentElement;
      const prevH = docEl.style.height;
      docEl.style.height = "max-content";
      const rect = docEl.getBoundingClientRect();
      docEl.style.height = prevH;
      const rawH = Math.ceil(rect.height || document.body.scrollHeight || 160);
      const cap = Math.min(MAX_INLINE_HEIGHT, Number(state.maxHeight) || MAX_INLINE_HEIGHT);
      const height = Math.max(120, Math.min(cap, rawH));
      const width = Math.ceil(window.innerWidth || 640);
      if (height !== lastSentHeight || width !== lastSentWidth) {
        lastSentWidth = width;
        lastSentHeight = height;
        app.sendSizeChanged({ width, height }).catch(() => {});
      }
    } catch {
      // ignore layout errors during teardown
    }
  };
  if (typeof requestAnimationFrame === "function") {
    requestAnimationFrame(run);
  } else {
    setTimeout(run, 0);
  }
}

function normalizeFile(f) {
  return {
    path: f?.path || f?.rel_path || "",
    abs_path: f?.abs_path || "",
    added: Number(f?.added ?? f?.lines_added ?? 0),
    removed: Number(f?.removed ?? f?.lines_removed ?? 0),
  };
}

function normalizeTx(tx) {
  const files = Array.isArray(tx?.files) ? tx.files.map(normalizeFile) : [];
  return {
    tx_id: String(tx?.tx_id || ""),
    created_at: tx?.created_at ?? tx?.timestamp_ms ?? "",
    source: String(tx?.source || "hyper_edit"),
    summary: String(tx?.summary || ""),
    files,
    rolled_back: Boolean(tx?.rolled_back),
  };
}

function txTotals(tx, diffData) {
  const srcFiles =
    diffData?.files?.length ? diffData.files.map(normalizeFile) : tx?.files || [];
  let added = 0;
  let removed = 0;
  for (const f of srcFiles) {
    added += Number(f.added || 0);
    removed += Number(f.removed || 0);
  }
  return { added, removed, fileCount: srcFiles.length };
}

function applyToolResult(result) {
  state.loadingInitial = false;
  state.cancelled = false;
  state.errorText = "";
  state.fallbackText = "";

  if (!result) {
    render();
    return;
  }

  if (result.isError) {
    state.errorText = extractTextContent(result) || "Tool execution failed.";
    render();
    return;
  }

  const sc = extractStructured(result);
  if (!sc) {
    state.fallbackText = extractTextContent(result);
    render();
    return;
  }

  state.action = sc.action || "list";
  state.workspace = sc.workspace || state.workspace || "";
  if (Array.isArray(sc.transactions)) {
    state.transactions = sc.transactions.map(normalizeTx).filter((x) => x.tx_id);
  }

  if (sc.diff && sc.diff.tx_id) {
    state.diffCache.set(sc.diff.tx_id, {
      loading: false,
      error: "",
      data: {
        tx_id: sc.diff.tx_id,
        diff_text: String(sc.diff.diff_text || ""),
        files: Array.isArray(sc.diff.files) ? sc.diff.files.map(normalizeFile) : [],
      },
    });
  }

  if (sc.rollback && sc.rollback.tx_id) {
    state.rollbackBanner = {
      ok: Boolean(sc.rollback.ok),
      tx_id: String(sc.rollback.tx_id),
      message: String(sc.rollback.message || ""),
      restored_files: Array.isArray(sc.rollback.restored_files)
        ? sc.rollback.restored_files
        : [],
    };
    if (sc.rollback.ok) {
      const target = state.transactions.find((x) => x.tx_id === sc.rollback.tx_id);
      if (target) target.rolled_back = true;
    }
  }

  const preferred =
    sc.selected_tx_id ||
    sc.diff?.tx_id ||
    sc.rollback?.tx_id ||
    state.selectedTxId ||
    state.transactions[0]?.tx_id ||
    "";

  if (preferred && (!state.transactions.length || state.transactions.some((x) => x.tx_id === preferred))) {
    state.selectedTxId = preferred;
  } else {
    state.selectedTxId = state.transactions[0]?.tx_id || preferred;
  }

  render();
  if (state.selectedTxId) {
    ensureDiffLoaded(state.selectedTxId);
  }
}

async function ensureDiffLoaded(txId) {
  if (!txId) return;
  const cached = state.diffCache.get(txId);
  if (cached && (cached.loading || cached.data)) return;

  if (!state.capabilities?.serverTools) {
    state.diffCache.set(txId, {
      loading: false,
      error: t("diffUnavailable"),
      data: null,
    });
    render();
    return;
  }

  state.diffCache.set(txId, { loading: true, error: "", data: null });
  render();

  try {
    const res = await app.callServerTool({
      name: "hyper_app_tx_diff",
      arguments: { tx_id: txId },
    });
    if (res?.isError) {
      throw new Error(extractTextContent(res) || t("toastDiffErr"));
    }
    const sc = extractStructured(res) || {};
    state.diffCache.set(txId, {
      loading: false,
      error: "",
      data: {
        tx_id: sc.tx_id || txId,
        diff_text: String(sc.diff_text || ""),
        files: Array.isArray(sc.files) ? sc.files.map(normalizeFile) : [],
      },
    });
    // Merge file stats back into transaction item if it had fewer details
    const tx = state.transactions.find((x) => x.tx_id === txId);
    if (tx && Array.isArray(sc.files) && sc.files.length && !tx.files.length) {
      tx.files = sc.files.map(normalizeFile);
    }
  } catch (err) {
    const msg = String(err?.message || err || t("toastDiffErr"));
    state.diffCache.set(txId, { loading: false, error: msg, data: null });
    showToast(`${t("toastDiffErr")}: ${msg}`);
  }
  render();
}

async function performRollback(txId) {
  if (!txId || state.rollingBackTxId) return;
  if (!state.capabilities?.serverTools) return;

  state.rollingBackTxId = txId;
  state.confirmRollbackTxId = "";
  render();

  try {
    const res = await app.callServerTool({
      name: "hyper_app_tx_rollback",
      arguments: { tx_id: txId },
    });
    const sc = extractStructured(res);
    if (res?.isError && !sc) {
      throw new Error(extractTextContent(res) || t("toastRbErr"));
    }
    const ok = sc ? Boolean(sc.ok) : !res?.isError;
    const message = sc?.message || extractTextContent(res) || (ok ? t("rollbackOk") : t("rollbackFail"));
    const restored = Array.isArray(sc?.restored_files) ? sc.restored_files : [];

    state.rollbackBanner = {
      ok,
      tx_id: sc?.tx_id || txId,
      message,
      restored_files: restored,
    };

    if (ok) {
      const tx = state.transactions.find((x) => x.tx_id === txId);
      if (tx) tx.rolled_back = true;
      showToast(`${t("rollbackOk")} · ${txId}`);
    } else {
      showToast(`${t("rollbackFail")}: ${message}`);
    }
  } catch (err) {
    const msg = String(err?.message || err || t("toastRbErr"));
    state.rollbackBanner = {
      ok: false,
      tx_id: txId,
      message: msg,
      restored_files: [],
    };
    showToast(`${t("toastRbErr")}: ${msg}`);
  } finally {
    state.rollingBackTxId = "";
    render();
  }
}

async function handleOpenFile(absPath) {
  if (!absPath) return;
  const out = await openFileInAuxPane(app, absPath);
  if (out.status === "ok") {
    showToast(`${t("toastOpened")}: ${pathBasename(absPath)}`);
  } else if (out.status === "unavailable") {
    showToast(t("toastPaneUnavailable"));
  }
  // status === "unsupported" (-32601) is silently ignored per spec
}

function renderSourceBadge(source) {
  const isNative = String(source || "").toLowerCase().includes("native");
  const cls = isNative ? "indigo" : "info";
  const label = isNative ? t("sourceNative") : t("sourceHyper");
  return `<span class="status-pill ${cls}">${escapeHtml(label)}</span>`;
}

function render() {
  const wsName = pathBasename(state.workspace);
  const selectedTx =
    state.transactions.find((x) => x.tx_id === state.selectedTxId) ||
    (state.selectedTxId
      ? {
          tx_id: state.selectedTxId,
          created_at: "",
          source: "hyper_edit",
          summary: "",
          files: state.diffCache.get(state.selectedTxId)?.data?.files || [],
          rolled_back: false,
        }
      : null);

  const headerHtml = `
    <header class="tx-topbar">
      <div class="tx-brand">
        ${BRAND_LOGO_SVG}
        <span class="tx-brand-title">${escapeHtml(t("title"))}</span>
        ${wsName ? `<span class="mono-tag" title="${escapeHtml(state.workspace)}">${escapeHtml(wsName)}</span>` : ""}
        ${state.transactions.length ? `<span class="mono-tag">${state.transactions.length} ${escapeHtml(t("txUnit"))}</span>` : ""}
      </div>
      <div class="tx-topbar-right">
        <div class="lang-switch" role="group" aria-label="Language">
          <button type="button" class="lang-btn ${state.lang === "en" ? "active" : ""}" data-set-lang="en">EN</button>
          <button type="button" class="lang-btn ${state.lang === "zh" ? "active" : ""}" data-set-lang="zh">中</button>
        </div>
      </div>
    </header>`;

  let bannerHtml = "";
  if (state.rollbackBanner) {
    const rb = state.rollbackBanner;
    const filesTxt = rb.restored_files?.length ? rb.restored_files.join(", ") : "";
    bannerHtml = `
      <div class="rollback-banner ${rb.ok ? "" : "err"}" role="status">
        <div class="rollback-banner-body">
          <span class="rollback-banner-title">${escapeHtml(rb.ok ? t("rollbackOk") : t("rollbackFail"))} · ${escapeHtml(rb.tx_id)}</span>
          ${rb.message ? `<span>${escapeHtml(rb.message)}</span>` : ""}
          ${filesTxt ? `<span class="rollback-banner-files">${escapeHtml(filesTxt)}</span>` : ""}
        </div>
        <button type="button" class="btn-ghost btn-xs" data-dismiss-banner="1">×</button>
      </div>`;
  }

  let bodyHtml = "";
  if (state.loadingInitial) {
    bodyHtml = `<div class="panel"><div class="loading-strip"><span>${escapeHtml(t("loadingTool"))}</span></div></div>`;
  } else if (state.cancelled) {
    bodyHtml = `<div class="panel"><div class="empty-state">${escapeHtml(t("cancelled"))}</div></div>`;
  } else if (state.errorText) {
    bodyHtml = `<div class="rollback-banner err"><div class="rollback-banner-body"><span class="rollback-banner-title">Error</span><span>${escapeHtml(state.errorText)}</span></div></div>`;
  } else if (state.fallbackText && !state.transactions.length && !selectedTx) {
    bodyHtml = `<div class="panel"><pre class="diff-code" style="white-space:pre-wrap;padding:6px">${escapeHtml(state.fallbackText)}</pre></div>`;
  } else if (!state.transactions.length && !selectedTx) {
    bodyHtml = `<div class="panel"><div class="empty-state">${escapeHtml(t("emptyTx"))}</div></div>`;
  } else {
    const stripHtml = state.transactions.length
      ? `<nav class="tx-strip" aria-label="WAL Transactions">
          ${state.transactions
            .map((tx) => {
              const isSel = tx.tx_id === state.selectedTxId;
              const diffEntry = state.diffCache.get(tx.tx_id)?.data;
              const totals = txTotals(tx, diffEntry);
              const firstNames = (tx.files || [])
                .map((f) => pathBasename(f.path))
                .filter(Boolean)
                .slice(0, 2)
                .join(", ");
              const subLine = tx.summary || firstNames || t("noSummary");
              return `
                <button type="button" class="tx-pill ${isSel ? "selected" : ""} ${tx.rolled_back ? "rolled-back" : ""}" data-select-tx="${escapeHtml(tx.tx_id)}" aria-pressed="${String(isSel)}">
                  <div class="tx-pill-top">
                    <span class="tx-id-text">${escapeHtml(tx.tx_id)}</span>
                    ${tx.rolled_back ? `<span class="status-pill muted">${escapeHtml(t("rolledBack"))}</span>` : renderSourceBadge(tx.source)}
                  </div>
                  <div class="tx-pill-mid" title="${escapeHtml(subLine)}">${escapeHtml(subLine)}</div>
                  <div class="tx-pill-bot">
                    <span>${escapeHtml(fmtRelTime(tx.created_at, state.lang))} · ${totals.fileCount}${escapeHtml(t("filesUnit"))}</span>
                    <span><span class="delta-add">+${totals.added}</span> <span class="delta-del">-${totals.removed}</span></span>
                  </div>
                </button>`;
            })
            .join("")}
        </nav>`
      : "";

    let detailHtml = "";
    if (selectedTx) {
      const diffState = state.diffCache.get(selectedTx.tx_id);
      const diffData = diffState?.data || null;
      const filesList = diffData?.files?.length ? diffData.files : selectedTx.files || [];
      const totals = txTotals(selectedTx, diffData);
      const isConfirming = state.confirmRollbackTxId === selectedTx.tx_id;
      const isRolling = state.rollingBackTxId === selectedTx.tx_id;

      let actionControl = "";
      if (selectedTx.rolled_back) {
        actionControl = `<span class="status-pill muted">${escapeHtml(t("rolledBack"))}</span>`;
      } else if (state.capabilities?.serverTools) {
        if (isRolling) {
          actionControl = `<button type="button" class="btn-danger" disabled>${escapeHtml(t("rollingBack"))}</button>`;
        } else if (isConfirming) {
          actionControl = `
            <div class="confirm-group">
              <span class="confirm-label">${escapeHtml(t("confirmAsk"))}</span>
              <button type="button" class="btn-ghost btn-xs" data-cancel-rb="1">${escapeHtml(t("cancelBtn"))}</button>
              <button type="button" class="btn-danger solid btn-xs" data-confirm-rb="${escapeHtml(selectedTx.tx_id)}">${escapeHtml(t("confirmBtn"))}</button>
            </div>`;
        } else {
          actionControl = `<button type="button" class="btn-danger" data-arm-rb="${escapeHtml(selectedTx.tx_id)}">${escapeHtml(t("rollbackBtn"))}</button>`;
        }
      }

      const fileChipsHtml = filesList.length
        ? `<div class="tx-files-bar">
            ${filesList
              .map((f) => {
                const abs = f.abs_path || "";
                const tag = abs ? "button" : "span";
                const attrs = abs
                  ? `type="button" data-open-file="${escapeHtml(abs)}" title="${escapeHtml(t("openFileTip") + ": " + abs)}"`
                  : `style="cursor:default"`;
                return `<${tag} class="file-chip" ${attrs}>
                  <span class="file-chip-name">${escapeHtml(f.path || pathBasename(abs))}</span>
                  <span class="delta-add">+${f.added || 0}</span>
                  <span class="delta-del">-${f.removed || 0}</span>
                </${tag}>`;
              })
              .join("")}
          </div>`
        : "";

      let diffBodyHtml = "";
      if (!diffState || diffState.loading) {
        diffBodyHtml = `<div class="loading-strip"><span>${escapeHtml(t("loadingDiff"))}</span></div>`;
      } else if (diffState.error) {
        diffBodyHtml = `<div class="empty-state">${escapeHtml(diffState.error)}</div>`;
      } else {
        const sections = parseUnifiedDiff(diffData?.diff_text || "", filesList);
        diffBodyHtml = renderDiffSectionsHtml(
          sections,
          state.collapsedFiles,
          t("emptyDiff"),
          t("openFileTip")
        );
      }

      detailHtml = `
        <section class="tx-detail-card">
          <div class="tx-detail-head">
            <div class="tx-detail-meta">
              <div class="tx-detail-title-row">
                <span class="tx-id-text">${escapeHtml(selectedTx.tx_id)}</span>
                ${renderSourceBadge(selectedTx.source)}
                ${selectedTx.created_at ? `<span class="mono-tag">${escapeHtml(fmtRelTime(selectedTx.created_at, state.lang))}</span>` : ""}
                <span class="mono-tag"><span class="delta-add">+${totals.added}</span> <span class="delta-del">-${totals.removed}</span></span>
              </div>
              ${selectedTx.summary ? `<div class="tx-detail-summary">${escapeHtml(selectedTx.summary)}</div>` : ""}
            </div>
            <div class="tx-detail-actions">${actionControl}</div>
          </div>
          ${fileChipsHtml}
          <div class="tx-diff-body">${diffBodyHtml}</div>
        </section>`;
    }

    bodyHtml = `${stripHtml}${detailHtml}`;
  }

  rootEl.innerHTML = `<div class="tx-shell">${headerHtml}${bannerHtml}${bodyHtml}</div>`;
  notifySize();
}

// Delegated event handling (clean, zero listener leaks across re-renders)
rootEl.addEventListener("click", (ev) => {
  const langBtn = ev.target.closest("[data-set-lang]");
  if (langBtn) {
    state.lang = langBtn.getAttribute("data-set-lang") === "zh" ? "zh" : "en";
    state.langManual = true;
    render();
    return;
  }

  if (ev.target.closest("[data-dismiss-banner]")) {
    state.rollbackBanner = null;
    render();
    return;
  }

  const selBtn = ev.target.closest("[data-select-tx]");
  if (selBtn) {
    const nextId = selBtn.getAttribute("data-select-tx") || "";
    if (nextId && nextId !== state.selectedTxId) {
      state.selectedTxId = nextId;
      state.confirmRollbackTxId = "";
      state.collapsedFiles.clear();
      render();
      ensureDiffLoaded(nextId);
    }
    return;
  }

  const armBtn = ev.target.closest("[data-arm-rb]");
  if (armBtn) {
    state.confirmRollbackTxId = armBtn.getAttribute("data-arm-rb") || "";
    render();
    return;
  }

  if (ev.target.closest("[data-cancel-rb]")) {
    state.confirmRollbackTxId = "";
    render();
    return;
  }

  const confirmBtn = ev.target.closest("[data-confirm-rb]");
  if (confirmBtn) {
    const txId = confirmBtn.getAttribute("data-confirm-rb") || "";
    performRollback(txId);
    return;
  }

  const toggleBtn = ev.target.closest("[data-toggle-file]");
  if (toggleBtn) {
    const key = toggleBtn.getAttribute("data-toggle-file") || "";
    if (state.collapsedFiles.has(key)) state.collapsedFiles.delete(key);
    else state.collapsedFiles.add(key);
    render();
    return;
  }

  const openBtn = ev.target.closest("[data-open-file]");
  if (openBtn) {
    handleOpenFile(openBtn.getAttribute("data-open-file") || "");
  }
});

// Register all MCP Apps lifecycle handlers BEFORE app.connect()
app.ontoolinput = (params) => {
  const args = params?.arguments || {};
  if (args.tx_id && !state.selectedTxId) {
    state.selectedTxId = String(args.tx_id);
  }
  state.loadingInitial = true;
  state.cancelled = false;
  render();
};

app.ontoolresult = (result) => {
  applyToolResult(result);
};

app.ontoolcancelled = () => {
  state.loadingInitial = false;
  state.cancelled = true;
  render();
};

app.onhostcontextchanged = (ctx) => {
  applyHostContext(ctx);
  if (ctx?.locale && !state.langManual) {
    state.lang = detectLocaleLang(ctx.locale);
  }
  if (ctx?.containerDimensions?.maxHeight) {
    state.maxHeight = Math.min(MAX_INLINE_HEIGHT, Number(ctx.containerDimensions.maxHeight));
  }
  render();
};

async function boot() {
  render();
  try {
    await app.connect();
    state.connected = true;
    state.capabilities = app.getHostCapabilities() || {};
    const ctx = app.getHostContext();
    applyHostContext(ctx);
    if (ctx?.locale && !state.langManual) {
      state.lang = detectLocaleLang(ctx.locale);
    }
    if (ctx?.containerDimensions?.maxHeight) {
      state.maxHeight = Math.min(MAX_INLINE_HEIGHT, Number(ctx.containerDimensions.maxHeight));
    }
    if (typeof ResizeObserver === "function") {
      const ro = new ResizeObserver(() => notifySize());
      ro.observe(document.documentElement);
      ro.observe(document.body);
    }
    render();
    if (state.selectedTxId) {
      ensureDiffLoaded(state.selectedTxId);
    }
  } catch (err) {
    state.loadingInitial = false;
    state.errorText = String(err?.message || err || "Failed to connect to MCP Apps host");
    render();
  }
}

boot();
