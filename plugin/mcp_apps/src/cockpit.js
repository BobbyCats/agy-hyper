/**
 * AGY Hyper — Live Telemetry & Watchdog Cockpit MCP App
 * URI: ui://agy-hyper/cockpit.html (`aux_pane` + `global` entrypoints for `hyper_cockpit`)
 */

import { App } from "@modelcontextprotocol/ext-apps/app-with-deps";
import {
  BRAND_LOGO_SVG,
  applyHostContext,
  copyToClipboard,
  createToastController,
  detectLocaleLang,
  escapeHtml,
  extractStructured,
  extractTextContent,
  fmtCurrency,
  fmtDurationSec,
  fmtRelTime,
  fmtTokens,
  openFileInAuxPane,
  parseUnifiedDiff,
  pathBasename,
  renderDiffSectionsHtml,
} from "./mcp-helpers.js";

const PROFILES = [
  { id: "observe", enTitle: "Observe", enDesc: "Read-only", zhTitle: "纯观测", zhDesc: "只读遥测" },
  { id: "safe", enTitle: "Safe", enDesc: "Native diff", zhTitle: "安全稳健", zhDesc: "保原生 Diff" },
  { id: "balanced", enTitle: "Balanced", enDesc: "Fast & safe", zhTitle: "均衡推荐", zhDesc: "极速受控" },
  { id: "hyper", enTitle: "Hyper", enDesc: "Pre-warm", zhTitle: "狂暴预热", zhDesc: "后台预热" },
];

const WINDOWS = ["24h", "7d", "30d", "90d"];

const I18N = {
  en: {
    brand: "AGY Hyper",
    refresh: "Refresh",
    refreshing: "Syncing…",
    connecting: "Connecting cockpit…",
    cancelled: "Cockpit query cancelled.",
    surfaceGlobal: "Global",
    surfacePane: "Side Pane",
    kpiContext: "Context Window",
    kpiCacheHit: "Cache Hit",
    kpiModel: "Active Model",
    kpiCost: "Est. Value",
    warmthHeading: "Cache Warmth & Context Meter",
    contextBar: "Context",
    cacheBar: "Prompt Cache",
    warm: "WARM",
    cooling: "COOLING",
    cold: "COLD",
    ttlEmpirical: "empirical",
    ttlDefault: "default",
    hintWarm: "Prompt cache is warm — stay in-loop to skip full prefill.",
    hintCooling: "Entering decay window — batch next turns to preserve prefix cache.",
    hintCold: "Cache cooled — combine next steps into one turn to re-warm.",
    sendCompact: "Send /compact",
    attachDiag: "Attach Context",
    subagentsHeading: "Subagent Watchdog",
    subagentsEmpty: "No active or recent subagents detected.",
    activeBadge: "active",
    stalledBadge: "stalled",
    loopBadge: "crash loop",
    stalledPill: "STALLED",
    loopPill: "LOOP",
    rescueSend: "1-Click Rescue",
    rescueAttach: "Attach Diag",
    rescueCopy: "Copy",
    usageHeading: "Usage Ledger & Burn Audit",
    metricCost: "Cost",
    metricTokens: "Tokens",
    statInput: "Uncached In",
    statCacheRead: "Cache Read",
    statOutput: "Output",
    chartLegendBar: "Bar:",
    chartLegendLine: "Line: Cache %",
    chartEmpty: "No daily telemetry points in this window yet.",
    auditFirstEdit: "1st Edit P50",
    auditSplitDrop: "Split Drop",
    auditCache: "Audit Cache",
    findingsEmpty: "No burn anomalies detected in this audit window.",
    txHeading: "Recent WAL Transactions",
    txEmpty: "No WAL file transactions recorded yet.",
    diffShow: "Diff",
    diffHide: "Hide",
    diffLoading: "Loading unified diff…",
    diffEmpty: "No line diff available.",
    rollbackBtn: "Rollback",
    confirmBtn: "Confirm",
    cancelBtn: "Cancel",
    rolledBack: "Rolled Back",
    openFileTip: "Open in side pane",
    toastProfileOk: "Profile switched to",
    toastProfileErr: "Profile switch failed",
    toastRefreshErr: "Refresh failed",
    toastCompactSent: "Sent /compact to conversation",
    toastRescueSent: "Rescue prompt sent to conversation",
    toastMsgErr: "Failed to send message",
    toastCtxAttached: "Diagnostic context attached to next message",
    toastCtxErr: "Failed to attach context",
    toastCopied: "Copied rescue prompt to clipboard",
    toastCopyFail: "Clipboard blocked in sandbox",
    toastRollbackOk: "Rolled back",
    toastRollbackErr: "Rollback failed",
    toastOpened: "Opened in side pane",
    toastPaneUnavailable: "Side pane file view unavailable here",
    gradeHealthy: "HEALTHY",
    gradeWatch: "WATCH",
    gradeBleeding: "BLEEDING",
    healthHealthy: "Healthy",
    healthWarming: "Near Limit",
    healthRedline: "Redline",
    healthLoop: "Error Loop",
    healthCompacted: "Compacted",
    filesUnit: "f",
  },
  zh: {
    brand: "AGY Hyper",
    refresh: "刷新",
    refreshing: "同步中…",
    connecting: "正在连接驾驶舱…",
    cancelled: "驾驶舱数据查询已取消。",
    surfaceGlobal: "全局视图",
    surfacePane: "侧边栏",
    kpiContext: "上下文水位",
    kpiCacheHit: "缓存命中",
    kpiModel: "当前模型",
    kpiCost: "窗口估值",
    warmthHeading: "缓存保鲜与上下文仪表",
    contextBar: "上下文占用",
    cacheBar: "Prompt 缓存",
    warm: "热缓存",
    cooling: "衰减区间",
    cold: "已冷却",
    ttlEmpirical: "实测",
    ttlDefault: "默认",
    hintWarm: "Prompt 缓存正热，保持连续循环可跳过全量重算。",
    hintCooling: "进入缓存衰减期，合并后续动作可保住前缀缓存。",
    hintCold: "缓存已冷，将后续步骤合并在一轮内重新预热更省时。",
    sendCompact: "发送 /compact",
    attachDiag: "附加诊断上下文",
    subagentsHeading: "子代理看门狗",
    subagentsEmpty: "当前暂无活跃或异常子代理。",
    activeBadge: "运行中",
    stalledBadge: "卡死",
    loopBadge: "连崩",
    stalledPill: "卡死",
    loopPill: "连崩",
    rescueSend: "一键救援",
    rescueAttach: "附加诊断",
    rescueCopy: "复制",
    usageHeading: "多日用量账本与烧钱体检",
    metricCost: "估值",
    metricTokens: "Token",
    statInput: "新增输入",
    statCacheRead: "缓存读取",
    statOutput: "模型输出",
    chartLegendBar: "柱状：",
    chartLegendLine: "折线：缓存命中%",
    chartEmpty: "当前窗口暂无按日遥测记录。",
    auditFirstEdit: "首改前 P50",
    auditSplitDrop: "切换掉率",
    auditCache: "审计命中",
    findingsEmpty: "本周期未检测到显著缓存泄漏或异常。",
    txHeading: "最近 WAL 事务快照",
    txEmpty: "暂无 WAL 文件改动快照。",
    diffShow: "看改动",
    diffHide: "收起",
    diffLoading: "正在加载行级 Diff…",
    diffEmpty: "暂无可展示的行级改动。",
    rollbackBtn: "回滚",
    confirmBtn: "确认回滚",
    cancelBtn: "取消",
    rolledBack: "已回滚",
    openFileTip: "在侧栏打开文件",
    toastProfileOk: "已切换安全模式：",
    toastProfileErr: "切换模式失败",
    toastRefreshErr: "刷新失败",
    toastCompactSent: "已向对话发送 /compact",
    toastRescueSent: "已将救援指令直接发送到对话",
    toastMsgErr: "发送消息失败",
    toastCtxAttached: "已将诊断摘要附加至下一条消息",
    toastCtxErr: "附加诊断上下文失败",
    toastCopied: "已复制救援指令到剪贴板",
    toastCopyFail: "沙箱环境限制了剪贴板访问",
    toastRollbackOk: "已原子回滚",
    toastRollbackErr: "回滚失败",
    toastOpened: "已在侧栏打开文件",
    toastPaneUnavailable: "当前视图不支持侧栏打开文件",
    gradeHealthy: "健康",
    gradeWatch: "需留意",
    gradeBleeding: "严重漏水",
    healthHealthy: "健康流畅",
    healthWarming: "接近警戒线",
    healthRedline: "触顶红线",
    healthLoop: "连崩循环",
    healthCompacted: "已压缩",
    filesUnit: "文件",
  },
};

