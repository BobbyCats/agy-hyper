(() => {
  const { $, apiFetch, copyTextReliable, initThemeSync, showToast, t } = window.HyperDeck.api;
  const { renderFullContextChart, renderHeroChart, renderWaterfallChart, resizeAllCharts } = window.HyperDeck.charts;
  const { renderSnapshot, renderWarmthMeter } = window.HyperDeck.render;

  const SUPPORTED_LANGS = ['en', 'zh-CN', 'ja'];
  const state = {
    lastSnap: null,
    mode: 'simple',
    heroTab: 'context',
    proTab: 'waterfall',
    selectedConvId: '',
  };
  let pollInFlight = false;
  let pollSeq = 0;

  function render() {
    if (state.lastSnap) renderSnapshot(state.lastSnap, state, triggerRollback);
  }

  function resizeVisibleCharts() {
    requestAnimationFrame(() => {
      resizeAllCharts();
      if (!state.lastSnap) return;
      try {
        if (state.mode === 'simple') {
          renderHeroChart(state.lastSnap, state.heroTab);
        } else if (state.proTab === 'waterfall') {
          renderWaterfallChart(state.lastSnap.benchmark);
        } else if (state.proTab === 'context') {
          renderFullContextChart(state.lastSnap);
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
    ['waterfall', 'context', 'engine', 'security'].forEach((name) => {
      $(`protab-${name}`).classList.toggle('hidden', name !== tab);
    });
    resizeVisibleCharts();
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

  initThemeSync(render);

  document.querySelectorAll('.lang-btn[data-lang]').forEach((btn) => {
    btn.addEventListener('click', async () => {
      if (!window.i18next) return;
      await window.i18next.changeLanguage(btn.dataset.lang);
      localStorage.setItem('agy-hyper-lang', btn.dataset.lang);
      applyStaticTranslations();
      syncLangButtons();
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
    state.selectedConvId = e.target.value;
    poll(true);
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
    const ok = await copyTextReliable('/compact');
    if (ok) {
      showToast(t('pulse.copied_compact'));
      const prev = btn.textContent;
      btn.textContent = t('pulse.copied_short');
      setTimeout(() => { btn.textContent = prev; }, 1600);
    } else {
      showToast(t('pulse.copy_fallback'));
    }
  });

  const qs = new URLSearchParams(location.search);
  if (qs.get('conversationId')) state.selectedConvId = qs.get('conversationId');
  if (qs.get('mode') === 'pro') setMode('pro');
  if (['context', 'latency', 'cache', 'saved'].includes(qs.get('hero'))) setHeroTab(qs.get('hero'));
  if (['waterfall', 'context', 'engine', 'security'].includes(qs.get('tab'))) setProTab(qs.get('tab'));

  initI18n()
    .catch(() => {})
    .finally(() => {
      poll(true);
      setInterval(() => poll(false), 2500);
      setInterval(() => { if (state.lastSnap) renderWarmthMeter(state.lastSnap); }, 1000);
    });
})();
