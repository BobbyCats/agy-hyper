(() => {
  const root = (window.HyperDeck = window.HyperDeck || {});
  const { $, escapeHtml, fmtRelTime, fmtSec, t } = root.api;
  const { renderFullContextChart, renderHeroChart, renderWaterfallChart } = root.charts;

  function renderWarmthMeter(snap) {
    if (!snap) return;
    const p = snap.pulse;
    const warmWin = snap.config?.cache_warm_window_secs || 60;
    let remaining = warmWin;
    if (p?.last_planner_epoch_ms) {
      const elapsed = Math.max(0, Math.floor((Date.now() - p.last_planner_epoch_ms) / 1000));
      remaining = Math.max(0, warmWin - elapsed);
    } else if (p) {
      remaining = p.cache_warm_remaining_secs ?? 0;
    }

    const pct = Math.max(0, Math.min(100, Math.round((remaining / warmWin) * 100)));
    const state = remaining === 0 ? 'err' : remaining <= 15 ? 'warn' : 'ok';
    const stateLabel = remaining === 0 ? t('pulse.cold') : remaining <= 15 ? t('pulse.cooling') : t('pulse.warm');
    const hintTxt = remaining === 0 ? t('pulse.hint_cold') : remaining <= 15 ? t('pulse.hint_cooling') : t('pulse.hint_warm');

    $('warmth-clock').textContent = remaining === 0 ? `0s / ${warmWin}s` : `${remaining}s / ${warmWin}s`;
    const pill = $('warmth-state-pill');
    pill.className = `status-pill ${state}`;
    pill.textContent = stateLabel;

    const fill = $('warmth-fill');
    fill.className = `meter-fill ${state}`;
    fill.style.width = `${remaining === 0 ? 4 : pct}%`;
    $('warmth-meter').setAttribute('aria-valuenow', String(remaining));
    $('cache-hint').textContent = hintTxt;
  }

  function renderToolsBars(eng) {
    const container = $('tools-bars');
    const entries = Object.entries(eng?.tool_counts || {})
      .filter(([, v]) => v > 0)
      .sort((a, b) => b[1] - a[1]);

    if (!entries.length) {
      container.innerHTML = `<div class="empty-state">${escapeHtml(t('engine.tools_empty'))}</div>`;
      return;
    }

    const total = entries.reduce((acc, [, v]) => acc + v, 0) || 1;
    const maxVal = entries[0][1] || 1;
    container.innerHTML = entries
      .map(([name, count]) => {
        const widthPct = Math.max(4, Math.round((count / maxVal) * 100));
        const sharePct = Math.round((count / total) * 100);
        return `
          <div class="prop-row">
            <div class="prop-fill" style="width:${widthPct}%"></div>
            <span class="prop-label">${escapeHtml(name)}</span>
            <span class="prop-count">${count} <span style="opacity:.6">(${sharePct}%)</span></span>
          </div>`;
      })
      .join('');
  }

  function txFileDisplay(entry) {
    if (typeof entry === 'string') return entry;
    return entry?.rel_path || entry?.abs_path || '';
  }

  function renderTxList(container, txs, showFull, onRollback) {
    if (!txs?.length) {
      container.innerHTML = `<div class="empty-state">${escapeHtml(t('shield.empty_tx'))}</div>`;
      return;
    }
    container.innerHTML = txs
      .map((tx) => {
        const rawPaths = (tx.files || []).map(txFileDisplay).filter(Boolean);
        const files = rawPaths.map((p) => p.split(/[\\/]/).pop()).join(', ');
        const btn = tx.rolled_back
          ? `<span class="mono-tag">${escapeHtml(t('shield.undone'))}</span>`
          : `<button type="button" class="btn-undo" data-undo="${escapeHtml(tx.tx_id)}">${escapeHtml(t('shield.undo_btn'))}</button>`;
        const tsMs = tx.timestamp_ms || tx.created_at_ms || 0;
        const srcTag = tx.source ? ` · ${tx.source}` : '';
        const meta = showFull
          ? `${tx.tx_id} · ${fmtRelTime(tsMs)} · ${rawPaths.length}f${srcTag}`
          : `${fmtRelTime(tsMs)} · ${rawPaths.length}f${srcTag}`;
        return `
          <div class="tx-item">
            <div class="tx-main">
              <span class="tx-files" title="${escapeHtml(rawPaths.join('\n'))}">${escapeHtml(files || tx.tx_id)}</span>
              <span class="tx-meta">${escapeHtml(meta)}</span>
            </div>
            ${btn}
          </div>`;
      })
      .join('');

    container.querySelectorAll('.btn-undo[data-undo]').forEach((b) => {
      b.addEventListener('click', () => onRollback(b.dataset.undo, false));
    });
  }

  function renderSessionSelect(sessions, selectedConvId, activeConvId) {
    const sel = $('session-select');
    const currentVal = selectedConvId || activeConvId || '';
    const opts = [
      `<option value="">${escapeHtml(t('app.session_latest'))}</option>`,
      ...(sessions || []).map((s) => {
        const rawTitle = (s.title || s.preview || s.conversation_id.slice(0, 8)).trim();
        const ws = (s.workspace || '').trim();
        const combined = ws && !rawTitle.toLowerCase().includes(ws.toLowerCase()) ? `${ws} · ${rawTitle}` : rawTitle;
        const shortName = combined.length > 40 ? `${combined.slice(0, 39).trimEnd()}…` : combined;
        const age = s.mtime_ms ? fmtRelTime(s.mtime_ms) : '';
        const label = age ? `${shortName} · ${age}` : shortName;
        const selected = s.conversation_id === currentVal ? ' selected' : '';
        return `<option value="${escapeHtml(s.conversation_id)}"${selected}>${escapeHtml(label)}</option>`;
      }),
    ];
    const nextHtml = opts.join('');
    if (sel.dataset.rendered !== nextHtml) {
      sel.innerHTML = nextHtml;
      sel.dataset.rendered = nextHtml;
    }
  }

  function renderSnapshot(snap, state, onRollback) {
    const cfg = snap.config || {};
    const eng = snap.engine || {};
    const pulse = snap.pulse || null;
    const bench = snap.benchmark || null;

    $('version-pill').textContent = `v${eng.version || snap.version || '0.1.0'}`;
    const isUds = snap.transport === 'uds-daemon';
    $('daemon-dot').classList.toggle('warn', !isUds);
    $('daemon-label').textContent = isUds
      ? t('app.daemon_online', { rtt: snap.rpc_rtt_ms ?? 0 })
      : t('app.daemon_fallback', { rtt: snap.rpc_rtt_ms ?? 0 });

    renderSessionSelect(snap.recent_sessions, state.selectedConvId, pulse?.conversation_id || bench?.conversation_id || '');

    const hChip = $('health-chip');
    hChip.className = 'status-pill';
    if (pulse?.health === 'red') {
      hChip.classList.add('err');
      hChip.textContent = t('pulse.redline');
    } else if (pulse?.health === 'yellow') {
      hChip.classList.add('warn');
      hChip.textContent = t('pulse.warming');
    } else {
      hChip.classList.add('ok');
      hChip.textContent = t('pulse.healthy');
    }

    const totalTok = pulse?.total_tokens || 0;
    $('s-ctx').textContent = totalTok > 0 ? `${(totalTok / 1000).toFixed(1)}k` : '—';
    $('s-p50').textContent = bench?.median_turn_ms ? fmtSec(bench.median_turn_ms) : '—';
    $('s-cache').textContent = pulse
      ? `${Math.round((pulse.cache_hit_ratio || 0) * 100)}%`
      : bench
        ? `${Math.round((bench.cache_hit_ratio || 0) * 100)}%`
        : '—';

    const sessionHyperTurns = bench?.hyper_tool_calls ?? 0;
    const turnsBasis = bench ? sessionHyperTurns : eng.total_turns_saved || 0;
    const llmP50 = bench?.median_llm_ms || 0;
    const estSavedMs = turnsBasis > 0 && llmP50 > 0 ? turnsBasis * llmP50 : 0;
    $('s-saved').textContent = fmtSec(estSavedMs);
    $('s-saved-cell').title = t('kpi.est_saved_hint', { turns: turnsBasis });

    const alertEl = $('cliff-alert');
    if (pulse?.multimodal_cliff_risk) {
      alertEl.textContent = t('pulse.cliff_multimodal', { count: pulse.media_items_recent });
      alertEl.className = 'inline-alert';
    } else if (pulse?.recent_timeouts > 0) {
      alertEl.textContent = t('pulse.cliff_timeout', { count: pulse.recent_timeouts });
      alertEl.className = 'inline-alert err';
    } else if (pulse?.health_reason === 'proxy_reset') {
      alertEl.textContent = t('pulse.cliff_reset', { count: pulse.recent_resets || 2 });
      alertEl.className = 'inline-alert';
    } else {
      alertEl.classList.add('hidden');
    }

    document.querySelectorAll('.seg-btn[data-profile]').forEach((b) => {
      b.classList.toggle('active', b.dataset.profile === (cfg.profile || 'balanced'));
    });
    const secretDot = $('pill-secret').querySelector('i');
    if (secretDot) secretDot.className = cfg.block_secret_files ? 'dot-ok' : 'dot-off';

    const txs = snap.transactions || [];
    $('tx-count-badge').textContent = t('shield.tx_count', { count: txs.length });
    renderTxList($('simple-tx-list'), txs.slice(0, 4), false, onRollback);
    renderTxList($('pro-wal-list'), txs, true, onRollback);

    $('wf-p50').textContent = bench?.median_turn_ms ? fmtSec(bench.median_turn_ms) : '—';
    $('wf-p90').textContent = bench?.p90_turn_ms ? fmtSec(bench.p90_turn_ms) : '—';
    $('wf-tps').textContent = bench?.effective_tps ? bench.effective_tps.toFixed(1) : '—';
    $('wf-llm').textContent = bench?.median_llm_ms ? fmtSec(bench.median_llm_ms) : '—';
    $('ctx-pro-summary').textContent = `${(totalTok / 1000).toFixed(1)}k · ${pulse ? Math.round((pulse.cache_hit_ratio || 0) * 100) : 0}%`;

    $('eng-pid').textContent = `PID ${eng.pid || '—'}`;
    $('eng-rtt').textContent = `${snap.rpc_rtt_ms ?? 0} ms`;
    $('eng-rss').textContent = `${(eng.rss_mb || 0).toFixed(1)} MB`;
    $('eng-uptime').textContent = `${eng.uptime_secs || 0}s`;
    const astTotal = (eng.ast_cache_hits || 0) + (eng.ast_cache_misses || 0);
    const astRate = astTotal > 0 ? Math.round(((eng.ast_cache_hits || 0) / astTotal) * 100) : 0;
    $('eng-ast').textContent = `${eng.ast_cache_entries || 0} (${astRate}%)`;
    const packTotal = (eng.pack_cache_hits || 0) + (eng.pack_cache_misses || 0);
    const packRate = packTotal > 0 ? Math.round(((eng.pack_cache_hits || 0) / packTotal) * 100) : 0;
    $('eng-pack').textContent = `${eng.pack_cache_hits || 0}/${packTotal} (${packRate}%)`;
    $('eng-prewarm').textContent = String(eng.prewarm_cached || 0);

    $('tog-prewarm').checked = Boolean(cfg.prewarm_tier2);
    $('tog-secret').checked = Boolean(cfg.block_secret_files);
    $('tog-argv').checked = Boolean(cfg.allow_custom_argv_checks);
    $('tog-pill').checked = Boolean(cfg.enable_ui_pill);

    renderToolsBars(eng);

    const wsList = eng.active_workspaces || [];
    $('eng-workspaces').innerHTML = wsList.length
      ? wsList.map(([p, rev]) => `<div class="audit-row"><span>${escapeHtml(p)}</span><span>rev #${rev}</span></div>`).join('')
      : `<div class="empty-state">${escapeHtml(t('engine.workspaces_empty'))}</div>`;

    const events = snap.recent_events || [];
    $('audit-blocked-chip').textContent = t('security.blocked_badge', { count: eng.blocked_security_events || 0 });
    $('pro-audit-list').innerHTML = events.length
      ? events
          .map(
            (ev) => `
          <div class="audit-row ${ev.blocked ? 'blocked' : ''}">
            <span>${ev.blocked ? escapeHtml(t('security.blocked_tag')) : escapeHtml(ev.kind)} · ${escapeHtml(ev.detail)}</span>
            <span>${ev.duration_ms}ms</span>
          </div>`
          )
          .join('')
      : `<div class="empty-state">${escapeHtml(t('security.audit_empty'))}</div>`;

    renderWarmthMeter(snap);
    try {
      if (state.mode === 'simple') renderHeroChart(snap, state.heroTab);
      if (state.mode === 'pro' && state.proTab === 'waterfall') renderWaterfallChart(bench);
      if (state.mode === 'pro' && state.proTab === 'context') renderFullContextChart(snap);
    } catch {
      // keep controls responsive even if a chart frame fails
    }
    if (!document.documentElement.classList.contains('ready')) {
      requestAnimationFrame(() => document.documentElement.classList.add('ready'));
    }
  }

  root.render = {
    renderWarmthMeter,
    renderSnapshot,
  };
})();
