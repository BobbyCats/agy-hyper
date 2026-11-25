/**
 * Shared helpers for AGY Hyper MCP Apps (SEP-1865 + Antigravity extensions).
 * Zero external network calls, zero localStorage/IndexedDB usage.
 */

import {
  applyDocumentTheme,
  applyHostFonts,
  applyHostStyleVariables,
} from "@modelcontextprotocol/ext-apps/app-with-deps";

/**
 * Standard Schema v1 passthrough validator for host extension requests
 * (such as com.google.antigravity/toggle-aux-pane) without duplicating Zod.
 */
export const PASSTHROUGH_SCHEMA = {
  "~standard": {
    version: 1,
    vendor: "agy-hyper",
    validate(value) {
      return { value };
    },
  },
};

export const BRAND_LOGO_SVG = `
<svg width="20" height="20" viewBox="0 0 128 128" fill="none" aria-hidden="true">
  <rect x="8" y="8" width="112" height="112" rx="28" fill="#0A0D14" stroke="currentColor" stroke-opacity="0.24" stroke-width="3"/>
  <path d="M38 37L63 64L38 91H52L77 64L52 37H38Z" fill="#F8FAFC"/>
  <path d="M61 37L86 64L61 91H75L100 64L75 37H61Z" fill="#38BDF8"/>
  <rect x="26" y="62" width="15" height="4" rx="2" fill="#38BDF8"/>
</svg>`;

export function applyHostContext(ctx) {
  if (!ctx || typeof ctx !== "object") return;
  if (ctx.theme === "light" || ctx.theme === "dark") {
    applyDocumentTheme(ctx.theme);
  }
  if (ctx.styles?.variables && typeof ctx.styles.variables === "object") {
    applyHostStyleVariables(ctx.styles.variables);
  }
  if (ctx.styles?.css?.fonts && typeof ctx.styles.css.fonts === "string") {
    applyHostFonts(ctx.styles.css.fonts);
  }
}

export function detectLocaleLang(hostLocale) {
  const raw = String(
    hostLocale || (typeof navigator !== "undefined" ? navigator.language : "en") || "en"
  ).toLowerCase();
  return raw.startsWith("zh") ? "zh" : "en";
}

