(() => {
  const root = (window.HyperDeck = window.HyperDeck || {});

  const $ = (id) => document.getElementById(id);

  const t = (key, opts) =>
    window.i18next && window.i18next.isInitialized ? window.i18next.t(key, opts) : key;

  function apiFetch(url, options = {}) {
    if (window.sidecar && typeof window.sidecar.fetch === 'function') {
      return window.sidecar.fetch(url, options);
    }
    return fetch(url, options);
  }

  function isDark() {
    return document.documentElement.dataset.theme === 'dark';
  }

  function palette() {
    const dark = isDark();
    return {
      fg: dark ? '#F1F5F9' : '#0F172A',
      muted: dark ? '#8B9BB4' : '#64748B',
      grid: dark ? 'rgba(148,163,184,0.12)' : 'rgba(15,23,42,0.08)',
      tooltipBg: dark ? '#111621' : '#FFFFFF',
      tooltipBorder: dark ? 'rgba(148,163,184,0.22)' : 'rgba(15,23,42,0.12)',
      accent: dark ? '#38BDF8' : '#0284C7',
      indigo: dark ? '#818CF8' : '#6366F1',
      ok: dark ? '#34D399' : '#10B981',
      warn: dark ? '#FBBF24' : '#D97706',
      err: dark ? '#FB7185' : '#E11D48',
    };
  }

  function initThemeSync(onThemeChanged) {
    if (window.__AGY_HYPER_STANDALONE__) {
      const btn = $('btn-theme');
      btn.classList.remove('hidden');
      const saved = localStorage.getItem('agy-hyper-theme');
      const prefersDark = window.matchMedia?.('(prefers-color-scheme: dark)').matches;
      document.documentElement.dataset.theme =
        new URLSearchParams(location.search).get('theme') || saved || (prefersDark ? 'dark' : 'light');
      btn.addEventListener('click', () => {
        const next = isDark() ? 'light' : 'dark';
        document.documentElement.dataset.theme = next;
        localStorage.setItem('agy-hyper-theme', next);
        onThemeChanged();
      });
      return;
    }

    const syncFromHost = () => {
      const probe = document.createElement('span');
      probe.style.cssText = 'position:absolute;visibility:hidden;color:var(--background, #f7f8fa)';
      document.documentElement.appendChild(probe);
      const c = getComputedStyle(probe).color;
      probe.remove();
      const nums = (c.match(/[\d.]+/g) || []).map(Number);
      if (nums.length < 3) return;
      const scale = c.startsWith('color(') ? 1 : 255;
      const [r, g, b] = nums.slice(0, 3).map((v) => v / scale);
      const next = 0.2126 * r + 0.7152 * g + 0.0722 * b < 0.5 ? 'dark' : 'light';
      if (document.documentElement.dataset.theme !== next) {
        document.documentElement.dataset.theme = next;
        onThemeChanged();
      }
    };

    syncFromHost();
    new MutationObserver(syncFromHost).observe(document.documentElement, {
      attributes: true,
      attributeFilter: ['style', 'data-theme'],
    });
    window.addEventListener('message', (ev) => {
      if (ev.data?.type === 'theme-change') requestAnimationFrame(syncFromHost);
    });
  }

  let toastTimer = null;
  function showToast(msg) {
    const el = $('toast');
    el.textContent = msg;
    el.classList.remove('hidden');
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => el.classList.add('hidden'), 2400);
  }

  function fmtSec(ms) {
    if (!ms || ms <= 0) return '0.0s';
    if (ms >= 60000) return `${(ms / 60000).toFixed(1)}m`;
    return `${(ms / 1000).toFixed(1)}s`;
  }

  const CURRENCY_SYMBOLS = {
    USD: '$',
    CNY: '¥',
    JPY: '¥',
    EUR: '€',
    GBP: '£',
    HKD: 'HK$',
    SGD: 'S$',
    KRW: '₩',
  };

  function fmtTokens(n) {
    const v = Number(n || 0);
    if (v >= 1_000_000_000) return `${(v / 1_000_000_000).toFixed(2)}B`;
    if (v >= 1_000_000) return `${(v / 1_000_000).toFixed(2)}M`;
    if (v >= 1_000) return `${(v / 1_000).toFixed(1)}k`;
    return String(Math.round(v));
  }

  function fmtCurrency(amount, currency = 'USD') {
    const code = String(currency || 'USD').toUpperCase();
    const sym = CURRENCY_SYMBOLS[code] || `${code} `;
    const val = Number(amount || 0);
    if (!Number.isFinite(val) || val <= 0) {
      return code === 'JPY' || code === 'KRW' ? `${sym}0` : `${sym}0.00`;
    }
    const zeroDec = code === 'JPY' || code === 'KRW';
    const digits = zeroDec ? (val < 10 ? 1 : 0) : val < 0.01 ? 3 : 2;
    const formatted = val.toLocaleString('en-US', {
      minimumFractionDigits: digits,
      maximumFractionDigits: digits,
    });
    return `${sym}${formatted}`;
  }

  function fmtRelTime(tsMs) {
    if (!tsMs) return '';
    const diffSec = Math.max(0, Math.floor((Date.now() - tsMs) / 1000));
    try {
      const rtf = new Intl.RelativeTimeFormat(window.i18next?.language || 'en', {
        numeric: 'auto',
        style: 'narrow',
      });
      if (diffSec < 60) return rtf.format(-diffSec, 'second');
      if (diffSec < 3600) return rtf.format(-Math.floor(diffSec / 60), 'minute');
      return rtf.format(-Math.floor(diffSec / 3600), 'hour');
    } catch {
      if (diffSec < 60) return `${diffSec}s`;
      if (diffSec < 3600) return `${Math.floor(diffSec / 60)}m`;
      return `${Math.floor(diffSec / 3600)}h`;
    }
  }

  function escapeHtml(s) {
    return String(s ?? '')
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  function renderBreakdown(container, rows) {
    if (!container) return;
    container.innerHTML = rows
      .map(
        (r) => `
        <div class="bd-row">
          <span class="bd-key"><i class="bd-swatch" style="background:${r.color}"></i><span>${escapeHtml(r.label)}</span></span>
          <span class="bd-val">${escapeHtml(r.value)}</span>
        </div>`
      )
      .join('');
  }

  async function copyTextReliable(text) {
    try {
      const r = await apiFetch('/api/clipboard', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ text }),
      }).then((x) => x.json());
      if (r?.ok) return true;
    } catch {
      // fall through
    }
    try {
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(text);
        return true;
      }
    } catch {
      // fall through
    }
    try {
      const ta = document.createElement('textarea');
      ta.value = text;
      ta.setAttribute('readonly', '');
      ta.style.cssText = 'position:fixed;top:-9999px;left:-9999px;opacity:0;';
      document.body.appendChild(ta);
      ta.focus();
      ta.select();
      const ok = document.execCommand('copy');
      ta.remove();
      if (ok) return true;
    } catch {
      // ignore
    }
    return false;
  }

  root.api = {
    $,
    t,
    apiFetch,
    isDark,
    palette,
    initThemeSync,
    showToast,
    fmtSec,
    fmtTokens,
    fmtCurrency,
    fmtRelTime,
    escapeHtml,
    renderBreakdown,
    copyTextReliable,
  };
})();
