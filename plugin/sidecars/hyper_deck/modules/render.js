(() => {
  const root = (window.HyperDeck = window.HyperDeck || {});
  const { $, escapeHtml, fmtCurrency, fmtRelTime, fmtSec, fmtTokens, t } = root.api;
  const { renderFullContextChart, renderHeroChart, renderUsageChart, renderWaterfallChart } = root.charts;

  function renderWarmthMeter(snap) {
    if (!snap) return;
    const p = snap.pulse;
    const warmWin = snap.config?.cache_warm_window_secs || 240;
    const coolThreshold = Math.max(15, Math.floor(warmWin * 0.5));
    let remaining = warmWin;
    if (p?.last_planner_epoch_ms) {
      const elapsed = Math.max(0, Math.floor((Date.now() - p.last_planner_epoch_ms) / 1000));
      remaining = Math.max(0, warmWin - elapsed);
    } else if (p) {
      remaining = p.cache_warm_remaining_secs ?? 0;
    }

    const pct = Math.max(0, Math.min(100, Math.round((remaining / warmWin) * 100)));
    const state = remaining === 0 ? 'err' : remaining <= coolThreshold ? 'warn' : 'ok';
    const stateLabel = remaining === 0 ? t('pulse.cold') : remaining <= coolThreshold ? t('pulse.cooling') : t('pulse.warm');
    const hintTxt = remaining === 0
      ? t('pulse.hint_cold', { secs: warmWin })
      : remaining <= coolThreshold
        ? t('pulse.hint_cooling')
        : t('pulse.hint_warm');

    $('warmth-clock').textContent = remaining === 0 ? `0s / ${warmWin}s` : `${remaining}s / ${warmWin}s`;
    const pill = $('warmth-state-pill');
    pill.className = `status-pill ${state}`;
    pill.textContent = stateLabel;

    const fill = $('warmth-fill');
    fill.className = `meter-fill ${state}`;
    fill.style.width = `${remaining === 0 ? 4 : pct}%`;
    const meter = $('warmth-meter');
    meter.setAttribute('aria-valuemax', String(warmWin));
    meter.setAttribute('aria-valuenow', String(remaining));
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

  function renderSessionSelect(sessions, tree, selectedConvId, activeConvId) {
    const sel = $('session-select');
    const currentVal = selectedConvId || activeConvId || '';
    const treeSubs = tree?.subagents || [];
    const rootId = tree?.root_conversation_id || '';
    const seen = new Set();
    const opts = [`<option value="">${escapeHtml(t('app.session_latest'))}</option>`];

    const pushSubOption = (sub) => {
      if (seen.has(sub.conversation_id)) return;
      seen.add(sub.conversation_id);
      const role = sub.agent_name || 'sub';
      const rawTitle = (sub.title || sub.conversation_id.slice(0, 8)).trim();
      const shortName = rawTitle.length > 26 ? `${rawTitle.slice(0, 25).trimEnd()}…` : rawTitle;
      const ctx = sub.total_tokens > 0 ? `${(sub.total_tokens / 1000).toFixed(0)}k` : '—';
      const selected = sub.conversation_id === currentVal ? ' selected' : '';
      opts.push(
        `<option value="${escapeHtml(sub.conversation_id)}"${selected}>${escapeHtml(`↳ [${role}] ${shortName} · ${ctx}`)}</option>`
      );
    };

    for (const s of sessions || []) {
      seen.add(s.conversation_id);
      const rawTitle = (s.title || s.preview || s.conversation_id.slice(0, 8)).trim();
      const ws = (s.workspace || '').trim();
      const combined = ws && !rawTitle.toLowerCase().includes(ws.toLowerCase()) ? `${ws} · ${rawTitle}` : rawTitle;
      const shortName = combined.length > 36 ? `${combined.slice(0, 35).trimEnd()}…` : combined;
      const subTag = s.subagent_count > 0 ? ` · ${t('subagents.sub_badge', { count: s.subagent_count })}` : '';
      const age = s.mtime_ms ? fmtRelTime(s.mtime_ms) : '';
      const label = age ? `${shortName}${subTag} · ${age}` : `${shortName}${subTag}`;
      const selected = s.conversation_id === currentVal ? ' selected' : '';
      opts.push(`<option value="${escapeHtml(s.conversation_id)}"${selected}>${escapeHtml(label)}</option>`);

      if (s.conversation_id === rootId && treeSubs.length) {
        for (const sub of treeSubs) pushSubOption(sub);
      }
    }

    if (treeSubs.length) {
      for (const sub of treeSubs) pushSubOption(sub);
    }

    const nextHtml = opts.join('');
    if (sel.dataset.rendered !== nextHtml) {
      sel.innerHTML = nextHtml;
      sel.dataset.rendered = nextHtml;
    }
  }

  function renderSubagentStrip(tree, activeConvId, onSelectSession) {
    const strip = $('subagent-strip');
    if (!strip) return;
    if (!tree || !tree.subagents?.length) {
      strip.classList.add('hidden');
      return;
    }
    strip.classList.remove('hidden');

    $('subagent-heading').textContent = t('subagents.heading', { count: tree.subagent_count });
    const actPill = $('subagent-active-pill');
    if (tree.active_subagents > 0) {
      actPill.textContent = t('subagents.active_badge', { count: tree.active_subagents });
      actPill.classList.remove('hidden');
    } else {
      actPill.classList.add('hidden');
    }

    const liveRedlines = (tree.subagents || []).filter((s) => s.active && s.health === 'red').length;
    const redPill = $('subagent-red-pill');
    if (liveRedlines > 0) {
      redPill.textContent = t('subagents.redline_badge', { count: liveRedlines });
      redPill.classList.remove('hidden');
    } else {
      redPill.classList.add('hidden');
    }

    $('btn-back-parent').classList.toggle('hidden', !tree.selected_is_subagent);
    $('subagent-tree-meta').textContent = t('subagents.tree_meta', {
      turns: tree.combined_turns || 0,
      tokens: fmtTokens(tree.combined_tokens || 0),
      cache: Math.round((tree.combined_cache_hit_ratio || 0) * 100),
    });

    const list = $('subagent-list');
    list.innerHTML = tree.subagents
      .map((sub) => {
        const isSel = sub.conversation_id === activeConvId;
        const role = sub.agent_name || 'sub';
        const title = sub.title || `#${sub.conversation_id.slice(0, 8)}`;
        const ctxFull = sub.total_tokens > 0 ? `${(sub.total_tokens / 1000).toFixed(1)}k` : '—';
        const ctxShort = sub.total_tokens > 0
          ? (sub.total_tokens >= 100000 ? `${Math.round(sub.total_tokens / 1000)}k` : `${(sub.total_tokens / 1000).toFixed(1)}k`)
          : '—';
        const cachePctNum = Math.round((sub.cache_hit_ratio || 0) * 100);
        const cachePct = `${cachePctNum}%`;
        const hyperTxt = sub.hyper_tool_calls > 0 ? ` · ⚡${sub.hyper_tool_calls}` : '';
        const cumTxt = fmtTokens(sub.cumulative_tokens || 0);
        const turns = sub.total_turns || 0;
        const tipKey = sub.active
          ? 'subagents.row_tooltip_active'
          : sub.health === 'red'
            ? 'subagents.row_tooltip_done_peak'
            : 'subagents.row_tooltip_done';
        const rowTip = t(tipKey, {
          ctx: ctxFull,
          cum: cumTxt,
          turns,
          cache: cachePctNum,
        });

        let statsHtml;
        if (sub.active) {
          const hCls = sub.health === 'red' ? 'err' : sub.health === 'yellow' ? 'warn' : 'ok';
          const hLbl = sub.health === 'red' ? 'RED' : sub.health === 'yellow' ? 'WARN' : 'OK';
          statsHtml = `
              <span>${escapeHtml(ctxFull)}</span>
              <span>${escapeHtml(cachePct)}${escapeHtml(hyperTxt)}</span>
              <span class="status-pill ${hCls}">${hLbl}</span>`;
        } else {
          const pillCls = sub.health === 'red' ? 'muted-peak' : 'muted';
          const doneLbl = ctxShort !== '—'
            ? t('subagents.done_ctx', { ctx: ctxShort })
            : t('subagents.done_pill');
          statsHtml = `
              <span>${escapeHtml(cachePct)}${escapeHtml(hyperTxt)}</span>
              <span class="status-pill ${pillCls}">${escapeHtml(doneLbl)}</span>`;
        }

        return `
          <button type="button" class="subagent-row${sub.active ? '' : ' done'}${isSel ? ' selected' : ''}" data-subconv="${escapeHtml(sub.conversation_id)}" title="${escapeHtml(rowTip)}">
            <span class="subagent-row-main">
              <i class="subagent-dot${sub.active ? ' live' : ''}"></i>
              <span class="subagent-role">${escapeHtml(role)}</span>
              <span class="subagent-title">${escapeHtml(title)}</span>
            </span>
            <span class="subagent-stats">${statsHtml}
            </span>
          </button>`;
      })
      .join('');

    list.querySelectorAll('.subagent-row[data-subconv]').forEach((btn) => {
      btn.addEventListener('click', () => {
        if (typeof onSelectSession === 'function') onSelectSession(btn.dataset.subconv);
      });
    });
  }

  function renderSessionUsageRibbon(snap) {
    const ribbon = $('session-usage-ribbon');
    if (!ribbon) return;
    const u = snap.session_usage;
    const ccySel = $('simple-currency-select');
    const ccy = u?.currency || snap.config?.display_currency || 'USD';
    if (ccySel && ccySel.value !== ccy) ccySel.value = ccy;

    if (!u || (u.tree_tokens <= 0 && u.session_tokens <= 0)) {
      ribbon.classList.add('hidden');
      return;
    }
    ribbon.classList.remove('hidden');

    const hasTree = u.subagent_count > 0 && u.tree_tokens > u.session_tokens;
    const tokens = hasTree ? u.tree_tokens : u.session_tokens;
    const cost = hasTree ? u.tree_cost : u.session_cost;
    const saved = hasTree ? u.tree_cache_saved : u.session_cache_saved;
    const attrRatio = hasTree ? u.tree_attributed_ratio : u.session_attributed_ratio;

    $('s-usage-label').textContent = hasTree
      ? t('usage.ribbon_tree', { count: u.subagent_count })
      : t('usage.ribbon_session');
    $('s-usage-tokens').textContent = `${fmtTokens(tokens)} tok`;
    $('s-usage-cost').textContent = attrRatio > 0
      ? `≈ ${fmtCurrency(cost, ccy)}`
      : t('usage.unattributed_short');

    const savedEl = $('s-usage-saved');
    if (attrRatio > 0 && saved > 0) {
      savedEl.textContent = t('usage.ribbon_saved', { val: fmtCurrency(saved, ccy) });
      savedEl.classList.remove('hidden');
    } else {
      savedEl.classList.add('hidden');
    }
  }

  function renderUsageReport(report, state) {
    if (!report || report.error) return;
    const ccy = report.currency || 'USD';
    const ccySel = $('usage-currency-select');
    if (ccySel && ccySel.value !== ccy) ccySel.value = ccy;

    document.querySelectorAll('.win-btn[data-uwin]').forEach((b) => {
      b.classList.toggle('active', b.dataset.uwin === (report.window || state.usageWindow));
    });
    document.querySelectorAll('.metric-btn[data-umetric]').forEach((b) => {
      b.classList.toggle('active', b.dataset.umetric === state.usageMetric);
    });

    const hasPriced = (report.attributed_tokens || 0) > 0 || (report.estimated_cost || 0) > 0;
    $('u-cost').textContent = hasPriced ? fmtCurrency(report.estimated_cost, ccy) : '—';
    $('u-saved').textContent = hasPriced ? fmtCurrency(report.cache_saved, ccy) : '—';
    $('u-tokens').textContent = fmtTokens(report.total_tokens || 0);
    $('u-cache').textContent = `${Math.round((report.cache_hit_ratio || 0) * 100)}%`;
    $('u-cache').title = t('usage.sessions_hint', {
      sessions: report.active_sessions || 0,
      turns: report.total_turns || 0,
    });

    const attrPct = Math.round((report.attributed_ratio ?? 1) * 100);
    const attrBanner = $('u-attr-banner');
    if ((report.unattributed_tokens || 0) > 0) {
      attrBanner.textContent = t('usage.unattributed_banner', {
        pct: attrPct,
        tokens: fmtTokens(report.unattributed_tokens),
      });
      attrBanner.classList.remove('hidden');
    } else {
      attrBanner.classList.add('hidden');
    }

    const srcLbl = report.fx_source === 'live'
      ? t('usage.fx_live')
      : report.fx_source === 'custom'
        ? t('usage.fx_custom')
        : t('usage.fx_offline');
    const ageLbl = report.fx_updated_at_ms ? ` · ${fmtRelTime(report.fx_updated_at_ms)}` : '';
    const rateStr = ccy === 'USD' ? 'USD' : `1 USD = ${Number(report.fx_rate || 1).toFixed(2)} ${ccy}`;
    $('u-fx-meta').textContent = `${rateStr} · ${srcLbl}${ageLbl}`;

    $('usage-chart-title').textContent = report.window === '24h'
      ? t('usage.trend_hourly')
      : t('usage.trend_daily');

    const modelsBox = $('usage-models');
    const models = report.models || [];
    if (!models.length) {
      modelsBox.innerHTML = `<div class="empty-state">${escapeHtml(t('usage.empty'))}</div>`;
    } else {
      const maxTok = models[0]?.total_tokens || 1;
      modelsBox.innerHTML = models
        .map((m) => {
          const w = Math.max(4, Math.round((m.total_tokens / maxTok) * 100));
          const label = m.attributed ? m.model : t('usage.unattributed_row');
          const costStr = m.priced ? fmtCurrency(m.estimated_cost, ccy) : '—';
          const hitStr = `${Math.round((m.cache_hit_ratio || 0) * 100)}%`;
          return `
            <div class="prop-row${m.attributed ? '' : ' muted-row'}">
              <div class="prop-fill" style="width:${w}%"></div>
              <span class="prop-label" title="${escapeHtml(label)}">${escapeHtml(label)}</span>
              <span class="prop-count">${escapeHtml(fmtTokens(m.total_tokens))} <span style="opacity:.65">(${hitStr})</span> · ${escapeHtml(costStr)}</span>
            </div>`;
        })
        .join('');
    }

    try {
      renderUsageChart(report, state.usageMetric);
    } catch {
      // keep UI responsive
    }
  }

  function renderSnapshot(snap, state, onRollback, onSelectSession) {
    const cfg = snap.config || {};
    const eng = snap.engine || {};
    const pulse = snap.pulse || null;
    const bench = snap.benchmark || null;
    const tree = snap.session_tree || null;
    const activeConvId = pulse?.conversation_id || bench?.conversation_id || '';

    $('version-pill').textContent = `v${eng.version || snap.version || '0.1.0'}`;
    const isUds = snap.transport === 'uds-daemon';
    $('daemon-dot').classList.toggle('warn', !isUds);
    $('daemon-label').textContent = isUds
      ? t('app.daemon_online', { rtt: snap.rpc_rtt_ms ?? 0 })
      : t('app.daemon_fallback', { rtt: snap.rpc_rtt_ms ?? 0 });

    renderSessionSelect(snap.recent_sessions, tree, state.selectedConvId, activeConvId);
    renderSubagentStrip(tree, activeConvId, onSelectSession);
    renderSessionUsageRibbon(snap);

    const selectedSub = tree?.selected_is_subagent
      ? (tree.subagents || []).find((s) => s.conversation_id === activeConvId) || null
      : null;

    const hChip = $('health-chip');
    hChip.className = 'status-pill';
    hChip.title = '';
    if (selectedSub && !selectedSub.active) {
      hChip.classList.add(pulse?.health === 'red' ? 'muted-peak' : 'muted');
      hChip.textContent = t('subagents.done_pill');
      if (pulse?.health === 'red') hChip.title = t('subagents.done_peak_hint');
    } else if (pulse?.health === 'red') {
      hChip.classList.add('err');
      hChip.textContent = selectedSub ? t('subagents.live_redline') : t('pulse.redline');
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
    renderUsageReport,
  };
})();