const state = {
  lang: "en",
  langManual: false,
  connected: false,
  loading: true,
  refreshing: false,
  cancelled: false,
  errorText: "",
  data: null,
  receivedAtMs: 0,
  usageWindow: "7d",
  usageMetric: "cost", // "cost" | "tokens"
  hoveredDayIdx: -1,
  switchingProfile: "",
  openDiffs: new Map(), // tx_id -> { loading, error, data }
  collapsedFiles: new Set(),
  confirmRollbackTxId: "",
  rollingBackTxId: "",
  capabilities: {},
  receivedToolInput: false,
  receivedToolResult: false,
};

const app = new App(
  { name: "AGY Hyper Cockpit", version: "0.6.0" },
  {},
  { autoResize: true }
);

const rootEl = document.getElementById("app");
const toastEl = document.getElementById("toast");
const showToast = createToastController(toastEl);

function t(key) {
  return I18N[state.lang]?.[key] ?? I18N.en[key] ?? key;
}

function hasMessageCap() {
  return Boolean(state.capabilities?.message);
}

function hasUpdateContextCap() {
  return Boolean(state.capabilities?.updateModelContext);
}

function hasServerToolsCap() {
  return Boolean(state.capabilities?.serverTools);
}

function applyCockpitPayload(sc) {
  if (!sc || typeof sc !== "object") return;
  state.data = sc;
  state.receivedAtMs = Date.now();
  state.loading = false;
  state.cancelled = false;
  state.errorText = "";
  if (sc.usage?.window && WINDOWS.includes(String(sc.usage.window).toLowerCase())) {
    state.usageWindow = String(sc.usage.window).toLowerCase();
  }
  state.hoveredDayIdx = -1;
  render();
}

async function fetchCockpitData(windowOverride) {
  if (!hasServerToolsCap() || state.refreshing) return;
  const win = windowOverride || state.usageWindow || "7d";
  state.usageWindow = win;
  state.refreshing = true;
  render();
  try {
    const res = await app.callServerTool({
      name: "hyper_app_cockpit_data",
      arguments: { usage_window: win, audit_days: 14 },
    });
    if (res?.isError) {
      throw new Error(extractTextContent(res) || t("toastRefreshErr"));
    }
    const sc = extractStructured(res);
    if (sc) {
      applyCockpitPayload(sc);
    }
  } catch (err) {
    const msg = String(err?.message || err || t("toastRefreshErr"));
    if (!state.data) state.errorText = msg;
    showToast(`${t("toastRefreshErr")}: ${msg}`);
  } finally {
    state.refreshing = false;
    state.loading = false;
    render();
  }
}

async function setProfile(profile) {
  if (!hasServerToolsCap() || !profile || state.switchingProfile) return;
  if (state.data?.profile === profile) return;
  state.switchingProfile = profile;
  render();
  try {
    const res = await app.callServerTool({
      name: "hyper_app_set_profile",
      arguments: { profile },
    });
    if (res?.isError) {
      throw new Error(extractTextContent(res) || t("toastProfileErr"));
    }
    const sc = extractStructured(res);
    const nextProfile = sc?.profile || profile;
    if (state.data) state.data.profile = nextProfile;
    showToast(`${t("toastProfileOk")} ${nextProfile}`);
  } catch (err) {
    showToast(`${t("toastProfileErr")}: ${String(err?.message || err)}`);
  } finally {
    state.switchingProfile = "";
    render();
  }
}

async function sendConversationMessage(text, okToast) {
  if (!hasMessageCap() || !text) return;
  try {
    const res = await app.sendMessage({
      role: "user",
      content: [{ type: "text", text }],
    });
    if (res?.isError) {
      throw new Error("Host rejected message");
    }
    showToast(okToast);
  } catch (err) {
    showToast(`${t("toastMsgErr")}: ${String(err?.message || err)}`);
  }
}

async function attachDiagnosticContext(text) {
  if (!hasUpdateContextCap() || !text) return;
  try {
    await app.updateModelContext({
      content: [{ type: "text", text }],
    });
    showToast(t("toastCtxAttached"));
  } catch (err) {
    showToast(`${t("toastCtxErr")}: ${String(err?.message || err)}`);
  }
}

