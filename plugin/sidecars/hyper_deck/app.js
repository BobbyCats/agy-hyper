(() => {
  const { $, apiFetch, copyTextReliable, initThemeSync, isDark, showToast, t } = window.HyperDeck.api;
  const { renderFullContextChart, renderHeroChart, renderUsageChart, renderWaterfallChart, resizeAllCharts } = window.HyperDeck.charts;
  const { renderAuditPanel, renderSnapshot, renderUsageReport, renderWarmthMeter } = window.HyperDeck.render;

  const SUPPORTED_LANGS = ['en', 'zh-CN', 'ja'];
  const state = {
    lastSnap: null,
    lastUsage: null,
    lastAudit: null,
    openDiffs: {},
    mode: 'simple',
    heroTab: 'context',
    proTab: 'waterfall',
    selectedConvId: '',
    usageWindow: localStorage.getItem('agy-hyper-uwin') || '7d',
    usageMetric: localStorage.getItem('agy-hyper-umetric') || 'cost',
    sharePreviewOpen: false,
    updateBannerOpen: false,
    updateApplying: false,
    forceShowUpdate: false,
  };
  let pollInFlight = false;
  let pollSeq = 0;
  let usageInFlight = false;
  let usageSeq = 0;
  let lastUsageFetchMs = 0;
  let auditInFlight = false;
  let lastAuditFetchMs = 0;
  let sharePreviewSeq = 0;
  let sharePreviewObjectUrl = '';

  function selectSession(convId) {
    state.selectedConvId = convId || '';
    poll(true);
  }

  async function toggleTxDiff(txId) {
    if (!txId) return;
    if (state.openDiffs[txId]) {
      delete state.openDiffs[txId];
      render();
      return;
    }
    state.openDiffs[txId] = { loading: true };
    render();
    try {
      const r = await apiFetch(`/api/tx/diff?txId=${encodeURIComponent(txId)}`, { method: 'GET' }).then((x) => x.json());
      if (state.openDiffs[txId]) {
        state.openDiffs[txId] = r?.ok ? { loading: false, data: r.diff } : { loading: false, error: r?.error || 'diff unavailable' };
        render();
      }
    } catch (e) {
      if (state.openDiffs[txId]) {
        state.openDiffs[txId] = { loading: false, error: String(e?.message || e) };
        render();
      }
    }
  }

  async function pollAudit(force = false) {
    if (state.mode !== 'pro' || state.proTab !== 'usage') return;
    if (auditInFlight && !force) return;
    if (!force && state.lastAudit && Date.now() - lastAuditFetchMs < 30000) {
      renderAuditPanel(state.lastAudit);
      return;
    }
    auditInFlight = true;
    try {
      const days = state.usageWindow === '24h' ? 1 : state.usageWindow === '7d' ? 7 : state.usageWindow === '30d' ? 30 : 90;
      const tz = -new Date().getTimezoneOffset();
      const rep = await apiFetch(`/api/audit?days=${days}&tzOffsetMins=${tz}`, { method: 'GET' }).then((r) => r.json());
      if (rep && !rep.error) {
        state.lastAudit = rep;
        lastAuditFetchMs = Date.now();
        renderAuditPanel(rep);
      }
    } catch {
      // keep previous audit intact
    } finally {
      auditInFlight = false;
    }
  }

  async function fetchAuditSvgPayload() {
    const days = state.usageWindow === '24h' ? 1 : state.usageWindow === '7d' ? 7 : state.usageWindow === '30d' ? 30 : 90;
    const tz = -new Date().getTimezoneOffset();
    const theme = isDark() ? 'dark' : 'light';
    const res = await apiFetch(`/api/audit/svg?days=${days}&theme=${theme}&tzOffsetMins=${tz}`, { method: 'GET' });
    const svgText = await res.text();
    if (!svgText.trim().startsWith('<svg')) {
      throw new Error('invalid SVG response');
    }
    return { days, theme, svgText };
  }

  function triggerBlobDownload(blob, filename) {
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = filename;
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 1500);
  }

  function base64ToBlob(b64, mime) {
    const bin = atob(b64);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    return new Blob([bytes], { type: mime });
  }

  async function renderSvgToPngBlob(svgText, cardW, cardH, outW, outH, theme) {
    const svgBlob = new Blob([svgText], { type: 'image/svg+xml;charset=utf-8' });
    const svgUrl = URL.createObjectURL(svgBlob);
    try {
      const img = await new Promise((resolve, reject) => {
        const el = new Image();
        el.onload = () => resolve(el);
        el.onerror = () => reject(new Error('failed to decode SVG for PNG export'));
        el.src = svgUrl;
      });
      const canvas = document.createElement('canvas');
      canvas.width = outW;
      canvas.height = outH;
      const ctx = canvas.getContext('2d');
      if (!ctx) throw new Error('canvas 2d context unavailable');
      ctx.fillStyle = theme === 'dark' ? '#060911' : '#E2E8F0';
      ctx.fillRect(0, 0, canvas.width, canvas.height);
      const scale = Math.min(outW / cardW, outH / cardH);
      const drawW = Math.round(cardW * scale);
      const drawH = Math.round(cardH * scale);
      const dx = Math.round((outW - drawW) / 2);
      const dy = Math.round((outH - drawH) / 2);
      ctx.drawImage(img, dx, dy, drawW, drawH);
      return await new Promise((resolve, reject) => {
        canvas.toBlob((b) => (b ? resolve(b) : reject(new Error('PNG encoding failed'))), 'image/png');
      });
    } finally {
      URL.revokeObjectURL(svgUrl);
    }
  }

  async function fetchShareCardPayload(includePng = false) {
    const days = state.usageWindow === '24h' ? 1 : state.usageWindow === '7d' ? 7 : state.usageWindow === '30d' ? 30 : 90;
    const tz = -new Date().getTimezoneOffset();
    const theme = isDark() ? 'dark' : 'light';
    const url = `/api/audit/share-card?days=${days}&theme=${theme}&tzOffsetMins=${tz}&includePng=${includePng ? '1' : '0'}`;
    const data = await apiFetch(url, { method: 'GET' }).then((r) => r.json());
    if (!data?.ok || !data?.svg || !String(data.svg).trim().startsWith('<svg')) {
      throw new Error(data?.error || 'share card unavailable');
    }
    return { days, theme, data };
  }

  function syncSharePreviewButton() {
    const btn = $('btn-preview-share-card');
    if (btn) {
      btn.textContent = state.sharePreviewOpen ? t('usage.share_preview_hide') : t('usage.share_preview');
    }
  }

  async function refreshSharePreview() {
    if (!state.sharePreviewOpen) return;
    const box = $('share-card-preview-box');
    const loading = $('share-card-preview-loading');
    const img = $('share-card-preview-img');
    if (!box || !loading || !img) return;
    const seq = ++sharePreviewSeq;
    box.classList.remove('hidden');
    loading.classList.remove('hidden');
    img.classList.add('hidden');
    try {
      const { data } = await fetchShareCardPayload(false);
      if (seq !== sharePreviewSeq || !state.sharePreviewOpen) return;
      if (sharePreviewObjectUrl) {
        URL.revokeObjectURL(sharePreviewObjectUrl);
        sharePreviewObjectUrl = '';
      }
      const blob = new Blob([data.svg], { type: 'image/svg+xml;charset=utf-8' });
      sharePreviewObjectUrl = URL.createObjectURL(blob);
      img.src = sharePreviewObjectUrl;
      loading.classList.add('hidden');
      img.classList.remove('hidden');
    } catch (e) {
      if (seq !== sharePreviewSeq) return;
      loading.classList.add('hidden');
      box.classList.add('hidden');
      state.sharePreviewOpen = false;
      syncSharePreviewButton();
      showToast(t('usage.toast_audit_err', { err: String(e?.message || e) }));
    }
  }

  function toggleSharePreview() {
    const box = $('share-card-preview-box');
    state.sharePreviewOpen = !state.sharePreviewOpen;
    syncSharePreviewButton();
    if (!state.sharePreviewOpen) {
      sharePreviewSeq++;
      if (box) box.classList.add('hidden');
      if (sharePreviewObjectUrl) {
        URL.revokeObjectURL(sharePreviewObjectUrl);
        sharePreviewObjectUrl = '';
      }
      return;
    }
    refreshSharePreview();
  }

  async function exportShareCardPng() {
    const btn = $('btn-export-share-png');
    if (!btn || btn.disabled) return;
    btn.disabled = true;
    const prev = btn.textContent;
    btn.textContent = t('usage.exporting_card');
    try {
      const { days, theme, data } = await fetchShareCardPayload(true);
      const pngBlob = data.png_base64
        ? base64ToBlob(data.png_base64, 'image/png')
        : await renderSvgToPngBlob(
            data.svg,
            data.svg_width || 920,
            data.svg_height || 500,
            data.png_width || 1920,
            data.png_height || 1080,
            theme
          );
      triggerBlobDownload(pngBlob, `agy-hyper-share-card-${days}d-${theme}.png`);
      showToast(t('usage.toast_share_png_ok'));
    } catch (e) {
      showToast(t('usage.toast_audit_err', { err: String(e?.message || e) }));
    } finally {
      btn.disabled = false;
      btn.textContent = prev;
    }
  }

  async function exportShareCardSvg() {
    const btn = $('btn-export-share-svg');
    if (!btn || btn.disabled) return;
    btn.disabled = true;
    const prev = btn.textContent;
    btn.textContent = t('usage.exporting_card');
    try {
      const { days, theme, data } = await fetchShareCardPayload(false);
      const blob = new Blob([data.svg], { type: 'image/svg+xml;charset=utf-8' });
      triggerBlobDownload(blob, `agy-hyper-share-card-${days}d-${theme}.svg`);
      showToast(t('usage.toast_share_svg_ok'));
    } catch (e) {
      showToast(t('usage.toast_audit_err', { err: String(e?.message || e) }));
    } finally {
      btn.disabled = false;
      btn.textContent = prev;
    }
  }

  async function exportAuditSvg() {
    const btn = $('btn-export-audit');
    if (!btn || btn.disabled) return;
    btn.disabled = true;
    const prev = btn.textContent;
    btn.textContent = t('usage.exporting_card');
    try {
      const { days, theme, svgText } = await fetchAuditSvgPayload();
      const blob = new Blob([svgText], { type: 'image/svg+xml;charset=utf-8' });
      triggerBlobDownload(blob, `agy-hyper-audit-${days}d-${theme}.svg`);
      showToast(t('usage.toast_audit_ok'));
    } catch (e) {
      showToast(t('usage.toast_audit_err', { err: String(e?.message || e) }));
    } finally {
      btn.disabled = false;
      btn.textContent = prev;
    }
  }

  async function exportAuditPng() {
    const btn = $('btn-export-audit-png');
    if (!btn || btn.disabled) return;
    btn.disabled = true;
    const prev = btn.textContent;
    btn.textContent = t('usage.exporting_card');
    try {
      const { days, theme, svgText } = await fetchAuditSvgPayload();
      const svgBlob = new Blob([svgText], { type: 'image/svg+xml;charset=utf-8' });
      const svgUrl = URL.createObjectURL(svgBlob);
      try {
        const img = await new Promise((resolve, reject) => {
          const el = new Image();
          el.onload = () => resolve(el);
          el.onerror = () => reject(new Error('failed to decode SVG for PNG export'));
          el.src = svgUrl;
        });
        const scale = 2;
        const pad = 20;
        const cardW = 920;
        const cardH = 560;
        const canvas = document.createElement('canvas');
        canvas.width = (cardW + pad * 2) * scale;
        canvas.height = (cardH + pad * 2) * scale;
        const ctx = canvas.getContext('2d');
        if (!ctx) throw new Error('canvas 2d context unavailable');
        ctx.fillStyle = theme === 'dark' ? '#060911' : '#E2E8F0';
        ctx.fillRect(0, 0, canvas.width, canvas.height);
        ctx.drawImage(img, pad * scale, pad * scale, cardW * scale, cardH * scale);
        const pngBlob = await new Promise((resolve, reject) => {
          canvas.toBlob((b) => (b ? resolve(b) : reject(new Error('PNG encoding failed'))), 'image/png');
        });
        triggerBlobDownload(pngBlob, `agy-hyper-audit-${days}d-${theme}.png`);
        showToast(t('usage.toast_audit_png_ok'));
      } finally {
        URL.revokeObjectURL(svgUrl);
      }
    } catch (e) {
      showToast(t('usage.toast_audit_err', { err: String(e?.message || e) }));
    } finally {
      btn.disabled = false;
      btn.textContent = prev;
    }
  }

  function render() {
    if (state.lastSnap) renderSnapshot(state.lastSnap, state, triggerRollback, selectSession, toggleTxDiff);
    if (state.lastUsage && state.mode === 'pro' && state.proTab === 'usage') {
      renderUsageReport(state.lastUsage, state);
    }
    if (state.lastAudit && state.mode === 'pro' && state.proTab === 'usage') {
      renderAuditPanel(state.lastAudit);
    }
  }

  function resizeVisibleCharts() {
    requestAnimationFrame(() => {
      resizeAllCharts();
      try {
        if (state.mode === 'simple' && state.lastSnap) {
          renderHeroChart(state.lastSnap, state.heroTab);
        } else if (state.mode === 'pro' && state.proTab === 'waterfall' && state.lastSnap) {
          renderWaterfallChart(state.lastSnap.benchmark);
        } else if (state.mode === 'pro' && state.proTab === 'context' && state.lastSnap) {
          renderFullContextChart(state.lastSnap);
        } else if (state.mode === 'pro' && state.proTab === 'usage' && state.lastUsage) {
          renderUsageChart(state.lastUsage, state.usageMetric);
        }
      } catch {
        // keep UI interactive
      }
    });
  }

  function detectInitialLang() {
    const qsLang = new URLSearchParams(location.search).get('lang');
    if (qsLang && SUPPORTED_LANGS.includes(qsLang)) return qsLang;
    const saved = localStorage.getItem('agy-hyper-lang');
    if (saved && SUPPORTED_LANGS.includes(saved)) return saved;
    const nav = (navigator.language || 'en').toLowerCase();
    if (nav.startsWith('zh')) return 'zh-CN';
    if (nav.startsWith('ja')) return 'ja';
    return 'en';
  }

  function applyStaticTranslations() {
    if (window.i18next?.language) document.documentElement.lang = window.i18next.language;
    document.querySelectorAll('[data-i18n]').forEach((el) => {
      el.textContent = t(el.dataset.i18n);
    });
  }

  function syncLangButtons() {
    const cur = window.i18next?.language || 'en';
    document.querySelectorAll('.lang-btn[data-lang]').forEach((b) => {
      b.classList.toggle('active', b.dataset.lang === cur);
    });
  }

  async function initI18n() {
    const resources = {};
    if (window.__HYPER_LOCALES__ && typeof window.__HYPER_LOCALES__ === 'object') {
      for (const lng of SUPPORTED_LANGS) {
        resources[lng] = { translation: window.__HYPER_LOCALES__[lng] || {} };
      }
    } else {
      await Promise.all(
        SUPPORTED_LANGS.map(async (lng) => {
          try {
            const data = await apiFetch(`/locales/${lng}.json`, { method: 'GET' }).then((r) => r.json());
            resources[lng] = { translation: data };
          } catch {
            resources[lng] = { translation: {} };
          }
        })
      );
    }
    if (window.i18next) {
      await window.i18next.init({
        lng: detectInitialLang(),
        fallbackLng: 'en',
        resources,
        interpolation: { escapeValue: false },
      });
    }
    applyStaticTranslations();
    syncLangButtons();
  }

  function setMode(next) {
    state.mode = next;
    $('btn-mode-simple').classList.toggle('active', next === 'simple');
    $('btn-mode-simple').setAttribute('aria-selected', String(next === 'simple'));
    $('btn-mode-pro').classList.toggle('active', next === 'pro');
    $('btn-mode-pro').setAttribute('aria-selected', String(next === 'pro'));
    $('view-simple').classList.toggle('hidden', next !== 'simple');
    $('view-pro').classList.toggle('hidden', next !== 'pro');
    if (next === 'pro' && state.proTab === 'usage') {
      pollUsage(false);
      pollAudit(false);
    }
    resizeVisibleCharts();
  }

  function setHeroTab(tab) {
    state.heroTab = tab;
    document.querySelectorAll('.kpi-tab[data-herotab]').forEach((btn) => {
      const active = btn.dataset.herotab === tab;
      btn.classList.toggle('active', active);
      btn.setAttribute('aria-selected', String(active));
    });
    if (state.lastSnap) {
      try { renderHeroChart(state.lastSnap, tab); } catch { /* ignore */ }
    }
  }

  function setProTab(tab) {
    state.proTab = tab;
    document.querySelectorAll('.pro-tab').forEach((btn) => {
      btn.classList.toggle('active', btn.dataset.protab === tab);
    });
    ['waterfall', 'context', 'usage', 'engine', 'security'].forEach((name) => {
      const el = $(`protab-${name}`);
      if (el) el.classList.toggle('hidden', name !== tab);
    });
    if (tab === 'usage' && state.mode === 'pro') {
      pollUsage(false);
      pollAudit(false);
    }
    resizeVisibleCharts();
  }

  async function pollUsage(force = false) {
    if (state.mode !== 'pro' || state.proTab !== 'usage') return;
    if (usageInFlight && !force) return;
    if (!force && state.lastUsage && state.lastUsage.window === state.usageWindow && Date.now() - lastUsageFetchMs < 15000) {
      renderUsageReport(state.lastUsage, state);
      return;
    }
    const seq = ++usageSeq;
    usageInFlight = true;
    try {
      const q = new URLSearchParams();
      q.set('window', state.usageWindow);
      q.set('tzOffsetMins', String(-new Date().getTimezoneOffset()));
      if (force) q.set('forceSync', '1');
      const rep = await apiFetch(`/api/usage?${q.toString()}`, { method: 'GET' }).then((r) => r.json());
      if (seq !== usageSeq) return;
      if (!rep.error) {
        state.lastUsage = rep;
        lastUsageFetchMs = Date.now();
        renderUsageReport(rep, state);
      }
    } catch {
      // keep previous usage view intact
    } finally {
      if (seq === usageSeq) usageInFlight = false;
    }
  }

  async function changeCurrency(code) {
    const r = await apiFetch('/api/currency', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ currency: code }),
    }).then((x) => x.json()).catch(() => ({ ok: false }));
    if (r.ok) {
      showToast(t('usage.toast_currency', { ccy: code }));
      lastUsageFetchMs = 0;
      poll(true);
      if (state.mode === 'pro' && state.proTab === 'usage') {
        pollUsage(true);
      }
    } else {
      showToast(t('usage.toast_currency_err', { err: r.error || 'error' }));
    }
  }

  async function syncLiveFx() {
    const btn = $('btn-sync-fx');
    if (!btn || btn.disabled) return;
    btn.disabled = true;
    const prev = btn.textContent;
    btn.textContent = t('usage.syncing_fx');
    try {
      const r = await apiFetch('/api/fx/sync', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: '{}',
      }).then((x) => x.json()).catch(() => ({ ok: false, error: 'network error' }));
      if (r.ok) {
        const ccy = state.lastUsage?.currency || state.lastSnap?.config?.display_currency || 'USD';
        const rate = r.rates?.[ccy] ?? 1;
        const detail = ccy === 'USD' ? r.provider : `1 USD = ${Number(rate).toFixed(2)} ${ccy}`;
        showToast(t('usage.toast_fx_ok', { detail }));
        lastUsageFetchMs = 0;
        poll(true);
        pollUsage(true);
      } else {
        showToast(t('usage.toast_fx_err', { err: r.error || 'offline' }));
      }
    } finally {
      btn.disabled = false;
      btn.textContent = prev;
    }
  }

  async function checkUpdateNow() {
    const btn = $('btn-check-update');
    if (!btn || btn.disabled) return;
    btn.disabled = true;
    btn.textContent = t('update.checking');
    try {
      const r = await apiFetch('/api/update/status?force=1', { method: 'GET' })
        .then((x) => x.json())
        .catch((e) => ({ ok: false, error: String(e?.message || e) }));
      if (r?.ok) {
        const { ok: _ok, ...nextUpdateState } = r;
        if (state.lastSnap) state.lastSnap.update_state = nextUpdateState;
        const latest = String(r.latest_version || '').replace(/^v/i, '');
        if (r.update_available && latest) {
          state.forceShowUpdate = true;
          state.updateBannerOpen = true;
          render();
          showToast(t('update.toast_available', { version: latest }));
        } else if (r.last_error) {
          render();
          showToast(t('update.toast_check_err', { err: r.last_error }));
        } else {
          render();
          showToast(t('update.toast_up_to_date', { version: r.current_version || '0.5.1' }));
        }
      } else {
        showToast(t('update.toast_check_err', { err: r?.error || 'error' }));
      }
    } finally {
      btn.disabled = false;
      btn.textContent = t('engine.btn_check_update');
    }
  }

  async function applyReleaseUpdateNow() {
    if (state.updateApplying) return;
    const latest = String(state.lastSnap?.update_state?.latest_version || '').replace(/^v/i, '') || 'latest';
    state.updateApplying = true;
    render();
    showToast(t('update.toast_applying', { version: latest }));
    try {
      const r = await apiFetch('/api/update/apply', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: '{}',
      })
        .then((x) => x.json())
        .catch((e) => ({ ok: false, error: String(e?.message || e) }));
      if (r?.ok) {
        const installed = String(r.result?.installed_version || latest).replace(/^v/i, '');
        state.updateApplying = false;
        state.updateBannerOpen = false;
        state.forceShowUpdate = false;
        if (state.lastSnap?.update_state) {
          state.lastSnap.update_state.update_available = false;
          state.lastSnap.update_state.current_version = installed;
        }
        render();
        showToast(t('update.toast_applied', { version: installed }));
        poll(true);
        if (r.restarting) {
          setTimeout(() => {
            try { location.reload(); } catch { /* ignore */ }
          }, 1200);
        }
      } else {
        state.updateApplying = false;
        render();
        showToast(t('update.toast_apply_err', { err: r?.error || 'error' }));
      }
    } catch (e) {
      state.updateApplying = false;
      render();
      showToast(t('update.toast_apply_err', { err: String(e?.message || e) }));
    }
  }

  async function dismissReleaseUpdateBanner() {
    const latest = String(state.lastSnap?.update_state?.latest_version || '').replace(/^v/i, '');
    state.updateBannerOpen = false;
    state.forceShowUpdate = false;
    if (state.lastSnap?.update_state && latest) {
      state.lastSnap.update_state.dismissed_version = latest;
    }
    render();
    const r = await apiFetch('/api/update/dismiss', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ version: latest }),
    })
      .then((x) => x.json())
      .catch(() => ({ ok: false }));
    if (r?.ok && state.lastSnap) {
      const { ok: _ok, ...nextUpdateState } = r;
      state.lastSnap.update_state = nextUpdateState;
      render();
    }
    if (latest) {
      showToast(t('update.toast_dismissed', { version: latest }));
    }
  }

  async function triggerRollback(txId, force = false) {
    const r = await apiFetch('/api/rollback', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ tx_id: txId, force }),
    }).then((x) => x.json()).catch(() => ({ ok: false }));

    if (r.ok) {
      const res = r.result || {};
      if (res.skipped_or_conflicts?.length) {
        showToast(t('shield.toast_conflict', { msg: res.skipped_or_conflicts[0] }));
      } else {
        const count = (res.restored_files || []).length + (res.removed_created_files || []).length;
        showToast(t('shield.toast_restored', { count, tx: txId }));
      }
      poll(true);
    } else {
      showToast(t('shield.toast_rb_err', { err: r.error || 'error' }));
    }
  }

  async function poll(force = false) {
    if (pollInFlight && !force) return;
    const seq = ++pollSeq;
    pollInFlight = true;
    try {
      const sp = new URLSearchParams(location.search);
      const q = new URLSearchParams();
      if (state.selectedConvId) q.set('conversationId', state.selectedConvId);
      if (sp.get('transcriptPath')) q.set('transcriptPath', sp.get('transcriptPath'));
      const url = q.toString() ? `/api/snapshot?${q.toString()}` : '/api/snapshot';
      const snap = await apiFetch(url, { method: 'GET' }).then((r) => r.json());
      if (seq !== pollSeq) return;
      if (!snap.error) {
        state.lastSnap = snap;
        render();
      }
    } catch {
      if (seq === pollSeq) {
        $('daemon-dot').classList.add('warn');
        $('daemon-label').textContent = t('app.reconnecting');
      }
    } finally {
      if (seq === pollSeq) pollInFlight = false;
    }
  }

  initThemeSync(() => {
    render();
    if (state.sharePreviewOpen) refreshSharePreview();
  });

  document.querySelectorAll('.lang-btn[data-lang]').forEach((btn) => {
    btn.addEventListener('click', async () => {
      if (!window.i18next) return;
      await window.i18next.changeLanguage(btn.dataset.lang);
      localStorage.setItem('agy-hyper-lang', btn.dataset.lang);
      applyStaticTranslations();
      syncLangButtons();
      syncSharePreviewButton();
      render();
    });
  });

  $('btn-mode-simple').addEventListener('click', () => setMode('simple'));
  $('btn-mode-pro').addEventListener('click', () => setMode('pro'));
  document.querySelectorAll('.kpi-tab[data-herotab]').forEach((b) =>
    b.addEventListener('click', () => setHeroTab(b.dataset.herotab))
  );
  document.querySelectorAll('.pro-tab').forEach((b) =>
    b.addEventListener('click', () => setProTab(b.dataset.protab))
  );
  $('session-select').addEventListener('change', (e) => {
    selectSession(e.target.value);
  });
  $('btn-back-parent').addEventListener('click', () => {
    const rootId = state.lastSnap?.session_tree?.root_conversation_id || '';
    selectSession(rootId);
  });

  $('btn-open-usage')?.addEventListener('click', () => {
    setProTab('usage');
    setMode('pro');
  });

  $('simple-currency-select')?.addEventListener('change', (e) => {
    changeCurrency(e.target.value);
  });

  $('usage-currency-select')?.addEventListener('change', (e) => {
    changeCurrency(e.target.value);
  });

  $('btn-sync-fx')?.addEventListener('click', () => {
    syncLiveFx();
  });

  $('btn-export-audit-png')?.addEventListener('click', () => {
    exportAuditPng();
  });

  $('btn-export-audit')?.addEventListener('click', () => {
    exportAuditSvg();
  });

  $('btn-export-share-png')?.addEventListener('click', () => {
    exportShareCardPng();
  });

  $('btn-export-share-svg')?.addEventListener('click', () => {
    exportShareCardSvg();
  });

  $('btn-preview-share-card')?.addEventListener('click', () => {
    toggleSharePreview();
  });

  $('update-pill')?.addEventListener('click', () => {
    state.updateBannerOpen = !state.updateBannerOpen;
    render();
  });

  $('btn-update-now')?.addEventListener('click', () => {
    applyReleaseUpdateNow();
  });

  $('btn-update-notes')?.addEventListener('click', () => {
    const url =
      $('btn-update-notes')?.getAttribute('href') ||
      'https://github.com/BobbyCats/agy-hyper/releases/latest';
    copyTextReliable(url).then((ok) => {
      if (ok && !window.__AGY_HYPER_STANDALONE__) {
        showToast(t('update.toast_notes_copied'));
      }
    });
  });

  $('btn-update-dismiss')?.addEventListener('click', () => {
    dismissReleaseUpdateBanner();
  });

  $('btn-check-update')?.addEventListener('click', () => {
    checkUpdateNow();
  });

  document.querySelectorAll('.win-btn[data-uwin]').forEach((btn) => {
    btn.addEventListener('click', () => {
      state.usageWindow = btn.dataset.uwin;
      localStorage.setItem('agy-hyper-uwin', state.usageWindow);
      lastUsageFetchMs = 0;
      lastAuditFetchMs = 0;
      pollUsage(false);
      pollAudit(true);
      if (state.sharePreviewOpen) refreshSharePreview();
    });
  });

  document.querySelectorAll('.metric-btn[data-umetric]').forEach((btn) => {
    btn.addEventListener('click', () => {
      state.usageMetric = btn.dataset.umetric;
      localStorage.setItem('agy-hyper-umetric', state.usageMetric);
      if (state.lastUsage) renderUsageReport(state.lastUsage, state);
    });
  });

  document.querySelectorAll('.seg-btn[data-profile]').forEach((btn) => {
    btn.addEventListener('click', async () => {
      const profile = btn.dataset.profile;
      const r = await apiFetch('/api/profile', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ profile }),
      }).then((x) => x.json()).catch(() => ({ ok: false }));
      showToast(r.ok ? t('shield.toast_profile', { profile: profile.toUpperCase() }) : t('shield.toast_profile_err', { err: r.error || 'error' }));
      if (r.ok) poll(true);
    });
  });

  document.querySelectorAll('input[data-toggle]').forEach((inp) => {
    inp.addEventListener('change', async () => {
      const key = inp.dataset.toggle;
      const value = inp.checked;
      const r = await apiFetch('/api/toggle', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ key, value }),
      }).then((x) => x.json()).catch(() => ({ ok: false }));
      if (r.ok) {
        if (key === 'enable_update_check' && !value) {
          state.forceShowUpdate = false;
          state.updateBannerOpen = false;
        }
        showToast(t('engine.toast_toggle_ok', { key, state: value ? 'ON' : 'OFF' }));
        poll(true);
      } else {
        inp.checked = !value;
        showToast(t('engine.toast_toggle_err', { err: r.error || 'error' }));
      }
    });
  });

  $('btn-copy-compact').addEventListener('click', async () => {
    const btn = $('btn-copy-compact');
    const isPivot = btn.dataset.action === 'pivot';
    const errPart = btn.dataset.err ? ` (${btn.dataset.err})` : '';
    const textToCopy = isPivot
      ? t('pulse.pivot_prompt', { count: Number(btn.dataset.streak || 2), err: errPart })
      : '/compact';
    const ok = await copyTextReliable(textToCopy);
    if (ok) {
      showToast(isPivot ? t('pulse.copied_pivot') : t('pulse.copied_compact'));
      btn.dataset.copiedLock = '1';
      btn.textContent = t('pulse.copied_short');
      setTimeout(() => {
        delete btn.dataset.copiedLock;
        btn.textContent = btn.dataset.action === 'pivot' ? t('pulse.copy_pivot') : t('pulse.copy_compact');
      }, 1600);
    } else {
      showToast(t('pulse.copy_fallback'));
    }
  });

  const qs = new URLSearchParams(location.search);
  if (qs.get('conversationId')) state.selectedConvId = qs.get('conversationId');
  if (qs.get('mode') === 'pro') setMode('pro');
  if (['context', 'latency', 'cache', 'saved'].includes(qs.get('hero'))) setHeroTab(qs.get('hero'));
  if (['waterfall', 'context', 'usage', 'engine', 'security'].includes(qs.get('tab'))) setProTab(qs.get('tab'));

  initI18n()
    .catch(() => {})
    .finally(() => {
      poll(true);
      if (state.mode === 'pro' && state.proTab === 'usage') {
        pollUsage(false);
        pollAudit(false);
      }
      setInterval(() => poll(false), 2500);
      setInterval(() => {
        if (state.mode === 'pro' && state.proTab === 'usage') pollUsage(false);
      }, 20000);
      setInterval(() => { if (state.lastSnap) renderWarmthMeter(state.lastSnap); }, 1000);
    });
})();