export function escapeHtml(str) {
  return String(str ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

export function toFileUri(absPath) {
  const raw = String(absPath || "").trim();
  if (!raw) return "";
  if (raw.startsWith("file://")) return raw;
  const normalized = raw.replace(/\\/g, "/");
  const prefixed = normalized.startsWith("/") ? normalized : `/${normalized}`;
  const encoded = prefixed
    .split("/")
    .map((seg, i) => (i === 1 && /^[a-zA-Z]:$/.test(seg) ? seg : encodeURIComponent(seg)))
    .join("/");
  return `file://${encoded}`;
}

export function pathBasename(p) {
  const s = String(p || "").replace(/[\\/]+$/, "");
  if (!s) return "";
  const parts = s.split(/[\\/]/);
  return parts[parts.length - 1] || s;
}

/**
 * Opens a workspace file in Antigravity's side pane using the host extension
 * `com.google.antigravity/toggle-aux-pane`. Gracefully ignores `-32601`
 * (method not found) on unsupported MCP Apps hosts.
 */
export async function openFileInAuxPane(app, absPath) {
  const uri = toFileUri(absPath);
  if (!uri) return { status: "invalid" };
  try {
    const res = await app.request(
      {
        method: "com.google.antigravity/toggle-aux-pane",
        params: { tabType: "fileView", uri },
      },
      PASSTHROUGH_SCHEMA
    );
    if (res && res.isError) {
      return { status: "unavailable", result: res };
    }
    return { status: "ok", result: res };
  } catch (err) {
    const code = err?.code;
    const msg = String(err?.message || err || "");
    if (code === -32601 || msg.includes("-32601") || /method not found/i.test(msg)) {
      return { status: "unsupported" };
    }
    return { status: "error", error: msg };
  }
}

export function extractStructured(result) {
  if (!result || typeof result !== "object") return null;
  if (result.structuredContent && typeof result.structuredContent === "object") {
    return result.structuredContent;
  }
  const text = extractTextContent(result);
  if (text && (text.startsWith("{") || text.startsWith("["))) {
    try {
      return JSON.parse(text);
    } catch {
      return null;
    }
  }
  return null;
}

export function extractTextContent(result) {
  if (!result || !Array.isArray(result.content)) return "";
  return result.content
    .filter((b) => b && b.type === "text" && typeof b.text === "string")
    .map((b) => b.text)
    .join("\n")
    .trim();
}

export function fmtTokens(n) {
  const v = Number(n) || 0;
  if (v >= 1_000_000) return `${(v / 1_000_000).toFixed(v >= 10_000_000 ? 1 : 2)}M`;
  if (v >= 1_000) return `${(v / 1_000).toFixed(v >= 100_000 ? 0 : 1)}k`;
  return String(Math.round(v));
}

export function fmtCurrency(amount, currency = "USD") {
  const v = Number(amount);
  if (!Number.isFinite(v)) return "—";
  const ccy = String(currency || "USD").toUpperCase();
  const sym =
    ccy === "USD" ? "$" :
    ccy === "CNY" || ccy === "JPY" ? "¥" :
    ccy === "EUR" ? "€" :
    ccy === "GBP" ? "£" :
    `${ccy} `;
  if (Math.abs(v) < 0.01 && v > 0) return `<${sym}0.01`;
  return `${sym}${v.toFixed(v >= 100 ? 1 : 2)}`;
}

export function fmtRelTime(input, lang = "en") {
  if (input === null || input === undefined || input === "") return "—";
  let ms = 0;
  if (typeof input === "number") {
    ms = input > 1e12 ? input : input * 1000;
  } else {
    const parsed = Date.parse(String(input));
    if (!Number.isFinite(parsed)) return String(input);
    ms = parsed;
  }
  const diffSec = Math.max(0, Math.floor((Date.now() - ms) / 1000));
  if (diffSec < 10) return lang === "zh" ? "刚刚" : "just now";
  if (diffSec < 60) return lang === "zh" ? `${diffSec}秒前` : `${diffSec}s ago`;
  const mins = Math.floor(diffSec / 60);
  if (mins < 60) return lang === "zh" ? `${mins}分钟前` : `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return lang === "zh" ? `${hrs}小时前` : `${hrs}h ago`;
  const days = Math.floor(hrs / 24);
  return lang === "zh" ? `${days}天前` : `${days}d ago`;
}

export function fmtDurationSec(secs) {
  const s = Math.max(0, Math.round(Number(secs) || 0));
  if (s >= 60) {
    const m = Math.floor(s / 60);
    const rem = s % 60;
    return rem ? `${m}m${String(rem).padStart(2, "0")}s` : `${m}m`;
  }
  return `${s}s`;
}

export async function copyToClipboard(text) {
  const s = String(text || "");
  if (!s) return false;
  try {
    if (typeof navigator !== "undefined" && navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(s);
      return true;
    }
  } catch {
    // sandbox may restrict async clipboard; fall through to execCommand
  }
  try {
    const ta = document.createElement("textarea");
    ta.value = s;
    ta.setAttribute("readonly", "");
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    ta.style.pointerEvents = "none";
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand("copy");
    ta.remove();
    return Boolean(ok);
  } catch {
    return false;
  }
}

/**
 * Parses unified diff text into per-file sections with old/new line numbers.
 * Matches files metadata (abs_path, added, removed) when provided.
 */
export function parseUnifiedDiff(diffText, filesMeta = []) {
  const raw = String(diffText || "").replace(/\r\n/g, "\n");
  const metaList = Array.isArray(filesMeta) ? filesMeta : [];
  const metaByPath = new Map();
  for (const f of metaList) {
    const key = normalizeDiffPath(f.path || f.rel_path || "");
    if (key) metaByPath.set(key, f);
  }

  if (!raw.trim()) {
    return metaList.map((f) => ({
      path: f.path || f.rel_path || "(file)",
      abs_path: f.abs_path || "",
      added: Number(f.added ?? f.lines_added ?? 0),
      removed: Number(f.removed ?? f.lines_removed ?? 0),
      rows: [],
    }));
  }

  const lines = raw.split("\n");
  const sections = [];
  let current = null;
  let oldLn = 0;
  let newLn = 0;

  const ensureSection = (pathHint) => {
    const cleaned = normalizeDiffPath(pathHint) || (metaList[sections.length]?.path || `file-${sections.length + 1}`);
    const meta = metaByPath.get(cleaned) || metaList.find((m) => normalizeDiffPath(m.path || m.rel_path) === cleaned);
    const sec = {
      path: meta?.path || meta?.rel_path || cleaned,
      abs_path: meta?.abs_path || "",
      added: 0,
      removed: 0,
      metaAdded: meta ? Number(meta.added ?? meta.lines_added ?? 0) : null,
      metaRemoved: meta ? Number(meta.removed ?? meta.lines_removed ?? 0) : null,
      rows: [],
    };
    sections.push(sec);
    current = sec;
    return sec;
  };

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];

    if (line.startsWith("diff --git ")) {
      const m = line.match(/^diff --git a\/(.+?) b\/(.+)$/);
      ensureSection(m ? m[2] : line.slice(11));
      continue;
    }

    if (line.startsWith("--- ")) {
      const next = lines[i + 1] || "";
      if (next.startsWith("+++ ")) {
        const oldPath = normalizeDiffPath(line.slice(4));
        const newPath = normalizeDiffPath(next.slice(4));
        const chosen = newPath && newPath !== "/dev/null" ? newPath : oldPath;
        if (!current || (current.rows.length > 0 && normalizeDiffPath(current.path) !== chosen)) {
          ensureSection(chosen);
        } else if (!current.path || current.path.startsWith("file-")) {
          current.path = chosen || current.path;
        }
        i++; // consume +++ line
        continue;
      }
    }

    if (line.startsWith("+++ ") || line.startsWith("index ") || line.startsWith("new file mode") || line.startsWith("deleted file mode")) {
      continue;
    }

    if (line.startsWith("@@")) {
      if (!current) ensureSection(metaList[0]?.path || "diff");
      const m = line.match(/^@@\s+-(\d+)(?:,\d+)?\s+\+(\d+)(?:,\d+)?\s+@@/);
      if (m) {
        oldLn = parseInt(m[1], 10);
        newLn = parseInt(m[2], 10);
      }
      current.rows.push({ kind: "hunk", oldLn: "", newLn: "", sig: "", text: line });
      continue;
    }

    if (!current) {
      if (!line.trim()) continue;
      ensureSection(metaList[0]?.path || "diff");
    }

    if (line.startsWith("+")) {
      current.added++;
      current.rows.push({ kind: "add", oldLn: "", newLn: newLn || "", sig: "+", text: line.slice(1) });
      if (newLn) newLn++;
    } else if (line.startsWith("-")) {
      current.removed++;
      current.rows.push({ kind: "del", oldLn: oldLn || "", newLn: "", sig: "-", text: line.slice(1) });
      if (oldLn) oldLn++;
    } else if (line.startsWith("\\ No newline")) {
      current.rows.push({ kind: "ctx", oldLn: "", newLn: "", sig: "", text: line });
    } else {
      const txt = line.startsWith(" ") ? line.slice(1) : line;
      // Ignore single trailing empty line at end of unified diff
      if (i === lines.length - 1 && txt === "") continue;
      current.rows.push({ kind: "ctx", oldLn: oldLn || "", newLn: newLn || "", sig: " ", text: txt });
      if (oldLn) oldLn++;
      if (newLn) newLn++;
    }
  }

  for (const sec of sections) {
    if (sec.added === 0 && sec.metaAdded) sec.added = sec.metaAdded;
    if (sec.removed === 0 && sec.metaRemoved) sec.removed = sec.metaRemoved;
  }

  return sections;
}

function normalizeDiffPath(p) {
  let s = String(p || "").trim();
  if (s.includes("\t")) s = s.split("\t")[0].trim();
  if (s.startsWith('"') && s.endsWith('"')) s = s.slice(1, -1);
  if (s.startsWith("a/") || s.startsWith("b/")) s = s.slice(2);
  return s;
}

export function renderDiffSectionsHtml(sections, collapsedSet, emptyLabel, openTitle) {
  if (!sections || !sections.length) {
    return `<div class="empty-state">${escapeHtml(emptyLabel)}</div>`;
  }
  return sections
    .map((sec, idx) => {
      const key = sec.path || `file-${idx}`;
      const isCollapsed = collapsedSet?.has(key);
      const chevron = isCollapsed ? "▸" : "▾";
      const fileAttr = sec.abs_path ? `data-open-file="${escapeHtml(sec.abs_path)}"` : "";
      const pathBtn = sec.abs_path
        ? `<button type="button" class="file-link-btn" ${fileAttr} title="${escapeHtml(openTitle + ": " + sec.abs_path)}">
             <span class="file-link-path">${escapeHtml(sec.path)}</span>
             <svg class="file-link-ico" width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/><polyline points="15 3 21 3 21 9"/><line x1="10" y1="14" x2="21" y2="3"/></svg>
           </button>`
        : `<span class="file-link-btn" style="cursor:default"><span class="file-link-path">${escapeHtml(sec.path)}</span></span>`;

      const rowsHtml = sec.rows.length
        ? sec.rows
            .map((r) => {
              if (r.kind === "hunk") {
                return `<tr class="diff-row-hunk"><td class="diff-ln"></td><td class="diff-ln"></td><td class="diff-sig"></td><td class="diff-txt">${escapeHtml(r.text)}</td></tr>`;
              }
              return `<tr class="diff-row-${r.kind}"><td class="diff-ln">${r.oldLn}</td><td class="diff-ln">${r.newLn}</td><td class="diff-sig">${r.sig}</td><td class="diff-txt">${escapeHtml(r.text)}</td></tr>`;
            })
            .join("")
        : `<tr><td colspan="4" class="diff-txt" style="padding:8px 10px;color:var(--fg-muted)">${escapeHtml(emptyLabel)}</td></tr>`;

      return `
        <div class="diff-section ${isCollapsed ? "collapsed" : ""}" data-diff-file="${escapeHtml(key)}">
          <div class="diff-file-bar">
            <div class="diff-file-left">
              <button type="button" class="diff-collapse-btn" data-toggle-file="${escapeHtml(key)}" aria-expanded="${String(!isCollapsed)}">${chevron}</button>
              ${pathBtn}
            </div>
            <div class="diff-file-right">
              <span class="delta-add">+${sec.added || 0}</span>
              <span class="delta-del">-${sec.removed || 0}</span>
            </div>
          </div>
          <div class="diff-table-wrap ${isCollapsed ? "hidden" : ""}">
            <table class="diff-code"><tbody>${rowsHtml}</tbody></table>
          </div>
        </div>`;
    })
    .join("");
}

export function createToastController(el) {
  let timer = null;
  return function showToast(msg, durationMs = 2600) {
    if (!el || !msg) return;
    el.textContent = msg;
    el.classList.remove("hidden");
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => {
      el.classList.add("hidden");
    }, durationMs);
  };
}