function buildHealthDiagText() {
  const d = state.data || {};
  const h = d.health || {};
  const a = d.audit || {};
  const lines = [
    `[AGY Hyper Diagnostic Snapshot]`,
    `Profile: ${d.profile || "unknown"} | Model: ${h.active_model || "unknown"} | Health: ${h.level || "unknown"}`,
    `Context: ${fmtTokens(h.context_tokens || 0)} / ${fmtTokens(h.context_limit || 0)} | Cache Hit: ${Number(h.cache_hit_pct || 0).toFixed(1)}% | Cache Age: ${Math.round(h.cache_age_secs || 0)}s / TTL ${Math.round(h.cache_ttl_secs || 300)}s (${h.cache_ttl_source || "default"})`,
  ];
  if (a.grade) {
    lines.push(
      `Audit Grade: ${a.grade} | 1st Edit P50: ${fmtTokens(a.first_edit_p50_tokens || 0)} | Split Cache Drop: ${Number(a.split_cache_drop_pct || 0).toFixed(1)}%`
    );
  }
  if (Array.isArray(a.top_findings) && a.top_findings.length) {
    lines.push(`Findings: ${a.top_findings.slice(0, 3).join(" ; ")}`);
  }
  return lines.join("\n");
}

function buildSubagentRescuePrompt(sub) {
  if (sub?.rescue_prompt && String(sub.rescue_prompt).trim()) {
    return String(sub.rescue_prompt).trim();
  }
  const role = sub?.role || "subagent";
  const id = sub?.id || "unknown";
  if ((sub?.crash_loop_count || 0) > 0) {
    return `Subagent ${role} #${id} hit ${sub.crash_loop_count} consecutive errors. Inspect what it completed, stop the stuck subagent, and continue only the remaining work in a fresh narrower subagent.`;
  }
  return `Subagent ${role} #${id} appears stalled (context ${fmtTokens(sub?.context_tokens || 0)}). Inspect its progress, stop it if hung, and continue only the remaining work.`;
}

function buildSubagentDiagText(sub) {
  return [
    `[AGY Hyper Subagent Watchdog Alert]`,
    `Subagent: ${sub.role || "subagent"} #${sub.id}`,
    `State: ${sub.state || "unknown"} | Stalled: ${Boolean(sub.stalled)} | Crash Loop Count: ${sub.crash_loop_count || 0} | Context: ${fmtTokens(sub.context_tokens || 0)}`,
    `Suggested action: ${buildSubagentRescuePrompt(sub)}`,
  ].join("\n");
}

async function toggleTxDiff(txId) {
  if (!txId) return;
  if (state.openDiffs.has(txId)) {
    state.openDiffs.delete(txId);
    render();
    return;
  }
  if (!hasServerToolsCap()) return;
  state.openDiffs.set(txId, { loading: true, error: "", data: null });
  render();
  try {
    const res = await app.callServerTool({
      name: "hyper_app_tx_diff",
      arguments: { tx_id: txId },
    });
    if (res?.isError) throw new Error(extractTextContent(res) || "diff error");
    const sc = extractStructured(res) || {};
    if (state.openDiffs.has(txId)) {
      state.openDiffs.set(txId, {
        loading: false,
        error: "",
        data: {
          tx_id: sc.tx_id || txId,
          diff_text: String(sc.diff_text || ""),
          files: Array.isArray(sc.files) ? sc.files : [],
        },
      });
      render();
    }
  } catch (err) {
    if (state.openDiffs.has(txId)) {
      state.openDiffs.set(txId, {
        loading: false,
        error: String(err?.message || err),
        data: null,
      });
      render();
    }
  }
}

async function performTxRollback(txId) {
  if (!txId || !hasServerToolsCap() || state.rollingBackTxId) return;
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
      throw new Error(extractTextContent(res) || t("toastRollbackErr"));
    }
    const ok = sc ? Boolean(sc.ok) : !res?.isError;
    if (ok) {
      const tx = (state.data?.recent_txs || []).find((x) => x.tx_id === txId);
      if (tx) tx.rolled_back = true;
      showToast(`${t("toastRollbackOk")} ${txId}`);
    } else {
      showToast(`${t("toastRollbackErr")}: ${sc?.message || ""}`);
    }
  } catch (err) {
    showToast(`${t("toastRollbackErr")}: ${String(err?.message || err)}`);
  } finally {
    state.rollingBackTxId = "";
    render();
  }
}

function computeWarmthSnapshot() {
  const h = state.data?.health || {};
  const ttl = Math.max(30, Number(h.cache_ttl_secs) || 300);
  const baseAge = Math.max(0, Number(h.cache_age_secs) || 0);
  const elapsedSinceFetch = state.receivedAtMs
    ? Math.max(0, Math.floor((Date.now() - state.receivedAtMs) / 1000))
    : 0;
  const liveAge = baseAge + elapsedSinceFetch;
  const remaining = Math.max(0, ttl - liveAge);
  const pct = Math.max(0, Math.min(100, Math.round((remaining / ttl) * 100)));
  const coolThreshold = Math.max(15, Math.floor(ttl * 0.5));
  const status = remaining === 0 ? "err" : remaining <= coolThreshold ? "warn" : "ok";
  const label = remaining === 0 ? t("cold") : remaining <= coolThreshold ? t("cooling") : t("warm");
  const hint = remaining === 0 ? t("hintCold") : remaining <= coolThreshold ? t("hintCooling") : t("hintWarm");
  return { ttl, remaining, pct, status, label, hint, source: h.cache_ttl_source || "default" };
}

function updateLiveWarmthDom() {
  if (!state.data) return;
  const w = computeWarmthSnapshot();
  const clockEl = document.getElementById("cp-warmth-clock");
  const pillEl = document.getElementById("cp-warmth-pill");
  const fillEl = document.getElementById("cp-warmth-fill");
  const hintEl = document.getElementById("cp-warmth-hint");
  if (clockEl) clockEl.textContent = `${fmtDurationSec(w.remaining)} / ${fmtDurationSec(w.ttl)}`;
  if (pillEl) {
    pillEl.className = `status-pill ${w.status}`;
    pillEl.textContent = w.label;
  }
  if (fillEl) {
    fillEl.className = `meter-fill ${w.status}`;
    fillEl.style.width = `${w.remaining === 0 ? 3 : w.pct}%`;
  }
  if (hintEl) hintEl.textContent = w.hint;
}

function mapHealthLevel(level) {
  const lv = String(level || "healthy").toLowerCase();
  if (lv.includes("red") || lv.includes("crit") || lv.includes("bleed")) {
    return { cls: "err", label: t("healthRedline") };
  }
  if (lv.includes("loop") || lv.includes("err")) {
    return { cls: "err", label: t("healthLoop") };
  }
  if (lv.includes("warn") || lv.includes("warm") || lv.includes("watch")) {
    return { cls: "warn", label: t("healthWarming") };
  }
  if (lv.includes("compact")) {
    return { cls: "info", label: t("healthCompacted") };
  }
  return { cls: "ok", label: t("healthHealthy") };
}

