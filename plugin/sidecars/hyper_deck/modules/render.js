(() => {
  const root = (window.HyperDeck = window.HyperDeck || {});
  const { $, copyTextReliable, escapeHtml, fmtCurrency, fmtRelTime, fmtSec, fmtTokens, showToast, t } = root.api;
  const { renderFullContextChart, renderHeroChart, renderUsageChart, renderWaterfallChart } = root.charts;

  function fmtIdleShort(secs) {
    const s = Math.max(0, Math.round(secs || 0));
    if (s >= 60) return `${Math.floor(s / 60)}m${String(s % 60).padStart(2, '0')}s`;
    return `${s}s`;
  }

  function fmtBytesShort(bytes) {
    const b = Math.max(0, Number(bytes) || 0);
    if (b >= 1024 * 1024) return `${(b / (1024 * 1024)).toFixed(1)}MB`;
    if (b >= 1024) return `${(b / 1024).toFixed(0)}KB`;
    return `${b}B`;
  }

  function renderWarmthMeter(snap) {
    if (!snap) return;
    const p = snap.pulse;
    const warmWin = (p?.effective_cache_ttl_secs >= 30 ? p.effective_cache_ttl_secs : 0)
      || snap.config?.cache_warm_window_secs
      || 240;
    const ttlSource = p?.cache_ttl_source || 'default';
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

    const srcEl = $('warmth-ttl-source');
    if (srcEl) {
      const isEmp = ttlSource === 'empirical';
      srcEl.textContent = isEmp
        ? t('pulse.ttl_empirical', { secs: warmWin })
        : t('pulse.ttl_default', { secs: warmWin });
      const matchingEst = (snap.model_ttl_estimates || []).find(
        (e) => e.source === 'empirical' && e.effective_ttl_secs === warmWin
      );
      srcEl.title = isEmp
        ? t('pulse.ttl_tooltip_empirical', {
            secs: warmWin,
            samples: matchingEst?.total_samples || '20+',
            model: matchingEst?.model_family || p?.model || 'model',
          })
        : t('pulse.ttl_tooltip_default', { secs: warmWin });
      srcEl.classList.remove('hidden');
    }

    const fill = $('warmth-fill');
    fill.className = `meter-fill ${state}`;
    fill.style.width = `${remaining === 0 ? 4 : pct}%`;
    const meter = $('warmth-meter');
    meter.setAttribute('aria-valuemax', String(warmWin));
    meter.setAttribute('aria-valuenow', String(remaining));
    $('cache-hint').textContent = hintTxt;
  }

  function renderContextXray(bench) {
    const box = $('ctx-xray');
    if (!box) return;
    const bd = bench?.context_breakdown;
    const cats = (bd?.categories || []).filter((c) => (c.bytes || 0) > 0);
    if (!cats.length || !bd?.total_bytes) {
      box.classList.add('hidden');
      return;
    }
    box.classList.remove('hidden');

    const colorMap = {
      file_reads: '#38bdf8',
      command_outputs: '#f59e0b',
      hyper_tools: '#10b981',
      other_tools: '#a855f7',
      thinking_text: '#6366f1',
      user_system: '#64748b',
    };

    const pills = [];
    if (bench.first_edit_context_tokens > 0) {
      const feK = `${(bench.first_edit_context_tokens / 1000).toFixed(0)}k`;
      const cls = bench.first_edit_context_tokens >= 180000 ? 'status-pill warn' : 'mono-tag';
      pills.push(`<span class="${cls}">${escapeHtml(t('context_pro.xray_first_edit', { ctx: feK }))}</span>`);
    }
    if ((bench.blanket_read_turns || 0) > 0) {
      pills.push(
        `<span class="status-pill warn">${escapeHtml(t('context_pro.xray_blanket_reads', { count: bench.blanket_read_turns, max: bench.max_parallel_reads || 4 }))}</span>`
      );
    }
    if ((bd.media_items || 0) > 0) {
      pills.push(`<span class="mono-tag">${escapeHtml(t('context_pro.xray_media', { count: bd.media_items }))}</span>`);
    }
    $('ctx-xray-pills').innerHTML = pills.join('');

    $('ctx-xray-bar').innerHTML = cats
      .map((c) => {
        const col = colorMap[c.key] || '#94a3b8';
        const lbl = t(`context_pro.cat_${c.key}`, { defaultValue: c.key });
        const tip = `${lbl}: ${c.share_pct}% (${fmtBytesShort(c.bytes)})`;
        return `<span class="xray-seg" style="width:${Math.max(1.5, c.share_pct)}%;background:${col}" title="${escapeHtml(tip)}"></span>`;
      })
      .join('');

    const sorted = [...cats].sort((a, b) => (b.bytes || 0) - (a.bytes || 0));
    $('ctx-xray-legend').innerHTML = sorted
      .map((c) => {
        const col = colorMap[c.key] || '#94a3b8';
        const lbl = t(`context_pro.cat_${c.key}`, { defaultValue: c.key });
        return `
          <div class="xray-item">
            <span class="xray-item-lbl"><i class="xray-swatch" style="background:${col}"></i>${escapeHtml(lbl)}</span>
            <span class="xray-item-val">${c.share_pct}% · ${fmtBytesShort(c.bytes)}</span>
          </div>`;
      })
      .join('');
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

  function renderDiffHtml(diffState) {
    if (!diffState) return '';
    if (diffState.loading) {
      return `<div class="tx-diff-drawer"><div class="empty-state">${escapeHtml(t('shield.diff_loading'))}</div></div>`;
    }
    if (diffState.error) {
      return `<div class="tx-diff-drawer"><div class="empty-state">${escapeHtml(diffState.error)}</div></div>`;
    }
    const files = diffState.data?.files || [];
    if (!files.length) {
      return `<div class="tx-diff-drawer"><div class="empty-state">${escapeHtml(t('shield.diff_empty'))}</div></div>`;
    }
    const blocks = files
      .map((f) => {
        const delta = `<span class="tx-delta"><span class="tx-delta-add">+${f.lines_added || 0}</span><span class="tx-delta-del">-${f.lines_removed || 0}</span></span>`;
        if (f.unavailable_reason) {
          return `<div class="diff-file"><div class="diff-file-head"><span>${escapeHtml(f.rel_path)}</span>${delta}</div><div class="empty-state">${escapeHtml(f.unavailable_reason)}</div></div>`;
        }
        const raw = f.unified_diff || '';
        const linesHtml = raw
          ? raw
              .split('\n')
              .map((line) => {
                let cls = 'diff-line-ctx';
                if (line.startsWith('@@')) cls = 'diff-line-hunk';
                else if (line.startsWith('+') && !line.startsWith('+++')) cls = 'diff-line-add';
                else if (line.startsWith('-') && !line.startsWith('---')) cls = 'diff-line-del';
                return `<span class="diff-line ${cls}">${escapeHtml(line)}</span>`;
              })
              .join('')
          : `<span class="diff-line diff-line-ctx">${escapeHtml(t('shield.diff_empty'))}</span>`;
        return `<div class="diff-file"><div class="diff-file-head"><span>${escapeHtml(f.rel_path)}</span>${delta}</div><pre class="diff-pre">${linesHtml}</pre></div>`;
      })
      .join('');
    return `<div class="tx-diff-drawer">${blocks}</div>`;
  }

  function renderTxList(container, txs, showFull, onRollback, onToggleDiff, openDiffs = {}) {
    if (!txs?.length) {
      container.innerHTML = `<div class="empty-state">${escapeHtml(t('shield.empty_tx'))}</div>`;
      return;
    }
    container.innerHTML = txs
      .map((tx) => {
        const rawPaths = (tx.files || []).map(txFileDisplay).filter(Boolean);
        const files = rawPaths.map((p) => p.split(/[\\/]/).pop()).join(', ');
        const added = (tx.files || []).reduce((acc, f) => acc + (Number(f?.lines_added) || 0), 0);
        const removed = (tx.files || []).reduce((acc, f) => acc + (Number(f?.lines_removed) || 0), 0);
        const deltaHtml =
          added > 0 || removed > 0
            ? `<span class="tx-delta"><span class="tx-delta-add">+${added}</span><span class="tx-delta-del">-${removed}</span></span>`
            : '';
        const diffState = openDiffs[tx.tx_id];
        const diffBtn = `<button type="button" class="btn-diff ${diffState ? 'active' : ''}" data-diff="${escapeHtml(tx.tx_id)}">${escapeHtml(
          diffState ? t('shield.diff_hide') : t('shield.diff_btn')
        )}</button>`;
        const undoBtn = tx.rolled_back
          ? `<span class="mono-tag">${escapeHtml(t('shield.undone'))}</span>`
          : `<button type="button" class="btn-undo" data-undo="${escapeHtml(tx.tx_id)}">${escapeHtml(t('shield.undo_btn'))}</button>`;
        const tsMs = tx.timestamp_ms || tx.created_at_ms || 0;
        const srcTag = tx.source ? ` · ${tx.source}` : '';
        const meta = showFull
          ? `${tx.tx_id} · ${fmtRelTime(tsMs)} · ${rawPaths.length}f${srcTag}`
          : `${fmtRelTime(tsMs)} · ${rawPaths.length}f${srcTag}`;
        return `
          <div class="tx-card">
            <div class="tx-item">
              <div class="tx-main">
                <span class="tx-files" title="${escapeHtml(rawPaths.join('\n'))}">${escapeHtml(files || tx.tx_id)}${deltaHtml}</span>
                <span class="tx-meta">${escapeHtml(meta)}</span>
              </div>
              <div class="tx-actions">${diffBtn}${undoBtn}</div>
            </div>
            ${renderDiffHtml(diffState)}
          </div>`;
      })
      .join('');

    container.querySelectorAll('.btn-undo[data-undo]').forEach((b) => {
      b.addEventListener('click', () => onRollback(b.dataset.undo, false));
    });
    if (onToggleDiff) {
      container.querySelectorAll('.btn-diff[data-diff]').forEach((b) => {
        b.addEventListener('click', () => onToggleDiff(b.dataset.diff));
      });
    }
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

    const loopCount = tree.error_loop_subagents || (tree.subagents || []).filter((s) => s.error_loop).length;
    const loopPill = $('subagent-loop-pill');
    if (loopPill) {
      if (loopCount > 0) {
        loopPill.textContent = t('subagents.error_loop_badge', { count: loopCount });
        loopPill.classList.remove('hidden');
      } else {
        loopPill.classList.add('hidden');
      }
    }

    const stalledCount = tree.stalled_subagents || (tree.subagents || []).filter((s) => s.stalled).length;
    const stalledPill = $('subagent-stalled-pill');
    if (stalledPill) {
      if (stalledCount > 0) {
        stalledPill.textContent = t('subagents.stalled_badge', { count: stalledCount });
        stalledPill.classList.remove('hidden');
      } else {
        stalledPill.classList.add('hidden');
      }
    }

    const liveRedlines = (tree.subagents || []).filter((s) => s.active && s.health === 'red' && !s.error_loop).length;
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
        const errStreak = sub.consecutive_error_streak || 0;
        const errDetail = sub.last_error_summary ? ` · ${sub.last_error_summary}` : '';
        const ckptMeta = (sub.checkpoint_count || 0) > 0 ? ` · ${sub.checkpoint_count} ckpt` : '';
        const isTokenPeak = sub.health === 'red' && sub.health_reason !== 'error_loop';
        const tipKey = sub.error_loop
          ? 'subagents.row_tooltip_error_loop'
          : sub.stalled
            ? 'subagents.row_tooltip_stalled'
            : sub.active
              ? 'subagents.row_tooltip_active'
              : isTokenPeak
                ? 'subagents.row_tooltip_done_peak'
                : 'subagents.row_tooltip_done';
        const rowTip = t(tipKey, {
          ctx: ctxFull,
          cum: cumTxt,
          turns,
          cache: cachePctNum,
          idle: fmtIdleShort(sub.idle_secs),
          streak: errStreak,
          err: errDetail,
          ckpt: ckptMeta,
        });

        let statsHtml;
        if (sub.error_loop) {
          const loopCls = errStreak >= 3 ? 'err' : 'warn';
          statsHtml = `
              <span>${escapeHtml(ctxFull)}</span>
              <span>${escapeHtml(cachePct)}${escapeHtml(hyperTxt)}</span>
              <span class="status-pill ${loopCls}">${escapeHtml(t('subagents.error_loop_pill', { count: errStreak }))}</span>
              <button type="button" class="subagent-rescue-btn ${loopCls}" data-rescue-id="${escapeHtml(sub.conversation_id)}" title="${escapeHtml(t('subagents.rescue_btn_tip'))}">${escapeHtml(t('subagents.rescue_btn'))}</button>`;
        } else if (sub.stalled) {
          statsHtml = `
              <span>${escapeHtml(ctxFull)}</span>
              <span>${escapeHtml(cachePct)}${escapeHtml(hyperTxt)}</span>
              <span class="status-pill warn">${escapeHtml(t('subagents.stalled_pill', { idle: fmtIdleShort(sub.idle_secs) }))}</span>
              <button type="button" class="subagent-rescue-btn warn" data-rescue-id="${escapeHtml(sub.conversation_id)}" title="${escapeHtml(t('subagents.rescue_btn_tip'))}">${escapeHtml(t('subagents.rescue_btn'))}</button>`;
        } else if (sub.active) {
          const hCls = sub.health === 'red' ? 'err' : sub.health === 'yellow' ? 'warn' : 'ok';
          const hLbl = sub.health === 'red' ? 'RED' : sub.health === 'yellow' ? 'WARN' : 'OK';
          statsHtml = `
              <span>${escapeHtml(ctxFull)}</span>
              <span>${escapeHtml(cachePct)}${escapeHtml(hyperTxt)}</span>
              <span class="status-pill ${hCls}">${hLbl}</span>`;
        } else {
          const pillCls = isTokenPeak ? 'muted-peak' : 'muted';
          const doneLbl = ctxShort !== '—'
            ? t('subagents.done_ctx', { ctx: ctxShort })
            : t('subagents.done_pill');
          statsHtml = `
              <span>${escapeHtml(cachePct)}${escapeHtml(hyperTxt)}</span>
              <span class="status-pill ${pillCls}">${escapeHtml(doneLbl)}</span>`;
        }

        const dotCls = sub.error_loop ? ' err-loop' : sub.stalled ? ' stalled' : sub.active ? ' live' : '';
        const rowStateCls = sub.error_loop ? ' err-loop' : sub.stalled ? ' stalled' : '';
        return `
          <div role="button" tabindex="0" class="subagent-row${sub.active ? '' : ' done'}${rowStateCls}${isSel ? ' selected' : ''}" data-subconv="${escapeHtml(sub.conversation_id)}" title="${escapeHtml(rowTip)}">
            <span class="subagent-row-main">
              <i class="subagent-dot${dotCls}"></i>
              <span class="subagent-role">${escapeHtml(role)}</span>
              <span class="subagent-title">${escapeHtml(title)}</span>
            </span>
            <span class="subagent-stats">${statsHtml}
            </span>
          </div>`;
      })
      .join('');

    const subById = new Map(tree.subagents.map((s) => [s.conversation_id, s]));
    list.querySelectorAll('.subagent-row[data-subconv]').forEach((row) => {
      row.addEventListener('click', () => {
        if (typeof onSelectSession === 'function') onSelectSession(row.dataset.subconv);
      });
      row.addEventListener('keydown', (e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          if (typeof onSelectSession === 'function') onSelectSession(row.dataset.subconv);
        }
      });
    });
    list.querySelectorAll('.subagent-rescue-btn[data-rescue-id]').forEach((btn) => {
      btn.addEventListener('click', async (e) => {
        e.stopPropagation();
        const sub = subById.get(btn.dataset.rescueId);
        if (!sub) return;
        const role = sub.agent_name || 'subagent';
        const shortId = (sub.conversation_id || '').slice(0, 8);
        const errPart = sub.last_error_summary ? ` (${sub.last_error_summary})` : '';
        const prompt = sub.error_loop
          ? t('subagents.rescue_prompt_loop', {
              role,
              id: shortId,
              count: sub.consecutive_error_streak || 2,
              err: errPart,
            })
          : t('subagents.rescue_prompt_stalled', {
              role,
              id: shortId,
              idle: fmtIdleShort(sub.idle_secs),
            });
        const ok = await copyTextReliable(prompt);
        if (ok) {
          showToast(t('subagents.copied_rescue', { role, id: shortId }));
          const prev = btn.textContent;
          btn.textContent = t('pulse.copied_short');
          setTimeout(() => { btn.textContent = prev; }, 1600);
        } else {
          showToast(t('pulse.copy_fallback'));
        }
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

  function renderAuditPanel(audit) {
    const block = $('usage-audit-block');
    if (!block) return;
    if (!audit || audit.error || (!audit.sessions_analyzed && !(audit.model_ttl_cliffs || []).length)) {
      block.classList.add('hidden');
      return;
    }
    block.classList.remove('hidden');

    const pills = [];
    if (audit.first_edit_p50_tokens > 0) {
      pills.push(
        `<span class="mono-tag">${escapeHtml(t('usage.audit_first_edit', { ctx: fmtTokens(audit.first_edit_p50_tokens) }))}</span>`
      );
    }
    if (audit.blanket_read_turns > 0) {
      pills.push(
        `<span class="status-pill warn">${escapeHtml(t('usage.audit_blanket', { count: audit.blanket_read_turns }))}</span>`
      );
    }
    if (audit.uncached_input_cost > 0) {
      pills.push(
        `<span class="mono-tag">${escapeHtml(
          t('usage.audit_uncached', { val: fmtCurrency(audit.uncached_input_cost, audit.currency || 'USD') })
        )}</span>`
      );
    }
    $('audit-kpi-pills').innerHTML = pills.join('');

    const cliffs = audit.model_ttl_cliffs || [];
    $('audit-ttl-chips').innerHTML = cliffs
      .slice(0, 6)
      .map((m) => {
        const cls = m.effective_ttl_secs <= 120 ? 'high' : m.effective_ttl_secs <= 180 ? 'warn' : 'ok';
        const tag = m.source === 'empirical' ? 'emp' : 'def';
        return `<span class="audit-ttl-chip ${cls}" title="n=${m.total_samples}">${escapeHtml(m.model_family)} · ${m.effective_ttl_secs}s (${tag})</span>`;
      })
      .join('');

    const findings = audit.findings || [];
    $('audit-findings-list').innerHTML = findings
      .slice(0, 3)
      .map(
        (f) => `
          <div class="audit-finding-row">
            <span class="audit-finding-dot ${escapeHtml(f.severity || 'ok')}"></span>
            <span>${escapeHtml(f.title)}</span>
          </div>`
      )
      .join('');
  }

  function renderSnapshot(snap, state, onRollback, onSelectSession, onToggleDiff) {
    const cfg = snap.config || {};
    const eng = snap.engine || {};
    const pulse = snap.pulse || null;
    const bench = snap.benchmark || null;
    const tree = snap.session_tree || null;
    const activeConvId = pulse?.conversation_id || bench?.conversation_id || '';

    $('version-pill').textContent = `v${eng.version || snap.version || '0.4.0'}`;
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
      const endedOnTokenPeak = pulse?.health === 'red' && pulse?.health_reason !== 'error_loop';
      hChip.classList.add(endedOnTokenPeak ? 'muted-peak' : 'muted');
      hChip.textContent = t('subagents.done_pill');
      if (endedOnTokenPeak) hChip.title = t('subagents.done_peak_hint');
    } else if (pulse?.health_reason === 'error_loop') {
      hChip.classList.add(pulse.health === 'red' ? 'err' : 'warn');
      hChip.textContent = t('pulse.error_loop', { count: pulse.consecutive_error_streak || 2 });
      if (pulse.repeat_error_signature) hChip.title = pulse.repeat_error_signature;
    } else if (pulse?.health === 'red') {
      hChip.classList.add('err');
      hChip.textContent = selectedSub ? t('subagents.live_redline') : t('pulse.redline');
    } else if (pulse?.health === 'yellow') {
      hChip.classList.add('warn');
      hChip.textContent = t('pulse.warming');
    } else if (pulse?.recently_compacted && !selectedSub) {
      hChip.classList.add('ok');
      hChip.textContent = t('pulse.compacted');
      hChip.title = t('pulse.compacted_hint', { ctx: ((pulse?.total_tokens || 0) / 1000).toFixed(1) });
    } else {
      hChip.classList.add('ok');
      hChip.textContent = t('pulse.healthy');
    }

    const copyBtn = $('btn-copy-compact');
    if (copyBtn) {
      const isMainLoop =
        pulse?.health_reason === 'error_loop' && !(selectedSub && !selectedSub.active);
      copyBtn.dataset.action = isMainLoop ? 'pivot' : 'compact';
      copyBtn.dataset.streak = String(pulse?.consecutive_error_streak || 2);
      copyBtn.dataset.err = pulse?.repeat_error_signature || '';
      if (!copyBtn.dataset.copiedLock) {
        copyBtn.textContent = isMainLoop ? t('pulse.copy_pivot') : t('pulse.copy_compact');
      }
    }

    const totalTok = pulse?.total_tokens || 0;
    $('s-ctx').textContent = totalTok > 0 ? `${(totalTok / 1000).toFixed(1)}k` : '—';
    $('s-p50').textContent = bench?.median_turn_ms ? fmtSec(bench.median_turn_ms) : '—';
    const stepHitPct = pulse ? Math.round((pulse.cache_hit_ratio || 0) * 100) : null;
    const sessHitPct = bench ? Math.round((bench.cache_hit_ratio || 0) * 100) : null;
    $('s-cache').textContent = stepHitPct !== null
      ? `${stepHitPct}%`
      : sessHitPct !== null
        ? `${sessHitPct}%`
        : '—';
    const cacheTabEl = $('s-cache')?.closest('.kpi-tab');
    if (cacheTabEl) {
      cacheTabEl.title = stepHitPct !== null && sessHitPct !== null
        ? t('pulse.cache_step_vs_session', { step: stepHitPct, avg: sessHitPct })
        : '';
    }

    const sessionHyperTurns = bench?.hyper_tool_calls ?? 0;
    const turnsBasis = bench ? sessionHyperTurns : eng.total_turns_saved || 0;
    const llmP50 = bench?.median_llm_ms || 0;
    const estSavedMs = turnsBasis > 0 && llmP50 > 0 ? turnsBasis * llmP50 : 0;
    $('s-saved').textContent = fmtSec(estSavedMs);
    $('s-saved-cell').title = t('kpi.est_saved_hint', { turns: turnsBasis });

    const alertEl = $('cliff-alert');
    if (pulse?.consecutive_error_streak >= 2) {
      const sig = pulse.repeat_error_signature ? ` (${pulse.repeat_error_signature})` : '';
      alertEl.textContent = `${t('pulse.cliff_error_loop', { count: pulse.consecutive_error_streak })}${sig}`;
      alertEl.className = pulse.consecutive_error_streak >= 3 ? 'inline-alert err' : 'inline-alert';
    } else if (pulse?.multimodal_cliff_risk) {
      alertEl.textContent = t('pulse.cliff_multimodal', { count: pulse.media_items_recent });
      alertEl.className = 'inline-alert';
    } else if (pulse?.recent_timeouts > 0) {
      alertEl.textContent = t('pulse.cliff_timeout', { count: pulse.recent_timeouts });
      alertEl.className = 'inline-alert err';
    } else if (pulse?.health_reason === 'proxy_reset') {
      alertEl.textContent = t('pulse.cliff_reset', { count: pulse.recent_resets || 2 });
      alertEl.className = 'inline-alert';
    } else if (pulse?.model_switched_from && pulse?.recently_compacted) {
      alertEl.textContent = t('pulse.cliff_compact_model_switch', { prev: pulse.model_switched_from });
      alertEl.className = 'inline-alert';
    } else if (pulse?.model_switched_from && (pulse.cache_hit_ratio || 0) < 0.35) {
      alertEl.textContent = t('pulse.cliff_model_switch', { prev: pulse.model_switched_from });
      alertEl.className = 'inline-alert info';
    } else if (pulse?.recently_compacted && totalTok > 0) {
      alertEl.textContent = t('pulse.cliff_compacted', { ctx: (totalTok / 1000).toFixed(1) });
      alertEl.className = 'inline-alert ok';
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
    renderTxList($('simple-tx-list'), txs.slice(0, 4), false, onRollback, onToggleDiff, state.openDiffs);
    renderTxList($('pro-wal-list'), txs, true, onRollback, onToggleDiff, state.openDiffs);

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

    if ($('tog-mcp-read')) $('tog-mcp-read').checked = cfg.enable_mcp_read !== false;
    if ($('tog-mcp-edit')) $('tog-mcp-edit').checked = cfg.enable_mcp_edit !== false;
    if ($('tog-mcp-check')) $('tog-mcp-check').checked = cfg.enable_mcp_check !== false;
    $('tog-prewarm').checked = Boolean(cfg.prewarm_tier2);
    $('tog-secret').checked = Boolean(cfg.block_secret_files);
    $('tog-argv').checked = Boolean(cfg.allow_custom_argv_checks);
    $('tog-pill').checked = Boolean(cfg.enable_ui_pill);
    if ($('tog-watchdog-notify')) $('tog-watchdog-notify').checked = cfg.enable_watchdog_notify !== false;

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
    renderContextXray(bench);
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
    renderAuditPanel,
  };
})();
