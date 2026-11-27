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
    if (bench.pre_edit_delta_tokens !== null && bench.pre_edit_delta_tokens !== undefined) {
      const dCls = bench.pre_edit_delta_tokens >= 120000 ? 'status-pill err' : bench.pre_edit_delta_tokens >= 70000 ? 'status-pill warn' : 'mono-tag';
      const tip = bench.first_edit_context_tokens > 0
        ? t('context_pro.xray_first_edit', { ctx: `${(bench.first_edit_context_tokens / 1000).toFixed(0)}k` })
        : '';
      pills.push(
        `<span class="${dCls}"${tip ? ` title="${escapeHtml(tip)}"` : ''}>${escapeHtml(
          t('context_pro.xray_pre_edit_delta', { delta: fmtTokens(bench.pre_edit_delta_tokens) })
        )}</span>`
      );
    } else if (bench.first_edit_context_tokens > 0) {
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

  function renderContextSplit(bench, tree) {
    const box = $('ctx-split-block');
    if (!box) return;
    const ss = bench?.split_stats || null;
    const tsc = tree?.split_cache || null;
    const switches = ss?.model_switches || [];
    const hasSwitches = (ss?.model_switch_count || 0) > 0 || switches.length > 0;
    const hasHandoffs = (tsc?.subagent_handoffs || 0) > 0;
    if (!hasSwitches && !hasHandoffs) {
      box.classList.add('hidden');
      return;
    }
    box.classList.remove('hidden');

    const switchDropPct = Math.round((ss?.avg_switch_cache_drop || 0) * 100);
    const treeDropPct = Math.round(tsc?.cache_drop_pct || 0);
    const dropPct = Math.max(switchDropPct, treeDropPct);
    const uncachedTok = Math.max(
      Number(ss?.switch_uncached_input_tokens || 0),
      Number(tsc?.uncached_penalty_tokens || 0)
    );
    const pills = [];
    if (dropPct > 0) {
      const cls = dropPct >= 35 ? 'err' : dropPct >= 15 ? 'warn' : 'ok';
      pills.push(`<span class="status-pill ${cls}">${escapeHtml(t('context_pro.split_drop_pill', { pct: dropPct }))}</span>`);
    }
    if (uncachedTok > 0) {
      pills.push(`<span class="mono-tag">${escapeHtml(t('context_pro.split_uncached_pill', { tokens: fmtTokens(uncachedTok) }))}</span>`);
    }
    $('ctx-split-pills').innerHTML = pills.join('');

    const rows = [];
    if (hasHandoffs) {
      const orch = Math.round((tsc.orchestrator_cache_hit_ratio || 0) * 100);
      const exec = Math.round((tsc.executor_cache_hit_ratio || 0) * 100);
      const cold = Math.round((tsc.executor_cold_start_hit_ratio || 0) * 100);
      const label = t('context_pro.split_orch_exec_row', { count: tsc.subagent_handoffs });
      const val = t('context_pro.split_orch_exec_val', { orch, exec, cold });
      rows.push(`
        <div class="split-trans-row">
          <span class="split-trans-main">
            <span class="split-trans-kind subagent">${escapeHtml(t('usage.split_kind_subagent'))}</span>
            <span class="split-trans-route" title="${escapeHtml(label)}">${escapeHtml(label)}</span>
          </span>
          <span class="split-trans-meta">${escapeHtml(val)}</span>
        </div>`);
    }
    for (const sw of [...switches].slice(-3).reverse()) {
      const pre = Math.round((sw.prev_cache_hit_ratio || 0) * 100);
      const post = Math.round((sw.post_cache_hit_ratio || 0) * 100);
      const swDrop = Math.round((sw.cache_drop_ratio || 0) * 100);
      const dCls = swDrop >= 35 ? 'err' : swDrop >= 15 ? 'warn' : 'ok';
      const route = t('context_pro.split_step_label', {
        step: sw.step_index ?? 0,
        from: sw.from_model || '?',
        to: sw.to_model || '?',
      });
      const uncached = fmtTokens(sw.uncached_input_tokens || 0);
      rows.push(`
        <div class="split-trans-row">
          <span class="split-trans-main">
            <span class="split-trans-kind switch">${escapeHtml(t('usage.split_kind_switch'))}</span>
            <span class="split-trans-route" title="${escapeHtml(route)}">${escapeHtml(route)}</span>
          </span>
          <span class="split-trans-meta">${pre}%→${post}% <strong class="${dCls}">(-${swDrop}pp)</strong> · ${escapeHtml(uncached)}</span>
        </div>`);
    }
    $('ctx-split-list').innerHTML = rows.join('');
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

  function renderSubagentStrip(tree, activeConvId, onSelectSession, collisions = []) {
    const strip = $('subagent-strip');
    if (!strip) return;
    if (!tree || !tree.subagents?.length) {
      strip.classList.add('hidden');
      return;
    }
    strip.classList.remove('hidden');

    $('subagent-heading').textContent = t('subagents.heading', { count: tree.subagent_count });
    const wfPill = $('subagent-wf-pill');
    if (wfPill) {
      if ((tree.workflow_node_count || 0) > 0) {
        wfPill.textContent = t('subagents.workflow_badge', { count: tree.workflow_node_count });
        wfPill.classList.remove('hidden');
      } else {
        wfPill.classList.add('hidden');
      }
    }
    const colPill = $('subagent-collision-pill');
    if (colPill) {
      const topCol = (collisions || [])[0];
      if (topCol) {
        const isRace = topCol.kind === 'same_file_race';
        colPill.className = `status-pill ${isRace ? 'err' : 'warn'}`;
        colPill.textContent = isRace
          ? t('subagents.collision_race_badge', { count: (topCol.rel_paths || []).length || 1 })
          : t('subagents.collision_shared_badge', { count: topCol.writer_count || 2 });
        colPill.title = t('subagents.collision_tip', {
          writers: topCol.writer_count || 2,
          files: (topCol.rel_paths || []).join(', ') || 'workspace',
        });
        colPill.classList.remove('hidden');
      } else {
        colPill.classList.add('hidden');
      }
    }
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

    const splitPill = $('subagent-split-pill');
    if (splitPill) {
      const tsc = tree.split_cache;
      const dropPct = Math.round(tsc?.cache_drop_pct || 0);
      if (tsc && (tsc.subagent_handoffs > 0 || tsc.model_switch_count > 0) && dropPct >= 10) {
        splitPill.className = `status-pill ${dropPct >= 35 ? 'err' : 'warn'}`;
        splitPill.textContent = t('subagents.split_drop_badge', { pct: dropPct });
        splitPill.title = t('subagents.split_drop_tip', {
          orch: Math.round((tsc.orchestrator_cache_hit_ratio || 0) * 100),
          exec: Math.round((tsc.executor_cache_hit_ratio || 0) * 100),
          cold: Math.round((tsc.executor_cold_start_hit_ratio || 0) * 100),
          penalty: fmtTokens(tsc.uncached_penalty_tokens || 0),
        });
        splitPill.classList.remove('hidden');
      } else {
        splitPill.classList.add('hidden');
      }
    }

    $('btn-back-parent').classList.toggle('hidden', !tree.selected_is_subagent);
    const treeCcy = tree.currency || 'USD';
    $('subagent-tree-meta').textContent = (tree.tree_cost || 0) > 0
      ? t('subagents.tree_meta_cost', {
          turns: tree.combined_turns || 0,
          tokens: fmtTokens(tree.combined_tokens || 0),
          tree_cost: fmtCurrency(tree.tree_cost || 0, treeCcy),
          root_cost: fmtCurrency(tree.root_cost || 0, treeCcy),
          fleet_cost: fmtCurrency(tree.fleet_worker_cost || 0, treeCcy),
          cache: Math.round((tree.combined_cache_hit_ratio || 0) * 100),
        })
      : t('subagents.tree_meta', {
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
    const openUsageBtn = $('btn-open-usage');
    if (openUsageBtn) {
      openUsageBtn.title = hasTree && attrRatio > 0
        ? t('usage.ribbon_tree_tip', {
            root: fmtCurrency(u.session_cost || 0, ccy),
            fleet: fmtCurrency(Math.max(0, (u.tree_cost || 0) - (u.session_cost || 0)), ccy),
            count: u.subagent_count,
          })
        : '';
    }
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

    const verdictEl = $('audit-verdict-pill');
    if (verdictEl) {
      const v = String(audit.verdict || '').toUpperCase();
      if (v === 'BLEEDING' || v === 'WATCH' || v === 'HEALTHY') {
        const cls = v === 'BLEEDING' ? 'err' : v === 'WATCH' ? 'warn' : 'ok';
        const lblKey = v === 'BLEEDING' ? 'usage.verdict_bleeding' : v === 'WATCH' ? 'usage.verdict_watch' : 'usage.verdict_healthy';
        verdictEl.className = `status-pill ${cls}`;
        verdictEl.textContent = t(lblKey);
        verdictEl.title = audit.verdict_reason || '';
        verdictEl.classList.remove('hidden');
      } else {
        verdictEl.classList.add('hidden');
      }
    }

    const pills = [];
    if (audit.pre_edit_delta_p50_tokens !== null && audit.pre_edit_delta_p50_tokens !== undefined) {
      const dCls =
        audit.pre_edit_delta_p50_tokens >= 120000
          ? 'status-pill err'
          : audit.pre_edit_delta_p50_tokens >= 70000
            ? 'status-pill warn'
            : 'mono-tag';
      const tip = audit.first_edit_p50_tokens > 0
        ? t('usage.audit_pre_edit_delta_tip', {
            delta: fmtTokens(audit.pre_edit_delta_p50_tokens),
            raw: fmtTokens(audit.first_edit_p50_tokens),
          })
        : '';
      pills.push(
        `<span class="${dCls}"${tip ? ` title="${escapeHtml(tip)}"` : ''}>${escapeHtml(
          t('usage.audit_pre_edit_delta', { delta: fmtTokens(audit.pre_edit_delta_p50_tokens) })
        )}</span>`
      );
    } else if (audit.first_edit_p50_tokens > 0) {
      pills.push(
        `<span class="mono-tag">${escapeHtml(t('usage.audit_first_edit', { ctx: fmtTokens(audit.first_edit_p50_tokens) }))}</span>`
      );
    }
    if ((audit.transport_error_count || 0) > 0) {
      const rCls = audit.transport_error_count >= 3 ? 'status-pill err' : 'status-pill warn';
      const rTip = t('usage.audit_transport_resets_tip', {
        count: audit.transport_error_count,
        eof: audit.eof_reset_count || 0,
        time: fmtSec(audit.total_retry_backoff_ms || 0),
      });
      pills.push(
        `<span class="${rCls}" title="${escapeHtml(rTip)}">${escapeHtml(
          t('usage.audit_transport_resets', {
            count: audit.transport_error_count,
            time: fmtSec(audit.total_retry_backoff_ms || 0),
          })
        )}</span>`
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
    if (audit.top_quota_drain?.model_family) {
      pills.push(
        `<span class="mono-tag">${escapeHtml(
          t('usage.audit_top_drain', {
            model: audit.top_quota_drain.model_family,
            pct: Math.round(audit.top_quota_drain.share_pct || 0),
          })
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

    const splitBadges = $('audit-split-badges');
    const splitEmpty = $('audit-split-empty');
    const splitBody = $('audit-split-body');
    const splitKpis = $('audit-split-kpis');
    const splitList = $('audit-split-list');
    if (splitBadges && splitEmpty && splitBody && splitKpis && splitList) {
      const sc = audit.split_cache || null;
      const totalTransitions = Number(sc?.total_transitions || 0);
      if (!sc || totalTransitions === 0) {
        splitBadges.innerHTML = '';
        splitEmpty.classList.remove('hidden');
        splitBody.classList.add('hidden');
        splitKpis.innerHTML = '';
        splitList.innerHTML = '';
      } else {
        splitEmpty.classList.add('hidden');
        splitBody.classList.remove('hidden');
        const dropPct = Math.round(sc.headline_cache_drop_pct || 0);
        const dropCls = dropPct >= 35 ? 'err' : dropPct >= 15 ? 'warn' : 'ok';
        const ccy = audit.currency || 'USD';
        splitBadges.innerHTML = [
          `<span class="status-pill ${dropCls}">${escapeHtml(t('usage.split_drop_badge', { pct: dropPct }))}</span>`,
          `<span class="mono-tag">${escapeHtml(t('usage.split_count_badge', { count: totalTransitions }))}</span>`,
        ].join('');

        const penTok = fmtTokens(sc.uncached_penalty_tokens || 0);
        const penCost = (sc.est_uncached_penalty || 0) > 0 ? ` · ${fmtCurrency(sc.est_uncached_penalty, ccy)}` : '';
        const hasExec = (sc.executor_turns || 0) > 0;
        const thirdLabel = hasExec ? t('usage.split_kpi_orch_exec') : t('usage.split_kpi_switches');
        const thirdVal = hasExec
          ? `${Math.round((sc.orchestrator_cache_hit_ratio || 0) * 100)}% / ${Math.round((sc.executor_cache_hit_ratio || 0) * 100)}%`
          : `${sc.model_switch_count || 0}×`;
        splitKpis.innerHTML = `
          <div class="split-kpi-cell">
            <span>${escapeHtml(t('usage.split_kpi_drop'))}</span>
            <strong class="${dropCls}">-${dropPct}%</strong>
          </div>
          <div class="split-kpi-cell">
            <span>${escapeHtml(t('usage.split_kpi_penalty'))}</span>
            <strong title="${escapeHtml(penTok + penCost)}">${escapeHtml(penTok + penCost)}</strong>
          </div>
          <div class="split-kpi-cell">
            <span>${escapeHtml(thirdLabel)}</span>
            <strong>${escapeHtml(thirdVal)}</strong>
          </div>`;

        const transitions = (sc.top_transitions || []).slice(0, 3);
        splitList.innerHTML = transitions
          .map((tr) => {
            const isSub = tr.kind === 'subagent_handoff';
            const kindLbl = isSub ? t('usage.split_kind_subagent') : t('usage.split_kind_switch');
            const kindCls = isSub ? 'subagent' : 'switch';
            const cnt = tr.transitions > 1 ? ` ×${tr.transitions}` : '';
            const route = `${tr.from_model || '?'} → ${tr.to_model || '?'}${cnt}`;
            const pre = Math.round((tr.pre_cache_hit_ratio || 0) * 100);
            const post = Math.round((tr.post_cache_hit_ratio || 0) * 100);
            const trDrop = Math.round(tr.cache_drop_pct || 0);
            const trDropCls = trDrop >= 35 ? 'err' : trDrop >= 15 ? 'warn' : 'ok';
            const uncached = fmtTokens(tr.uncached_input_tokens || 0);
            return `
              <div class="split-trans-row">
                <span class="split-trans-main">
                  <span class="split-trans-kind ${kindCls}">${escapeHtml(kindLbl)}</span>
                  <span class="split-trans-route" title="${escapeHtml(route)}">${escapeHtml(route)}</span>
                </span>
                <span class="split-trans-meta">${pre}%→${post}% <strong class="${trDropCls}">(-${trDrop}pp)</strong> · ${escapeHtml(uncached)}</span>
              </div>`;
          })
          .join('');
      }
    }

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

  function renderUpdateRadar(snap, state) {
    const pill = $('update-pill');
    const banner = $('update-banner');
    if (!pill || !banner) return;

    const us = snap.update_state || null;
    const cfg = snap.config || {};
    const latest = String(us?.latest_version || '').trim().replace(/^v/i, '');
    const dismissed = String(us?.dismissed_version || '').trim().replace(/^v/i, '');
    const enabled = cfg.enable_update_check !== false || Boolean(state.forceShowUpdate);
    const actionable = Boolean(
      enabled &&
        us?.update_available &&
        latest &&
        (Boolean(state.forceShowUpdate) || dismissed !== latest)
    );

    if (!actionable) {
      pill.classList.add('hidden');
      pill.classList.remove('active');
      pill.setAttribute('aria-expanded', 'false');
      banner.classList.add('hidden');
      state.updateBannerOpen = false;
      return;
    }

    pill.textContent = t('update.pill_badge', { version: latest });
    pill.title = t('update.pill_tip', { version: latest });
    pill.classList.remove('hidden');
    pill.classList.toggle('active', Boolean(state.updateBannerOpen));
    pill.setAttribute('aria-expanded', String(Boolean(state.updateBannerOpen)));

    if (state.updateBannerOpen) {
      banner.classList.remove('hidden');
      const txtEl = $('update-banner-text');
      if (txtEl) txtEl.textContent = t('update.banner_title', { version: latest });
      const notesEl = $('btn-update-notes');
      if (notesEl) {
        notesEl.href =
          us?.release_url || `https://github.com/BobbyCats/agy-hyper/releases/tag/v${latest}`;
      }
      const nowBtn = $('btn-update-now');
      if (nowBtn) {
        nowBtn.disabled = Boolean(state.updateApplying);
        nowBtn.textContent = state.updateApplying ? t('update.btn_updating') : t('update.btn_now');
      }
    } else {
      banner.classList.add('hidden');
    }
  }

  function renderSnapshot(snap, state, onRollback, onSelectSession, onToggleDiff) {
    const cfg = snap.config || {};
    const eng = snap.engine || {};
    const pulse = snap.pulse || null;
    const bench = snap.benchmark || null;
    const tree = snap.session_tree || null;
    const activeConvId = pulse?.conversation_id || bench?.conversation_id || '';

    $('version-pill').textContent = `v${eng.version || snap.version || snap.update_state?.current_version || '0.6.0'}`;
    renderUpdateRadar(snap, state);
    const isUds = snap.transport === 'uds-daemon';
    $('daemon-dot').classList.toggle('warn', !isUds);
    $('daemon-label').textContent = isUds
      ? t('app.daemon_online', { rtt: snap.rpc_rtt_ms ?? 0 })
      : t('app.daemon_fallback', { rtt: snap.rpc_rtt_ms ?? 0 });

    renderSessionSelect(snap.recent_sessions, tree, state.selectedConvId, activeConvId);
    renderSubagentStrip(tree, activeConvId, onSelectSession, snap.write_collisions || []);
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
    const txColBadge = $('tx-collision-badge');
    if (txColBadge) {
      const topCol = (snap.write_collisions || [])[0];
      if (topCol) {
        const isRace = topCol.kind === 'same_file_race';
        txColBadge.className = `status-pill ${isRace ? 'err' : 'warn'}`;
        txColBadge.textContent = isRace
          ? t('subagents.collision_race_badge', { count: (topCol.rel_paths || []).length || 1 })
          : t('subagents.collision_shared_badge', { count: topCol.writer_count || 2 });
        txColBadge.title = t('subagents.collision_tip', {
          writers: topCol.writer_count || 2,
          files: (topCol.rel_paths || []).join(', ') || 'workspace',
        });
        txColBadge.classList.remove('hidden');
      } else {
        txColBadge.classList.add('hidden');
      }
    }
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
    if ($('tog-session-naming')) $('tog-session-naming').checked = cfg.session_naming?.enabled !== false;
    if ($('tog-radar-sound')) $('tog-radar-sound').checked = cfg.enable_radar_sound !== false;
    if ($('tog-update-check')) $('tog-update-check').checked = cfg.enable_update_check !== false;

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
    renderContextSplit(bench, tree);
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

  function fmtMonthDay(tsMs) {
    if (!tsMs || tsMs <= 0) return '—';
    const d = new Date(Number(tsMs));
    if (Number.isNaN(d.getTime())) return '—';
    const mm = String(d.getMonth() + 1).padStart(2, '0');
    const dd = String(d.getDate()).padStart(2, '0');
    return `${mm}-${dd}`;
  }

  function fmtSessionAge(tsMs) {
    if (!tsMs || tsMs <= 0) return '—';
    const diffSec = Math.max(0, Math.floor((Date.now() - Number(tsMs)) / 1000));
    const lang = (window.i18next?.language || 'en').toLowerCase();
    const isZh = lang.startsWith('zh');
    const isJa = lang.startsWith('ja');
    if (diffSec < 60) return isZh ? `${diffSec}秒前` : isJa ? `${diffSec}秒前` : `${diffSec}s ago`;
    const mins = Math.floor(diffSec / 60);
    if (mins < 60) return isZh ? `${mins}分钟前` : isJa ? `${mins}分前` : `${mins}m ago`;
    const hrs = Math.floor(mins / 60);
    if (hrs < 24) return isZh ? `${hrs}小时前` : isJa ? `${hrs}時間前` : `${hrs}h ago`;
    const days = Math.floor(hrs / 24);
    return isZh ? `${days}天前` : isJa ? `${days}日前` : `${days}d ago`;
  }

  function highlightTermsHtml(rawText, queryStr) {
    const text = String(rawText || '');
    if (!text) return '';
    const terms = String(queryStr || '')
      .split(/\s+/)
      .map((s) => s.trim())
      .filter(Boolean);
    if (!terms.length) return escapeHtml(text);

    const escapedTerms = terms
      .slice(0, 6)
      .map((term) => term.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'));
    if (!escapedTerms.length) return escapeHtml(text);

    try {
      const re = new RegExp(`(${escapedTerms.join('|')})`, 'gi');
      return text
        .split(re)
        .map((part, idx) =>
          idx % 2 === 1 ? `<mark class="sess-hl">${escapeHtml(part)}</mark>` : escapeHtml(part)
        )
        .join('');
    } catch {
      return escapeHtml(text);
    }
  }

  function stripRedundantCodePrefix(rawTitle, code, group) {
    const s = String(rawTitle || '').trim();
    if (!s || (!code && !group)) return s;
    // Match `<Emoji> <Code>·<Rest>` so when `[Group]` badge is already shown beside it, `<Code>·` isn't repeated
    const m = /^(\S+)\s+([^·｜\s]{1,12})·(.+)$/u.exec(s);
    if (!m) return s;
    const [, emoji, pfx, rest] = m;
    const pfxLower = pfx.trim().toLowerCase();
    const codeLower = String(code || '').trim().toLowerCase();
    const grpLower = String(group || '').trim().toLowerCase();
    if (pfxLower && (pfxLower === codeLower || grpLower.includes(pfxLower))) {
      return `${emoji} ${rest.trim()}`;
    }
    return s;
  }

  function renderRadarGlyph(rawState, size = 15, glyphStyle = 'waveform') {
    const st = ['waiting', 'stalled', 'unread', 'running', 'idle'].includes(rawState) ? rawState : 'idle';
    const tip = escapeHtml(t(`radar.state_${st}`));
    let inner = '';
    if (st === 'running') {
      if (glyphStyle === 'bars_fade') {
        // n3r4zzurr0/svg-spinners `bars-scale-fade`
        inner = `
          <rect class="sp-bar s1" x="2.2" y="2.5" width="2.5" height="11" rx="1.2" fill="currentColor"/>
          <rect class="sp-bar s2" x="6.75" y="2.5" width="2.5" height="11" rx="1.2" fill="currentColor"/>
          <rect class="sp-bar s3" x="11.3" y="2.5" width="2.5" height="11" rx="1.2" fill="currentColor"/>`;
      } else if (glyphStyle === 'pulse_rings') {
        // uiball/ldrs `l-ripples` dual expanding wave rings
        inner = `
          <circle class="ldrs-ripple r1" cx="8" cy="8" r="6.4" stroke="currentColor" stroke-width="1.5"/>
          <circle class="ldrs-ripple r2" cx="8" cy="8" r="6.4" stroke="currentColor" stroke-width="1.5"/>
          <circle cx="8" cy="8" r="2" fill="currentColor"/>`;
      } else {
        // Default: uiball/ldrs `l-waveform` 4-bar sinusoidal phase wave
        inner = `
          <rect class="ldrs-bar b1" x="1.8" y="2.4" width="2.1" height="11.2" rx="1.05" fill="currentColor"/>
          <rect class="ldrs-bar b2" x="5.2" y="2.4" width="2.1" height="11.2" rx="1.05" fill="currentColor"/>
          <rect class="ldrs-bar b3" x="8.6" y="2.4" width="2.1" height="11.2" rx="1.05" fill="currentColor"/>
          <rect class="ldrs-bar b4" x="12.0" y="2.4" width="2.1" height="11.2" rx="1.05" fill="currentColor"/>`;
      }
    } else if (st === 'waiting') {
      // uiball/ldrs `l-dot-wave`: 3 gently undulating prompt beads
      inner = `
        <circle class="ldrs-dot d1" cx="3.6" cy="8" r="1.65" fill="currentColor"/>
        <circle class="ldrs-dot d2" cx="8" cy="8" r="1.65" fill="currentColor"/>
        <circle class="ldrs-dot d3" cx="12.4" cy="8" r="1.65" fill="currentColor"/>`;
    } else if (st === 'unread') {
      // Crisp squircle check-capsule (lucide check)
      inner = `
        <rect x="1.75" y="1.75" width="12.5" height="12.5" rx="3.8" fill="currentColor" fill-opacity="0.14" stroke="currentColor" stroke-width="1.35" stroke-opacity="0.42"/>
        <path d="M5.1 8.15L7.05 10.1L11.05 5.95" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>`;
    } else if (st === 'stalled') {
      // Interrupted ECG / waveform slash
      inner = `
        <path d="M1.8 8H4.6L6.3 4.3L8.8 11.7L10.4 8H14.2" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>
        <line x1="3.2" y1="12.8" x2="12.8" y2="3.2" stroke="currentColor" stroke-width="1.45" stroke-linecap="round" stroke-opacity="0.85"/>`;
    } else {
      // Calm resting 3-bar studio equalizer baseline
      inner = `
        <rect x="3.1" y="6.2" width="1.9" height="3.6" rx="0.95" fill="currentColor" fill-opacity="0.55"/>
        <rect x="7.05" y="4.6" width="1.9" height="6.8" rx="0.95" fill="currentColor" fill-opacity="0.75"/>
        <rect x="11.0" y="6.2" width="1.9" height="3.6" rx="0.95" fill="currentColor" fill-opacity="0.55"/>`;
    }
    return `<span class="radar-glyph ${st}" title="${tip}"><svg width="${size}" height="${size}" viewBox="0 0 16 16" fill="none" aria-hidden="true">${inner}</svg></span>`;
  }

  function renderSoundIcon(enabled) {
    if (enabled) {
      return `<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5"/><path d="M15.54 8.46a5 5 0 0 1 0 7.07"/><path d="M19.07 4.93a10 10 0 0 1 0 14.14"/></svg>`;
    }
    return `<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5"/><line x1="23" y1="9" x2="17" y2="15"/><line x1="17" y1="9" x2="23" y2="15"/></svg>`;
  }

  function renderRadarRowHtml(it, glyphStyle = 'waveform') {
    const isCodex = it.client === 'codex';
    const st = it.live_state || 'idle';
    const clientBadge = isCodex
      ? '<span class="sess-client-badge codex">Codex</span>'
      : '<span class="sess-client-badge agy">AGY</span>';
    const bizLabel = it.group || it.code || '';
    const bizBadge = bizLabel ? `<span class="sess-biz-badge">${escapeHtml(bizLabel)}</span>` : '';
    const shortId = (it.id || '').slice(0, 8);
    const cleanTitle = stripRedundantCodePrefix(it.title || `#${shortId}`, it.code, it.group);
    const tip = isCodex ? t('sessions.open_codex') : t('sessions.jump_agy_tip');
    return `
      <button type="button" class="radar-row ${escapeHtml(st)}" data-radar-client="${escapeHtml(it.client)}" data-radar-id="${escapeHtml(it.id)}" data-radar-uri="${escapeHtml(it.open_url || '')}" title="${escapeHtml(tip)}">
        <span class="radar-row-main">
          ${renderRadarGlyph(st, 15, glyphStyle)}
          ${clientBadge}
          ${bizBadge}
          <span class="radar-row-title">${escapeHtml(cleanTitle)}</span>
        </span>
        <span class="radar-row-right">
          <span class="radar-age">${escapeHtml(fmtSessionAge(it.updated_at_ms))}</span>
        </span>
      </button>`;
  }

  function renderLiveRadar(radar, state, handlers = {}) {
    const panel = $('live-radar-strip');
    if (!panel) return;
    if (!radar || radar.ok === false || state.mode === 'pro') {
      panel.classList.add('hidden');
      return;
    }

    const att = radar.attention_items || [];
    const rec = radar.recent_items || [];
    if (!att.length && !rec.length) {
      panel.classList.add('hidden');
      return;
    }
    panel.classList.remove('hidden');

    const wCnt = Number(radar.waiting_count || 0);
    const sCnt = Number(radar.stalled_count || 0);
    const uCnt = Number(radar.unread_count || 0);
    const rCnt = Number(radar.running_count || 0);

    panel.classList.toggle('has-urgent', wCnt > 0 || sCnt > 0);
    panel.classList.toggle('has-unread', uCnt > 0);

    const gStyle = radar.glyph_style || 'waveform';
    const leadState = wCnt > 0 ? 'waiting' : sCnt > 0 ? 'stalled' : uCnt > 0 ? 'unread' : rCnt > 0 ? 'running' : 'idle';
    const leadEl = $('radar-lead-glyph');
    if (leadEl) leadEl.innerHTML = renderRadarGlyph(leadState, 16, gStyle);

    const pills = [];
    if (wCnt > 0) pills.push(`<span class="status-pill warn">${escapeHtml(t('radar.waiting_pill', { count: wCnt }))}</span>`);
    if (sCnt > 0) pills.push(`<span class="status-pill err">${escapeHtml(t('radar.stalled_pill', { count: sCnt }))}</span>`);
    if (uCnt > 0) pills.push(`<span class="status-pill info">${escapeHtml(t('radar.unread_pill', { count: uCnt }))}</span>`);
    if (rCnt > 0) pills.push(`<span class="status-pill ok">${escapeHtml(t('radar.running_pill', { count: rCnt }))}</span>`);
    if (!pills.length) pills.push(`<span class="mono-tag">${escapeHtml(t('radar.calm_pill'))}</span>`);
    const pillsEl = $('radar-summary-pills');
    if (pillsEl) pillsEl.innerHTML = pills.join('');

    const namerBtn = $('btn-radar-namer');
    if (namerBtn) {
      if (!namerBtn.innerHTML) {
        namerBtn.innerHTML = `<svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M10.5 2.5l3 3-8 8-3-3 8-8z"/><path d="M8.5 4.5l3 3"/><path d="M13 1v2M12 2h2M3 1v1.5M2.25 1.75h1.5"/></svg>`;
      }
      const nTip = t('radar.sync_titles_tip');
      namerBtn.title = nTip;
      namerBtn.setAttribute('aria-label', nTip);
    }

    const soundBtn = $('btn-radar-sound');
    if (soundBtn) {
      const soundOn = radar.sound_enabled !== false;
      soundBtn.innerHTML = renderSoundIcon(soundOn);
      soundBtn.classList.toggle('muted', !soundOn);
      const sTip = soundOn ? t('radar.sound_on_tip') : t('radar.sound_off_tip');
      soundBtn.title = sTip;
      soundBtn.setAttribute('aria-label', sTip);
    }

    const recentBtn = $('btn-radar-recent');
    const recentDrawer = $('radar-recent-drawer');
    const showRecent = Boolean(state.radarRecentOpen) || att.length === 0;
    if (recentBtn) {
      recentBtn.classList.toggle('hidden', rec.length === 0 || att.length === 0);
      recentBtn.setAttribute('aria-expanded', String(Boolean(state.radarRecentOpen)));
    }

    const attEl = $('radar-attention-list');
    if (attEl) {
      attEl.innerHTML = att.slice(0, 8).map((it) => renderRadarRowHtml(it, gStyle)).join('');
      attEl.classList.toggle('hidden', att.length === 0);
    }

    if (recentDrawer) {
      const recList = $('radar-recent-list');
      if (showRecent && rec.length > 0 && recList) {
        recentDrawer.classList.remove('hidden');
        recList.innerHTML = rec.slice(0, 6).map((it) => renderRadarRowHtml(it, gStyle)).join('');
      } else {
        recentDrawer.classList.add('hidden');
      }
    }

    panel.querySelectorAll('button.radar-row[data-radar-id]').forEach((btn) => {
      btn.addEventListener('click', (ev) => {
        if (typeof handlers.onJumpRadarItem === 'function') {
          handlers.onJumpRadarItem(
            {
              client: btn.dataset.radarClient || 'agy',
              id: btn.dataset.radarId || '',
              open_url: btn.dataset.radarUri || '',
            },
            ev
          );
        }
      });
    });
  }

  function renderSessionsView(resp, state, handlers = {}) {
    const metaEl = $('sessions-stats-meta');
    const groupsEl = $('sessions-group-pills');
    const listEl = $('sessions-list');
    if (!listEl) return;

    document.querySelectorAll('.sess-filter-btn[data-sclient]').forEach((b) => {
      b.classList.toggle('active', b.dataset.sclient === state.sessionsClient);
    });
    document.querySelectorAll('.sess-filter-btn[data-sdays]').forEach((b) => {
      b.classList.toggle('active', Number(b.dataset.sdays) === Number(state.sessionsDays));
    });
    document.querySelectorAll('.sess-filter-btn[data-ssort]').forEach((b) => {
      b.classList.toggle('active', b.dataset.ssort === state.sessionsSort);
    });
    const clearBtn = $('btn-sessions-clear');
    if (clearBtn) {
      clearBtn.classList.toggle('hidden', !state.sessionsQuery);
    }

    if (!resp) {
      if (metaEl) metaEl.textContent = '…';
      listEl.innerHTML = `<div class="empty-state">${escapeHtml(t('sessions.loading'))}</div>`;
      return;
    }

    if (metaEl) {
      metaEl.textContent = t('sessions.stats_summary', {
        total: resp.total_matches ?? 0,
        agy: resp.agy_matches ?? 0,
        codex: resp.codex_matches ?? 0,
        ms: Number(resp.elapsed_ms ?? 0).toFixed(1),
      });
    }

    if (groupsEl) {
      const facets = resp.groups || [];
      const totalAll = facets.reduce((acc, g) => acc + (g.count || 0), 0);
      const pills = [
        `<button type="button" class="sess-group-pill ${!state.sessionsGroup ? 'active' : ''}" data-sgroup="">${escapeHtml(
          t('sessions.group_all')
        )} <span class="sess-pill-cnt">${totalAll}</span></button>`,
      ];
      for (const g of facets) {
        const isAct = state.sessionsGroup && state.sessionsGroup === g.group;
        pills.push(
          `<button type="button" class="sess-group-pill ${isAct ? 'active' : ''}" data-sgroup="${escapeHtml(
            g.group
          )}">${escapeHtml(g.group)} <span class="sess-pill-cnt">${g.count || 0}</span></button>`
        );
      }
      groupsEl.innerHTML = pills.join('');
      groupsEl.querySelectorAll('.sess-group-pill[data-sgroup]').forEach((btn) => {
        btn.addEventListener('click', () => {
          if (typeof handlers.onSelectGroup === 'function') {
            handlers.onSelectGroup(btn.dataset.sgroup || '');
          }
        });
      });
    }

    const items = resp.sessions || [];
    if (!items.length) {
      listEl.innerHTML = `<div class="empty-state">${escapeHtml(t('sessions.empty'))}</div>`;
      return;
    }

    const q = state.sessionsQuery || '';
    listEl.innerHTML = items
      .map((it) => {
        const isCodex = it.client === 'codex';
        const clientBadge = isCodex
          ? '<span class="sess-client-badge codex">Codex</span>'
          : '<span class="sess-client-badge agy">AGY</span>';
        const bizLabel = it.group || it.code || '';
        const bizBadge = bizLabel
          ? `<span class="sess-biz-badge">${escapeHtml(bizLabel)}</span>`
          : '';
        const gStyle = state.lastRadar?.glyph_style || state.lastSnap?.config?.radar_glyph_style || 'waveform';
        const st = it.live_state || (it.active ? 'running' : 'idle');
        const stateGlyph = renderRadarGlyph(st, 14, gStyle);
        const shortId = (it.id || '').slice(0, 8);
        const createdTxt = t('sessions.created_lbl', { date: fmtMonthDay(it.created_at_ms) });
        const activeTxt = t('sessions.active_lbl', { age: fmtSessionAge(it.updated_at_ms) });

        const snippetHtml =
          it.snippet && it.snippet.trim() && it.snippet.trim() !== it.title.trim()
            ? `<p class="sess-snippet">${highlightTermsHtml(it.snippet, q)}</p>`
            : '';

        const primaryAction = isCodex && it.open_url
          ? `<a href="${escapeHtml(it.open_url)}" class="btn-accent btn-xs sess-open-link" data-open-uri="${escapeHtml(
              it.open_url
            )}" data-short-id="${escapeHtml(shortId)}">${escapeHtml(t('sessions.open_codex'))}</a>`
          : `<button type="button" class="btn-ghost btn-xs" data-inspect-agy="${escapeHtml(it.id)}">${escapeHtml(
              t('sessions.inspect_agy')
            )}</button><button type="button" class="btn-accent btn-xs" data-jump-agy="${escapeHtml(it.id)}" title="${escapeHtml(
              t('sessions.jump_agy_tip')
            )}">${escapeHtml(t('sessions.jump_agy'))}</button>`;

        return `
          <article class="sess-card">
            <div class="sess-card-top">
              <div class="sess-badges">
                ${stateGlyph}${clientBadge}${bizBadge}
                <span class="sess-id-tag">#${escapeHtml(shortId)}</span>
              </div>
              <div class="sess-actions">
                <button type="button" class="btn-ghost btn-xs" data-copy-sid="${escapeHtml(it.id)}">${escapeHtml(
          t('sessions.copy_id')
        )}</button>
                ${primaryAction}
              </div>
            </div>
            <h3 class="sess-title">${highlightTermsHtml(it.title || `#${shortId}`, q)}</h3>
            ${snippetHtml}
            <div class="sess-meta-row">
              <span>${escapeHtml(createdTxt)}</span>
              <span class="sess-meta-sep">·</span>
              <span>${escapeHtml(activeTxt)}</span>
              ${it.workspace ? `<span class="sess-meta-sep">·</span><span class="sess-ws-lbl">${escapeHtml(it.workspace)}</span>` : ''}
            </div>
          </article>`;
      })
      .join('');

    listEl.querySelectorAll('button[data-copy-sid]').forEach((btn) => {
      btn.addEventListener('click', () => {
        if (typeof handlers.onCopyId === 'function') handlers.onCopyId(btn.dataset.copySid);
      });
    });
    listEl.querySelectorAll('button[data-inspect-agy]').forEach((btn) => {
      btn.addEventListener('click', () => {
        if (typeof handlers.onInspectAgy === 'function') handlers.onInspectAgy(btn.dataset.inspectAgy);
      });
    });
    listEl.querySelectorAll('button[data-jump-agy]').forEach((btn) => {
      btn.addEventListener('click', (ev) => {
        if (typeof handlers.onJumpAgy === 'function') handlers.onJumpAgy(btn.dataset.jumpAgy, ev);
      });
    });
    listEl.querySelectorAll('a.sess-open-link[data-open-uri]').forEach((link) => {
      link.addEventListener('click', (ev) => {
        if (typeof handlers.onOpenCodex === 'function') {
          ev.preventDefault();
          handlers.onOpenCodex(link.dataset.openUri, link.dataset.shortId || '', link.closest('.sess-card')?.querySelector('[data-copy-sid]')?.dataset.copySid || '');
        }
      });
    });
  }

  root.render = {
    renderWarmthMeter,
    renderSnapshot,
    renderUsageReport,
    renderAuditPanel,
    renderSessionsView,
    renderLiveRadar,
    renderRadarGlyph,
  };
})();