function mapAuditGrade(grade) {
  const g = String(grade || "HEALTHY").toUpperCase();
  if (g === "BLEEDING" || g === "CRITICAL" || g === "D" || g === "F") {
    return { cls: "err", label: t("gradeBleeding") };
  }
  if (g === "WATCH" || g === "WARN" || g === "C") {
    return { cls: "warn", label: t("gradeWatch") };
  }
  return { cls: "ok", label: t("gradeHealthy") };
}

function renderUsageChartSvg(daily, currency) {
  if (!Array.isArray(daily) || !daily.length) {
    return `<div class="empty-state">${escapeHtml(t("chartEmpty"))}</div>`;
  }
  const W = 420;
  const H = 144;
  const padL = 32;
  const padR = 28;
  const padT = 14;
  const padB = 22;
  const plotW = W - padL - padR;
  const plotH = H - padT - padB;
  const isCost = state.usageMetric === "cost";
  const vals = daily.map((d) => (isCost ? Number(d.cost_usd || 0) : Number(d.tokens || 0)));
  const maxVal = Math.max(...vals, isCost ? 0.01 : 1000);
  const n = daily.length;
  const slotW = plotW / n;
  const barW = Math.max(4, Math.min(28, slotW * 0.62));

  const gridLines = [0, 0.5, 1]
    .map((f) => {
      const y = (padT + plotH * (1 - f)).toFixed(1);
      const leftLbl = isCost ? fmtCurrency(maxVal * f, currency) : fmtTokens(maxVal * f);
      const rightLbl = `${Math.round(f * 100)}%`;
      return `
        <line x1="${padL}" y1="${y}" x2="${W - padR}" y2="${y}" stroke="var(--line)" stroke-dasharray="${f === 0 ? "" : "2 3"}" />
        <text x="${padL - 4}" y="${Number(y) + 3}" text-anchor="end" font-size="8.5" fill="var(--fg-subtle)" font-family="var(--app-font-mono)">${escapeHtml(leftLbl)}</text>
        <text x="${W - padR + 4}" y="${Number(y) + 3}" text-anchor="start" font-size="8.5" fill="var(--ok)" opacity="0.85" font-family="var(--app-font-mono)">${rightLbl}</text>`;
    })
    .join("");

  const bars = [];
  const sparkPts = [];
  const xLabels = [];
  const stepLbl = n <= 7 ? 1 : Math.ceil(n / 6);

  daily.forEach((d, idx) => {
    const cx = padL + slotW * idx + slotW / 2;
    const v = vals[idx];
    const barH = Math.max(2, (v / maxVal) * plotH);
    const bx = (cx - barW / 2).toFixed(1);
    const by = (padT + plotH - barH).toFixed(1);
    const active = idx === state.hoveredDayIdx ? " active" : "";
    const shortDate = String(d.date || "").slice(5) || `#${idx + 1}`;
    const tip = `${d.date || shortDate} · ${fmtTokens(d.tokens || 0)} · ${Number(d.cache_hit_pct || 0).toFixed(0)}% · ${fmtCurrency(d.cost_usd || 0, currency)}`;
    bars.push(
      `<rect class="svg-bar${active}" data-day-idx="${idx}" x="${bx}" y="${by}" width="${barW.toFixed(1)}" height="${barH.toFixed(1)}" rx="2.5"><title>${escapeHtml(tip)}</title></rect>`
    );

    const hitPct = Math.max(0, Math.min(100, Number(d.cache_hit_pct || 0)));
    const py = (padT + plotH * (1 - hitPct / 100)).toFixed(1);
    sparkPts.push(`${cx.toFixed(1)},${py}`);

    if (idx % stepLbl === 0 || idx === n - 1) {
      xLabels.push(
        `<text x="${cx.toFixed(1)}" y="${H - 5}" text-anchor="middle" font-size="8.5" fill="var(--fg-subtle)" font-family="var(--app-font-mono)">${escapeHtml(shortDate)}</text>`
      );
    }
  });

  const sparkLine =
    sparkPts.length > 1
      ? `<polyline fill="none" stroke="var(--ok)" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" points="${sparkPts.join(" ")}" pointer-events="none" />`
      : "";

  const sparkDots = sparkPts
    .map((pt, idx) => {
      const [x, y] = pt.split(",");
      const r = idx === state.hoveredDayIdx ? 3.5 : 2.1;
      return `<circle cx="${x}" cy="${y}" r="${r}" fill="var(--ok)" stroke="var(--surface)" stroke-width="1" pointer-events="none" />`;
    })
    .join("");

  const activeDay = daily[state.hoveredDayIdx >= 0 ? state.hoveredDayIdx : daily.length - 1];
  const readout = activeDay
    ? `${activeDay.date || ""} · ${fmtTokens(activeDay.tokens || 0)} · ${Number(activeDay.cache_hit_pct || 0).toFixed(0)}% · ${fmtCurrency(activeDay.cost_usd || 0, currency)}`
    : "";

  return `
    <div class="chart-box">
      <div class="chart-meta-bar">
        <span>${escapeHtml(t("chartLegendBar"))} ${escapeHtml(isCost ? t("metricCost") : t("metricTokens"))} · <span style="color:var(--ok)">${escapeHtml(t("chartLegendLine"))}</span></span>
        <span id="cp-chart-readout" class="chart-hover-readout">${escapeHtml(readout)}</span>
      </div>
      <svg class="usage-svg" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" role="img" aria-label="Daily usage and cache hit rate">
        ${gridLines}
        ${bars.join("")}
        ${sparkLine}
        ${sparkDots}
        ${xLabels.join("")}
      </svg>
    </div>`;
}

function render() {
  const d = state.data;
  const h = d?.health || {};
  const u = d?.usage || {};
  const a = d?.audit || {};
  const subs = Array.isArray(d?.subagents) ? d.subagents : [];
  const txs = Array.isArray(d?.recent_txs) ? d.recent_txs : [];
  const healthBadge = mapHealthLevel(h.level);
  const wsName = pathBasename(d?.workspace || "");
  const surfaceTag = hasMessageCap() ? t("surfacePane") : t("surfaceGlobal");

  const headerHtml = `
    <header class="cp-topbar">
      <div class="cp-brand">
        ${BRAND_LOGO_SVG}
        <div class="cp-brand-copy">
          <div class="cp-brand-row">
            <span class="cp-brand-name">${escapeHtml(t("brand"))}</span>
            ${d?.version ? `<span class="mono-tag">v${escapeHtml(String(d.version).replace(/^v/i, ""))}</span>` : ""}
            <span class="status-pill ${healthBadge.cls}">${escapeHtml(healthBadge.label)}</span>
          </div>
          <div class="cp-brand-sub">
            <span class="cp-dot ${healthBadge.cls}"></span>
            <span>${escapeHtml(wsName || h.active_model || surfaceTag)}</span>
            <span class="mono-tag">${escapeHtml(surfaceTag)}</span>
          </div>
        </div>
      </div>
      <div class="cp-controls">
        ${
          hasServerToolsCap()
            ? `<button type="button" class="btn-ghost" data-refresh="1" ${state.refreshing ? "disabled" : ""}>
                <svg class="${state.refreshing ? "spinning" : ""}" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="23 4 23 10 17 10"/><path d="M20.49 15a9 9 0 1 1-2.12-9.36L23 10"/></svg>
                <span>${escapeHtml(state.refreshing ? t("refreshing") : t("refresh"))}</span>
              </button>`
            : ""
        }
        <div class="lang-switch" role="group" aria-label="Language">
          <button type="button" class="lang-btn ${state.lang === "en" ? "active" : ""}" data-set-lang="en">EN</button>
          <button type="button" class="lang-btn ${state.lang === "zh" ? "active" : ""}" data-set-lang="zh">中</button>
        </div>
      </div>
    </header>`;

  const currentProfile = String(d?.profile || "balanced").toLowerCase();
  const profileBarHtml = `
    <div class="profile-bar" role="radiogroup" aria-label="Safety Profile">
      ${PROFILES.map((p) => {
        const active = currentProfile === p.id;
        const disabled = !hasServerToolsCap() || Boolean(state.switchingProfile);
        const title = state.lang === "zh" ? p.zhTitle : p.enTitle;
        const desc = state.lang === "zh" ? p.zhDesc : p.enDesc;
        return `
          <button type="button" class="profile-btn ${active ? "active" : ""}" data-profile="${p.id}" role="radio" aria-checked="${String(active)}" ${disabled ? "disabled" : ""}>
            <strong>${escapeHtml(title)}</strong>
            <span>${escapeHtml(desc)}</span>
          </button>`;
      }).join("")}
    </div>`;

  if (state.loading && !d) {
    rootEl.innerHTML = `<div class="cockpit-shell">${headerHtml}<div class="panel"><div class="empty-state">${escapeHtml(t("connecting"))}</div></div></div>`;
    return;
  }

  if (state.cancelled && !d) {
    rootEl.innerHTML = `<div class="cockpit-shell">${headerHtml}<div class="panel"><div class="empty-state">${escapeHtml(t("cancelled"))}</div></div></div>`;
    return;
  }

  if (state.errorText && !d) {
    rootEl.innerHTML = `<div class="cockpit-shell">${headerHtml}<div class="panel"><div class="empty-state">${escapeHtml(state.errorText)}</div></div></div>`;
    return;
  }

  // Context & Warmth calculations
  const ctxTok = Number(h.context_tokens || 0);
  const ctxLim = Math.max(1, Number(h.context_limit || 200_000));
  const ctxPct = Math.max(0, Math.min(100, Math.round((ctxTok / ctxLim) * 100)));
  const ctxState = ctxPct >= 88 ? "err" : ctxPct >= 70 ? "warn" : "ok";
  const cacheHitPct = Number(h.cache_hit_pct ?? 0);
  const cacheCls = cacheHitPct >= 70 ? "ok" : cacheHitPct >= 40 ? "warn" : "err";
  const w = computeWarmthSnapshot();
  const ttlBadgeText = `${fmtDurationSec(w.ttl)} ${w.source === "empirical" ? t("ttlEmpirical") : t("ttlDefault")}`;

  const kpiStripHtml = `
    <section class="kpi-grid-4" aria-label="Telemetry Overview">
      <div class="kpi-cell">
        <span class="kpi-lbl">${escapeHtml(t("kpiContext"))}</span>
        <strong class="kpi-val ${ctxState}">${escapeHtml(fmtTokens(ctxTok))}</strong>
        <span class="kpi-sub">${ctxPct}% / ${escapeHtml(fmtTokens(ctxLim))}</span>
      </div>
      <div class="kpi-cell">
        <span class="kpi-lbl">${escapeHtml(t("kpiCacheHit"))}</span>
        <strong class="kpi-val ${cacheCls}">${cacheHitPct.toFixed(1)}%</strong>
        <span class="kpi-sub">${escapeHtml(ttlBadgeText)}</span>
      </div>
      <div class="kpi-cell">
        <span class="kpi-lbl">${escapeHtml(t("kpiModel"))}</span>
        <strong class="kpi-val" title="${escapeHtml(h.active_model || "—")}">${escapeHtml(h.active_model || "—")}</strong>
        <span class="kpi-sub">${escapeHtml(currentProfile)}</span>
      </div>
      <div class="kpi-cell">
        <span class="kpi-lbl">${escapeHtml(t("kpiCost"))} (${escapeHtml(state.usageWindow.toUpperCase())})</span>
        <strong class="kpi-val accent">${escapeHtml(fmtCurrency(u.est_cost_usd, u.currency || "USD"))}</strong>
        <span class="kpi-sub">${escapeHtml(fmtTokens((u.total_input || 0) + (u.total_cache_read || 0) + (u.total_output || 0)))} tok</span>
      </div>
    </section>`;

  // Live Cache Warmth & Context Meter panel
  const convMeterActions = [];
  if (hasMessageCap()) {
    convMeterActions.push(
      `<button type="button" class="btn-accent btn-xs" data-send-compact="1">${escapeHtml(t("sendCompact"))}</button>`
    );
  }
  if (hasUpdateContextCap()) {
    convMeterActions.push(
      `<button type="button" class="btn-ghost btn-xs" data-attach-health="1">${escapeHtml(t("attachDiag"))}</button>`
    );
  }

  const warmthPanelHtml = `
    <section class="panel">
      <div class="panel-head">
        <h2 class="panel-title">${escapeHtml(t("warmthHeading"))}</h2>
        <span class="mono-tag">${escapeHtml(ttlBadgeText)}</span>
      </div>
      <div class="meter-group">
        <div class="meter-row">
          <div class="meter-label-row">
            <div class="meter-left">
              <span style="font-size:11.5px;font-weight:600">${escapeHtml(t("contextBar"))}</span>
              <span class="status-pill ${ctxState}">${ctxPct}%</span>
            </div>
            <span class="meter-readout">${escapeHtml(fmtTokens(ctxTok))} / ${escapeHtml(fmtTokens(ctxLim))}</span>
          </div>
          <div class="meter-track" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${ctxPct}">
            <div class="meter-fill ${ctxState}" style="width:${Math.max(2, ctxPct)}%"></div>
            <span class="meter-tick" style="left:70%" title="70%"></span>
            <span class="meter-tick" style="left:88%" title="88%"></span>
          </div>
        </div>

        <div class="meter-row">
          <div class="meter-label-row">
            <div class="meter-left">
              <span style="font-size:11.5px;font-weight:600">${escapeHtml(t("cacheBar"))}</span>
              <span id="cp-warmth-pill" class="status-pill ${w.status}">${escapeHtml(w.label)}</span>
            </div>
            <span id="cp-warmth-clock" class="meter-readout">${escapeHtml(fmtDurationSec(w.remaining))} / ${escapeHtml(fmtDurationSec(w.ttl))}</span>
          </div>
          <div class="meter-track" role="progressbar" aria-valuemin="0" aria-valuemax="${w.ttl}" aria-valuenow="${w.remaining}">
            <div id="cp-warmth-fill" class="meter-fill ${w.status}" style="width:${w.remaining === 0 ? 3 : w.pct}%"></div>
            <span class="meter-tick" style="left:50%" title="50%"></span>
          </div>
        </div>
      </div>
      <div class="meter-foot">
        <span id="cp-warmth-hint" class="meter-hint">${escapeHtml(w.hint)}</span>
        ${convMeterActions.length ? `<div class="meter-actions">${convMeterActions.join("")}</div>` : ""}
      </div>
    </section>`;

  // Subagent Watchdog Strip
  const stalledCount = subs.filter((s) => s.stalled || String(s.state || "").toLowerCase() === "stalled").length;
  const loopCount = subs.filter((s) => (s.crash_loop_count || 0) > 0 || String(s.state || "").toLowerCase() === "error_loop").length;
  const activeCount = subs.filter((s) => !["done", "completed", "killed"].includes(String(s.state || "").toLowerCase())).length;

  const subagentCardsHtml = subs.length
    ? `<div class="subagent-list">
        ${subs
          .map((sub, idx) => {
            const isLoop = (sub.crash_loop_count || 0) > 0 || String(sub.state || "").toLowerCase() === "error_loop";
            const isStalled = Boolean(sub.stalled) || String(sub.state || "").toLowerCase() === "stalled";
            const cardCls = isLoop ? "crash-loop" : isStalled ? "stalled" : "";
            const rescuePrompt = buildSubagentRescuePrompt(sub);
            const showRescue = isLoop || isStalled || Boolean(sub.rescue_prompt);

            let statePill = `<span class="status-pill muted">${escapeHtml(sub.state || "idle")}</span>`;
            if (isLoop) {
              statePill = `<span class="status-pill err">${escapeHtml(t("loopPill"))} ×${sub.crash_loop_count || 1}</span>`;
            } else if (isStalled) {
              statePill = `<span class="status-pill warn">${escapeHtml(t("stalledPill"))}</span>`;
            } else if (String(sub.state || "").toLowerCase() === "running" || String(sub.state || "").toLowerCase() === "working") {
              statePill = `<span class="status-pill ok">${escapeHtml(sub.state)}</span>`;
            }

            const actionBtns = [];
            if (showRescue) {
              if (hasMessageCap()) {
                actionBtns.push(
                  `<button type="button" class="btn-danger btn-xs" data-rescue-send="${idx}">${escapeHtml(t("rescueSend"))}</button>`
                );
              }
              if (hasUpdateContextCap()) {
                actionBtns.push(
                  `<button type="button" class="btn-ghost btn-xs" data-rescue-attach="${idx}">${escapeHtml(t("rescueAttach"))}</button>`
                );
              }
              actionBtns.push(
                `<button type="button" class="btn-ghost btn-xs" data-rescue-copy="${idx}">${escapeHtml(t("rescueCopy"))}</button>`
              );
            }

            return `
              <div class="subagent-card ${cardCls}">
                <div class="subagent-top">
                  <div class="subagent-id-group">
                    <span class="subagent-role">${escapeHtml(sub.role || "worker")}</span>
                    <span class="subagent-id">#${escapeHtml(String(sub.id || idx).slice(0, 14))}</span>
                  </div>
                  <div class="subagent-right">
                    ${statePill}
                    <span class="mono-tag">${escapeHtml(fmtTokens(sub.context_tokens || 0))}</span>
                  </div>
                </div>
                ${
                  showRescue
                    ? `<div class="subagent-actions">
                        <span class="subagent-prompt-preview" title="${escapeHtml(rescuePrompt)}">${escapeHtml(rescuePrompt)}</span>
                        <div class="subagent-btns">${actionBtns.join("")}</div>
                      </div>`
                    : ""
                }
              </div>`;
          })
          .join("")}
      </div>`
    : `<div class="empty-state">${escapeHtml(t("subagentsEmpty"))}</div>`;

  const subagentPanelHtml = `
    <section class="panel">
      <div class="panel-head">
        <h2 class="panel-title">
          <span>${escapeHtml(t("subagentsHeading"))}</span>
          <span class="mono-tag">${subs.length}</span>
        </h2>
        <div style="display:inline-flex;align-items:center;gap:5px;flex-wrap:wrap">
          ${activeCount > 0 ? `<span class="status-pill ok">${activeCount} ${escapeHtml(t("activeBadge"))}</span>` : ""}
          ${stalledCount > 0 ? `<span class="status-pill warn">${stalledCount} ${escapeHtml(t("stalledBadge"))}</span>` : ""}
          ${loopCount > 0 ? `<span class="status-pill err">${loopCount} ${escapeHtml(t("loopBadge"))}</span>` : ""}
        </div>
      </div>
      ${subagentCardsHtml}
    </section>`;

  // Multi-day Usage Ledger & Burn Audit Summary
  const grade = mapAuditGrade(a.grade);
  const findings = Array.isArray(a.top_findings) ? a.top_findings : [];
  const daily = Array.isArray(u.daily) ? u.daily : [];

  const usageAuditPanelHtml = `
    <section class="panel">
      <div class="panel-head">
        <h2 class="panel-title">
          <span>${escapeHtml(t("usageHeading"))}</span>
          <span class="status-pill ${grade.cls}">${escapeHtml(grade.label)}</span>
        </h2>
        <div style="display:inline-flex;align-items:center;gap:6px;flex-wrap:wrap">
          <div class="seg-inline" role="tablist" aria-label="Usage window">
            ${WINDOWS.map(
              (win) =>
                `<button type="button" class="win-btn ${state.usageWindow === win ? "active" : ""}" data-uwin="${win}" ${state.refreshing ? "disabled" : ""}>${win.toUpperCase()}</button>`
            ).join("")}
          </div>
          <div class="seg-inline" role="tablist" aria-label="Chart metric">
            <button type="button" class="win-btn ${state.usageMetric === "cost" ? "active" : ""}" data-umetric="cost">${escapeHtml(t("metricCost"))}</button>
            <button type="button" class="win-btn ${state.usageMetric === "tokens" ? "active" : ""}" data-umetric="tokens">${escapeHtml(t("metricTokens"))}</button>
          </div>
        </div>
      </div>

      <div class="usage-stats-row">
        <div class="usage-mini">
          <span>${escapeHtml(t("statInput"))}</span>
          <strong>${escapeHtml(fmtTokens(u.total_input || 0))}</strong>
        </div>
        <div class="usage-mini">
          <span>${escapeHtml(t("statCacheRead"))}</span>
          <strong style="color:var(--ok)">${escapeHtml(fmtTokens(u.total_cache_read || 0))}</strong>
        </div>
        <div class="usage-mini">
          <span>${escapeHtml(t("statOutput"))}</span>
          <strong>${escapeHtml(fmtTokens(u.total_output || 0))}</strong>
        </div>
      </div>

      ${renderUsageChartSvg(daily, u.currency || "USD")}

      <div style="height:10px"></div>

      <div class="audit-chips">
        <span class="mono-tag">${escapeHtml(t("auditCache"))}: ${Number(a.cache_hit_pct ?? 0).toFixed(1)}%</span>
        <span class="mono-tag">${escapeHtml(t("auditFirstEdit"))}: ${escapeHtml(fmtTokens(a.first_edit_p50_tokens || 0))}</span>
        <span class="mono-tag">${escapeHtml(t("auditSplitDrop"))}: -${Number(a.split_cache_drop_pct ?? 0).toFixed(1)}%</span>
      </div>

      ${
        findings.length
          ? `<div class="audit-findings">
              ${findings
                .map(
                  (f) => `<div class="finding-row"><span class="finding-dot ${grade.cls}"></span><span>${escapeHtml(f)}</span></div>`
                )
                .join("")}
            </div>`
          : `<div class="empty-state">${escapeHtml(t("findingsEmpty"))}</div>`
      }
    </section>`;

  // Recent WAL Transactions Quick List
  const txListHtml = txs.length
    ? `<div class="cp-tx-list">
        ${txs
          .map((tx) => {
            const diffState = state.openDiffs.get(tx.tx_id);
            const isConfirming = state.confirmRollbackTxId === tx.tx_id;
            const isRolling = state.rollingBackTxId === tx.tx_id;
            const isNative = String(tx.source || "").toLowerCase().includes("native");

            let rbCtrl = "";
            if (tx.rolled_back) {
              rbCtrl = `<span class="status-pill muted">${escapeHtml(t("rolledBack"))}</span>`;
            } else if (hasServerToolsCap()) {
              if (isRolling) {
                rbCtrl = `<button type="button" class="btn-danger btn-xs" disabled>…</button>`;
              } else if (isConfirming) {
                rbCtrl = `
                  <button type="button" class="btn-ghost btn-xs" data-cancel-tx-rb="1">${escapeHtml(t("cancelBtn"))}</button>
                  <button type="button" class="btn-danger solid btn-xs" data-confirm-tx-rb="${escapeHtml(tx.tx_id)}">${escapeHtml(t("confirmBtn"))}</button>`;
              } else {
                rbCtrl = `<button type="button" class="btn-danger btn-xs" data-arm-tx-rb="${escapeHtml(tx.tx_id)}">${escapeHtml(t("rollbackBtn"))}</button>`;
              }
            }

            let drawerHtml = "";
            if (diffState) {
              if (diffState.loading) {
                drawerHtml = `<div class="cp-tx-diff-drawer"><div class="empty-state">${escapeHtml(t("diffLoading"))}</div></div>`;
              } else if (diffState.error) {
                drawerHtml = `<div class="cp-tx-diff-drawer"><div class="empty-state">${escapeHtml(diffState.error)}</div></div>`;
              } else {
                const secs = parseUnifiedDiff(diffState.data?.diff_text || "", diffState.data?.files || []);
                drawerHtml = `<div class="cp-tx-diff-drawer">${renderDiffSectionsHtml(
                  secs,
                  state.collapsedFiles,
                  t("diffEmpty"),
                  t("openFileTip")
                )}</div>`;
              }
            }

            return `
              <div class="cp-tx-card">
                <div class="cp-tx-row">
                  <div class="cp-tx-main">
                    <div class="cp-tx-top">
                      <span class="cp-tx-id">${escapeHtml(tx.tx_id)}</span>
                      <span class="status-pill ${isNative ? "indigo" : "info"}">${escapeHtml(tx.source || "hyper_edit")}</span>
                    </div>
                    <span class="cp-tx-summary" title="${escapeHtml(tx.summary || "")}">${escapeHtml(tx.summary || tx.tx_id)}</span>
                    <span class="cp-tx-meta">${escapeHtml(fmtRelTime(tx.created_at, state.lang))} · ${Number(tx.files_count ?? (Array.isArray(tx.files) ? tx.files.length : 0))} ${escapeHtml(t("filesUnit"))}</span>
                  </div>
                  <div class="cp-tx-actions">
                    ${
                      hasServerToolsCap()
                        ? `<button type="button" class="btn-ghost btn-xs" data-toggle-tx-diff="${escapeHtml(tx.tx_id)}">${escapeHtml(diffState ? t("diffHide") : t("diffShow"))}</button>`
                        : ""
                    }
                    ${rbCtrl}
                  </div>
                </div>
                ${drawerHtml}
              </div>`;
          })
          .join("")}
      </div>`
    : `<div class="empty-state">${escapeHtml(t("txEmpty"))}</div>`;

  const txPanelHtml = `
    <section class="panel">
      <div class="panel-head">
        <h2 class="panel-title">
          <span>${escapeHtml(t("txHeading"))}</span>
          <span class="mono-tag">${txs.length}</span>
        </h2>
      </div>
      ${txListHtml}
    </section>`;

  rootEl.innerHTML = `
    <div class="cockpit-shell">
      ${headerHtml}
      ${profileBarHtml}
      ${kpiStripHtml}
      <div class="cp-grid">
        <div class="cp-stack">
          ${warmthPanelHtml}
          ${usageAuditPanelHtml}
        </div>
        <div class="cp-stack">
          ${subagentPanelHtml}
          ${txPanelHtml}
        </div>
      </div>
    </div>`;
}

// Delegated click & hover interactions
rootEl.addEventListener("click", async (ev) => {
  const langBtn = ev.target.closest("[data-set-lang]");
  if (langBtn) {
    state.lang = langBtn.getAttribute("data-set-lang") === "zh" ? "zh" : "en";
    state.langManual = true;
    render();
    return;
  }

  if (ev.target.closest("[data-refresh]")) {
    fetchCockpitData(state.usageWindow);
    return;
  }

  const profBtn = ev.target.closest("[data-profile]");
  if (profBtn) {
    setProfile(profBtn.getAttribute("data-profile") || "");
    return;
  }

  if (ev.target.closest("[data-send-compact]")) {
    sendConversationMessage("/compact", t("toastCompactSent"));
    return;
  }

  if (ev.target.closest("[data-attach-health]")) {
    attachDiagnosticContext(buildHealthDiagText());
    return;
  }

  const rescueSendBtn = ev.target.closest("[data-rescue-send]");
  if (rescueSendBtn) {
    const idx = Number(rescueSendBtn.getAttribute("data-rescue-send"));
    const sub = state.data?.subagents?.[idx];
    if (sub) {
      sendConversationMessage(buildSubagentRescuePrompt(sub), t("toastRescueSent"));
    }
    return;
  }

  const rescueAttachBtn = ev.target.closest("[data-rescue-attach]");
  if (rescueAttachBtn) {
    const idx = Number(rescueAttachBtn.getAttribute("data-rescue-attach"));
    const sub = state.data?.subagents?.[idx];
    if (sub) {
      attachDiagnosticContext(buildSubagentDiagText(sub));
    }
    return;
  }

  const rescueCopyBtn = ev.target.closest("[data-rescue-copy]");
  if (rescueCopyBtn) {
    const idx = Number(rescueCopyBtn.getAttribute("data-rescue-copy"));
    const sub = state.data?.subagents?.[idx];
    if (sub) {
      const ok = await copyToClipboard(buildSubagentRescuePrompt(sub));
      showToast(ok ? t("toastCopied") : t("toastCopyFail"));
    }
    return;
  }

  const winBtn = ev.target.closest("[data-uwin]");
  if (winBtn) {
    const nextWin = winBtn.getAttribute("data-uwin") || "7d";
    if (nextWin !== state.usageWindow) {
      fetchCockpitData(nextWin);
    }
    return;
  }

  const metricBtn = ev.target.closest("[data-umetric]");
  if (metricBtn) {
    state.usageMetric = metricBtn.getAttribute("data-umetric") === "tokens" ? "tokens" : "cost";
    render();
    return;
  }

  const diffBtn = ev.target.closest("[data-toggle-tx-diff]");
  if (diffBtn) {
    toggleTxDiff(diffBtn.getAttribute("data-toggle-tx-diff") || "");
    return;
  }

  const armRb = ev.target.closest("[data-arm-tx-rb]");
  if (armRb) {
    state.confirmRollbackTxId = armRb.getAttribute("data-arm-tx-rb") || "";
    render();
    return;
  }

  if (ev.target.closest("[data-cancel-tx-rb]")) {
    state.confirmRollbackTxId = "";
    render();
    return;
  }

  const confirmRb = ev.target.closest("[data-confirm-tx-rb]");
  if (confirmRb) {
    performTxRollback(confirmRb.getAttribute("data-confirm-tx-rb") || "");
    return;
  }

  const toggleFile = ev.target.closest("[data-toggle-file]");
  if (toggleFile) {
    const key = toggleFile.getAttribute("data-toggle-file") || "";
    if (state.collapsedFiles.has(key)) state.collapsedFiles.delete(key);
    else state.collapsedFiles.add(key);
    render();
    return;
  }

  const openFile = ev.target.closest("[data-open-file]");
  if (openFile) {
    const abs = openFile.getAttribute("data-open-file") || "";
    const out = await openFileInAuxPane(app, abs);
    if (out.status === "ok") showToast(`${t("toastOpened")}: ${pathBasename(abs)}`);
    else if (out.status === "unavailable") showToast(t("toastPaneUnavailable"));
  }
});

rootEl.addEventListener("mouseover", (ev) => {
  const bar = ev.target.closest("[data-day-idx]");
  if (!bar) return;
  const idx = Number(bar.getAttribute("data-day-idx"));
  if (!Number.isFinite(idx) || idx === state.hoveredDayIdx) return;
  state.hoveredDayIdx = idx;
  const daily = state.data?.usage?.daily || [];
  const d = daily[idx];
  const readout = document.getElementById("cp-chart-readout");
  if (d && readout) {
    readout.textContent = `${d.date || ""} · ${fmtTokens(d.tokens || 0)} · ${Number(d.cache_hit_pct || 0).toFixed(0)}% · ${fmtCurrency(d.cost_usd || 0, state.data?.usage?.currency || "USD")}`;
  }
});

// Register all handlers BEFORE app.connect()
app.ontoolinput = (params) => {
  state.receivedToolInput = true;
  const args = params?.arguments || {};
  if (args.usage_window && WINDOWS.includes(String(args.usage_window).toLowerCase())) {
    state.usageWindow = String(args.usage_window).toLowerCase();
  }
  state.cancelled = false;
  if (!state.data) {
    state.loading = true;
    render();
  }
};

app.ontoolresult = (result) => {
  state.receivedToolResult = true;
  if (result?.isError) {
    state.loading = false;
    state.errorText = extractTextContent(result) || "Cockpit tool failed";
    render();
    return;
  }
  const sc = extractStructured(result);
  if (sc) {
    applyCockpitPayload(sc);
  } else if (hasServerToolsCap()) {
    fetchCockpitData(state.usageWindow);
  } else {
    state.loading = false;
    state.errorText = extractTextContent(result) || "No telemetry payload received.";
    render();
  }
};

app.ontoolcancelled = () => {
  state.loading = false;
  state.cancelled = true;
  render();
};

app.onhostcontextchanged = (ctx) => {
  applyHostContext(ctx);
  if (ctx?.locale && !state.langManual) {
    state.lang = detectLocaleLang(ctx.locale);
    render();
  }
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
    render();

    // Live 1s cache warmth countdown tick (lightweight DOM-only update)
    setInterval(updateLiveWarmthDom, 1000);

    // If host opened the view without delivering toolinput/toolresult, fetch on demand
    setTimeout(() => {
      if (!state.receivedToolResult && !state.receivedToolInput && !state.data && hasServerToolsCap()) {
        fetchCockpitData(state.usageWindow);
      }
    }, 800);
  } catch (err) {
    state.loading = false;
    state.errorText = String(err?.message || err || "Failed to connect to MCP Apps host");
    render();
  }
}

boot();
