(() => {
  const root = (window.HyperDeck = window.HyperDeck || {});
  const { $, escapeHtml, fmtCurrency, fmtSec, fmtTokens, isDark, palette, renderBreakdown, t } = root.api;

  const charts = { hero: null, waterfall: null, ctxPro: null, usage: null };

  function ensureChart(key, elId) {
    if (!window.echarts) return null;
    const el = $(elId);
    if (!el || el.offsetWidth === 0 || el.offsetHeight === 0) return null;
    if (!charts[key]) {
      charts[key] = window.echarts.init(el, null, { renderer: 'canvas' });
      if (typeof ResizeObserver !== 'undefined') {
        new ResizeObserver(() => {
          if (el.offsetWidth === 0 || el.offsetHeight === 0) return;
          try { charts[key]?.resize(); } catch { /* ignore */ }
        }).observe(el);
      }
    }
    return charts[key];
  }

  function baseTooltip(pal, extra = {}) {
    return {
      trigger: 'axis',
      backgroundColor: pal.tooltipBg,
      borderColor: pal.tooltipBorder,
      textStyle: { color: pal.fg, fontSize: 11 },
      ...extra,
    };
  }

  function baseCategoryAxis(pal, data, boundaryGap = true) {
    return {
      type: 'category',
      data,
      boundaryGap,
      axisLine: { show: false },
      axisTick: { show: false },
      axisLabel: { color: pal.muted, fontSize: 9.5, interval: 'auto' },
    };
  }

  function resizeAllCharts() {
    Object.values(charts).forEach((c) => {
      try { c && c.resize(); } catch { /* ignore */ }
    });
  }

  function renderHeroChart(snap, heroTab) {
    const chart = ensureChart('hero', 'chart-hero');
    const pal = palette();
    const turns = (snap.benchmark?.turns || []).slice(-26);
    const pulse = snap.pulse || {};
    const eng = snap.engine || {};
    const cfg = snap.config || {};
    const redTok = pulse.effective_red_tokens || cfg.watermark_red_tokens || 280000;
    const yellowTok = pulse.effective_yellow_tokens || cfg.watermark_yellow_tokens || 180000;
    const redK = Math.round(redTok / 1000);
    const yellowK = Math.round(yellowTok / 1000);
    const cliffK = Math.round((pulse.pricing_cliff_tokens || 200000) / 1000);
    const modelShort = pulse.model ? pulse.model.replace(/-xhigh$|-high$|-preview$/, '') : '';
    const labels = turns.map((x) => `T${x.turn_index}`);

    const totalTok = pulse.total_tokens || 0;
    const cachedTok = pulse.cache_read_tokens || 0;
    const freshTok = Math.max(0, totalTok - cachedTok);
    const hitPct = Math.round((pulse.cache_hit_ratio ?? snap.benchmark?.cache_hit_ratio ?? 0) * 100);

    if (heroTab === 'context') {
      $('hero-chart-title').textContent = t('hero.tab_context');
      $('hero-chart-meta').textContent = `${(totalTok / 1000).toFixed(1)}k / ${redK}k`;
      $('hero-chart-meta').title = t('hero.watermark_basis', { yellow: yellowK, red: redK, cliff: cliffK });
      const totalsK = turns.map((x) => Math.round(((x.input_tokens + x.cache_read_tokens) / 1000) * 10) / 10);
      const maxY = Math.max(redK + 20, totalsK.length ? Math.max(...totalsK) + 15 : 0, 60);

      renderBreakdown($('hero-breakdown'), [
        { color: pal.accent, label: t('hero.row_context'), value: `${(totalTok / 1000).toFixed(1)}k / ${redK}k` },
        { color: pal.ok, label: t('hero.row_cache'), value: `${hitPct}% · ${(freshTok / 1000).toFixed(1)}k fresh` },
        { color: pal.indigo, label: t('hero.row_watermark'), value: modelShort ? `${modelShort} · ${yellowK}k/${redK}k` : `${yellowK}k / ${redK}k` },
      ]);

      if (!chart) return;
      if (!turns.length) { chart.clear(); return; }
      chart.setOption({
        animation: false,
        grid: { top: 14, right: 10, bottom: 20, left: 36 },
        tooltip: baseTooltip(pal, {
          formatter: (items) => {
            const tr = turns[items?.[0]?.dataIndex ?? 0];
            if (!tr) return '';
            const tot = tr.input_tokens + tr.cache_read_tokens;
            const hit = tot > 0 ? Math.round((tr.cache_read_tokens / tot) * 100) : 0;
            return `<b>T${tr.turn_index}</b><br/>${t('pulse.context_label')}: <b>${(tot / 1000).toFixed(1)}k</b><br/>${t('kpi.cache_hit')}: <b>${hit}%</b>`;
          },
        }),
        xAxis: baseCategoryAxis(pal, labels, false),
        yAxis: {
          type: 'value',
          min: 0,
          max: maxY,
          splitNumber: 3,
          splitLine: { lineStyle: { color: pal.grid } },
          axisLabel: { color: pal.muted, fontSize: 9.5, formatter: '{value}k' },
        },
        series: [{
          type: 'line',
          smooth: 0.25,
          showSymbol: false,
          data: totalsK,
          lineStyle: { width: 2, color: pal.accent },
          itemStyle: { color: pal.accent },
          areaStyle: {
            color: {
              type: 'linear', x: 0, y: 0, x2: 0, y2: 1,
              colorStops: [
                { offset: 0, color: isDark() ? 'rgba(56,189,248,0.28)' : 'rgba(2,132,199,0.20)' },
                { offset: 1, color: 'rgba(56,189,248,0.01)' },
              ],
            },
          },
          markLine: {
            silent: true,
            symbol: 'none',
            label: { show: false },
            data: [
              { yAxis: yellowK, lineStyle: { type: 'dashed', color: pal.warn, width: 1 } },
              { yAxis: redK, lineStyle: { type: 'dashed', color: pal.err, width: 1 } },
            ],
          },
        }],
      }, true);
      return;
    }

    if (heroTab === 'latency') {
      $('hero-chart-title').textContent = t('hero.tab_latency');
      $('hero-chart-meta').textContent = snap.benchmark?.median_turn_ms ? fmtSec(snap.benchmark.median_turn_ms) : '—';
      const llmSec = turns.map((x) => Math.round(((x.llm_inference_ms || 0) / 1000) * 100) / 100);
      const toolSec = turns.map((x) => Math.round(((x.tool_exec_ms || 0) / 1000) * 100) / 100);
      const avgLlm = turns.length ? turns.reduce((a, b) => a + (b.llm_inference_ms || 0), 0) / turns.length : 0;
      const avgTool = turns.length ? turns.reduce((a, b) => a + (b.tool_exec_ms || 0), 0) / turns.length : 0;

      renderBreakdown($('hero-breakdown'), [
        { color: pal.indigo, label: t('hero.row_llm_avg'), value: fmtSec(avgLlm) },
        { color: pal.warn, label: t('hero.row_tool_avg'), value: fmtSec(avgTool) },
        { color: pal.accent, label: t('kpi.paired_tps'), value: snap.benchmark?.effective_tps ? `${snap.benchmark.effective_tps.toFixed(1)} tok/s` : '—' },
      ]);

      if (!chart) return;
      if (!turns.length) { chart.clear(); return; }
      chart.setOption({
        animation: false,
        grid: { top: 14, right: 10, bottom: 20, left: 36 },
        tooltip: baseTooltip(pal, {
          axisPointer: { type: 'shadow' },
          formatter: (items) => {
            const tr = turns[items?.[0]?.dataIndex ?? 0];
            if (!tr) return '';
            return `<b>T${tr.turn_index}</b><br/>${t('waterfall.tooltip_llm')}: <b>${fmtSec(tr.llm_inference_ms)}</b><br/>${t('waterfall.tooltip_tool')}: <b>${fmtSec(tr.tool_exec_ms)}</b>`;
          },
        }),
        xAxis: baseCategoryAxis(pal, labels, true),
        yAxis: {
          type: 'value',
          splitNumber: 3,
          splitLine: { lineStyle: { color: pal.grid } },
          axisLabel: { color: pal.muted, fontSize: 9.5, formatter: '{value}s' },
        },
        series: [
          { name: t('waterfall.llm'), type: 'bar', stack: 't', barMaxWidth: 12, data: llmSec, itemStyle: { color: pal.indigo, borderRadius: [0, 0, 2, 2] } },
          { name: t('waterfall.std'), type: 'bar', stack: 't', barMaxWidth: 12, data: toolSec, itemStyle: { color: pal.warn, borderRadius: [2, 2, 0, 0] } },
        ],
      }, true);
      return;
    }

    if (heroTab === 'cache') {
      $('hero-chart-title').textContent = t('hero.tab_cache');
      $('hero-chart-meta').textContent = `${hitPct}%`;
      const hitSeries = turns.map((x) => {
        const tot = x.input_tokens + x.cache_read_tokens;
        return tot > 0 ? Math.round((x.cache_read_tokens / tot) * 100) : 0;
      });

      const cacheBreaks = snap.benchmark?.cache_break_turns || 0;
      const cacheRows = [
        { color: pal.ok, label: t('pulse.series_cached'), value: `${(cachedTok / 1000).toFixed(1)}k` },
        { color: pal.accent, label: t('pulse.series_fresh'), value: `${(freshTok / 1000).toFixed(1)}k` },
        cacheBreaks > 0
          ? { color: pal.err, label: t('hero.row_cache_breaks'), value: String(cacheBreaks) }
          : { color: pal.indigo, label: t('engine.ast_cache'), value: `${eng.ast_cache_entries || 0} files` },
      ];
      renderBreakdown($('hero-breakdown'), cacheRows);

      if (!chart) return;
      if (!turns.length) { chart.clear(); return; }
      chart.setOption({
        animation: false,
        grid: { top: 14, right: 10, bottom: 20, left: 36 },
        tooltip: baseTooltip(pal, {
          formatter: (items) => {
            const i = items?.[0]?.dataIndex ?? 0;
            const tr = turns[i];
            if (!tr) return '';
            return `<b>T${tr.turn_index}</b><br/>${t('kpi.cache_hit')}: <b>${hitSeries[i]}%</b><br/>${t('pulse.series_cached')}: <b>${(tr.cache_read_tokens / 1000).toFixed(1)}k</b>`;
          },
        }),
        xAxis: baseCategoryAxis(pal, labels, false),
        yAxis: {
          type: 'value',
          min: 0,
          max: 100,
          splitNumber: 2,
          splitLine: { lineStyle: { color: pal.grid } },
          axisLabel: { color: pal.muted, fontSize: 9.5, formatter: '{value}%' },
        },
        series: [{
          type: 'line',
          smooth: 0.25,
          showSymbol: false,
          data: hitSeries,
          lineStyle: { width: 2, color: pal.ok },
          itemStyle: { color: pal.ok },
          areaStyle: {
            color: {
              type: 'linear', x: 0, y: 0, x2: 0, y2: 1,
              colorStops: [
                { offset: 0, color: isDark() ? 'rgba(52,211,153,0.28)' : 'rgba(16,185,129,0.20)' },
                { offset: 1, color: 'rgba(16,185,129,0.01)' },
              ],
            },
          },
        }],
      }, true);
      return;
    }

    // heroTab === 'saved'
    const turnsSaved = eng.total_turns_saved || 0;
    const hyperCalls = snap.benchmark?.hyper_tool_calls || 0;
    const totalCalls = snap.benchmark?.total_tool_calls || 0;
    const rawRatio = snap.benchmark?.hyper_adoption_ratio ?? (totalCalls > 0 ? hyperCalls / totalCalls : 0);
    const pctNum = rawRatio * 100;
    const pctStr = pctNum > 0 && pctNum < 1 ? `${pctNum.toFixed(1)}%` : `${Math.round(pctNum)}%`;
    const astHits = eng.ast_cache_hits || 0;
    const astTotal = astHits + (eng.ast_cache_misses || 0);
    const packHits = eng.pack_cache_hits || 0;

    $('hero-chart-title').textContent = t('hero.tab_saved');
    $('hero-chart-meta').textContent = t('hero.turns_unit', { count: turnsSaved });
    const hyperCounts = turns.map((x) => (x.used_hyper_tools ? Math.max(1, x.tool_calls || 1) : 0));
    const stdCounts = turns.map((x) => (x.used_hyper_tools ? 0 : x.tool_calls || 0));

    renderBreakdown($('hero-breakdown'), [
      { color: pal.accent, label: t('hero.row_hyper_share'), value: totalCalls > 0 ? `${hyperCalls}/${totalCalls} (${pctStr})` : '0%' },
      { color: pal.ok, label: t('hero.row_batched'), value: t('hero.turns_unit', { count: turnsSaved }) },
      { color: pal.indigo, label: t('hero.row_mem_cache'), value: `AST ${astHits}/${astTotal} · Pack ${packHits}` },
    ]);

    if (!chart) return;
    if (!turns.length) { chart.clear(); return; }
    chart.setOption({
      animation: false,
      grid: { top: 14, right: 10, bottom: 20, left: 32 },
      tooltip: baseTooltip(pal, { axisPointer: { type: 'shadow' } }),
      xAxis: baseCategoryAxis(pal, labels, true),
      yAxis: {
        type: 'value',
        minInterval: 1,
        splitNumber: 3,
        splitLine: { lineStyle: { color: pal.grid } },
        axisLabel: { color: pal.muted, fontSize: 9.5 },
      },
      series: [
        { name: t('waterfall.hyper'), type: 'bar', stack: 'calls', barMaxWidth: 12, data: hyperCounts, itemStyle: { color: pal.accent } },
        { name: t('waterfall.std'), type: 'bar', stack: 'calls', barMaxWidth: 12, data: stdCounts, itemStyle: { color: pal.warn, borderRadius: [2, 2, 0, 0] } },
      ],
    }, true);
  }

  function renderWaterfallChart(bench) {
    const chart = ensureChart('waterfall', 'chart-waterfall');
    const pal = palette();
    const turns = (bench?.turns || []).slice(-14);

    const sumLlm = turns.reduce((a, b) => a + (b.llm_inference_ms || 0), 0);
    const sumHyper = turns.reduce((a, b) => a + (b.used_hyper_tools ? b.tool_exec_ms || 0 : 0), 0);
    const sumStd = turns.reduce((a, b) => a + (b.used_hyper_tools ? 0 : b.tool_exec_ms || 0), 0);
    const totalMs = Math.max(1, sumLlm + sumHyper + sumStd);

    renderBreakdown($('wf-breakdown'), [
      { color: pal.indigo, label: t('waterfall.llm'), value: `${fmtSec(sumLlm)} (${Math.round((sumLlm / totalMs) * 100)}%)` },
      { color: pal.accent, label: t('waterfall.hyper'), value: `${fmtSec(sumHyper)} (${Math.round((sumHyper / totalMs) * 100)}%)` },
      { color: pal.warn, label: t('waterfall.std'), value: `${fmtSec(sumStd)} (${Math.round((sumStd / totalMs) * 100)}%)` },
    ]);

    if (!chart) return;
    if (!turns.length) {
      chart.clear();
      return;
    }

    const cats = turns.map((x) => `T${String(x.turn_index).padStart(2, '0')}`);
    const llmData = turns.map((x) => Math.round(((x.llm_inference_ms || 0) / 1000) * 100) / 100);
    const hyperData = turns.map((x) => (x.used_hyper_tools ? Math.round(((x.tool_exec_ms || 0) / 1000) * 100) / 100 : 0));
    const stdData = turns.map((x) => (x.used_hyper_tools ? 0 : Math.round(((x.tool_exec_ms || 0) / 1000) * 100) / 100));

    chart.setOption({
      animation: false,
      grid: { top: 8, right: 16, bottom: 24, left: 38 },
      tooltip: baseTooltip(pal, {
        axisPointer: { type: 'shadow' },
        formatter: (items) => {
          const tr = turns[items?.[0]?.dataIndex ?? 0];
          if (!tr) return '';
          const tools = (tr.tool_names || []).slice(0, 4).join(', ');
          return [
            `<b>${escapeHtml(t('waterfall.tooltip_turn', { turn: tr.turn_index }))}</b>`,
            `${escapeHtml(t('waterfall.tooltip_llm'))}: <b>${fmtSec(tr.llm_inference_ms)}</b>`,
            `${escapeHtml(t('waterfall.tooltip_tool'))}: <b>${fmtSec(tr.tool_exec_ms)}</b> (${tr.tool_calls})`,
            `${escapeHtml(t('waterfall.tooltip_out'))}: <b>${tr.output_tokens} tok</b>`,
            tools ? `<span style="opacity:.7">${escapeHtml(tools)}</span>` : '',
          ].filter(Boolean).join('<br/>');
        },
      }),
      xAxis: {
        type: 'value',
        splitLine: { lineStyle: { color: pal.grid } },
        axisLabel: { color: pal.muted, fontSize: 10, formatter: '{value}s' },
      },
      yAxis: {
        type: 'category',
        data: cats,
        inverse: true,
        axisLine: { show: false },
        axisTick: { show: false },
        axisLabel: { color: pal.muted, fontSize: 10.5, fontFamily: 'ui-monospace, SF Mono, monospace' },
      },
      series: [
        { name: t('waterfall.llm'), type: 'bar', stack: 'turn', barMaxWidth: 14, itemStyle: { color: pal.indigo, borderRadius: [2, 0, 0, 2] }, data: llmData },
        { name: t('waterfall.hyper'), type: 'bar', stack: 'turn', barMaxWidth: 14, itemStyle: { color: pal.accent, borderRadius: [0, 2, 2, 0] }, data: hyperData },
        { name: t('waterfall.std'), type: 'bar', stack: 'turn', barMaxWidth: 14, itemStyle: { color: pal.warn, borderRadius: [0, 2, 2, 0] }, data: stdData },
      ],
    }, true);
  }

  function renderFullContextChart(snap) {
    const chart = ensureChart('ctxPro', 'chart-ctx-pro');
    const pal = palette();
    const turns = (snap.benchmark?.turns || []).slice(-32);
    const pulse = snap.pulse || {};
    const redK = Math.round((pulse.effective_red_tokens || snap.config?.watermark_red_tokens || 280000) / 1000);
    const yellowK = Math.round((pulse.effective_yellow_tokens || snap.config?.watermark_yellow_tokens || 180000) / 1000);

    const cachedTok = pulse.cache_read_tokens || 0;
    const totalTok = pulse.total_tokens || 0;
    const freshTok = Math.max(0, totalTok - cachedTok);
    const peakK = turns.length ? Math.max(...turns.map((x) => (x.input_tokens + x.cache_read_tokens) / 1000)) : 0;

    renderBreakdown($('ctx-breakdown'), [
      { color: pal.accent, label: t('pulse.series_cached'), value: `${(cachedTok / 1000).toFixed(1)}k` },
      { color: pal.indigo, label: t('pulse.series_fresh'), value: `${(freshTok / 1000).toFixed(1)}k` },
      { color: pal.warn, label: t('hero.row_peak_ctx'), value: `${peakK.toFixed(1)}k / ${redK}k` },
    ]);

    if (!chart) return;
    if (!turns.length) {
      chart.clear();
      return;
    }

    const cats = turns.map((x) => `T${x.turn_index}`);
    const cachedK = turns.map((x) => Math.round((x.cache_read_tokens / 1000) * 10) / 10);
    const freshK = turns.map((x) => Math.round((x.input_tokens / 1000) * 10) / 10);
    const hitPct = turns.map((x) => {
      const tot = x.input_tokens + x.cache_read_tokens;
      return tot > 0 ? Math.round((x.cache_read_tokens / tot) * 100) : 0;
    });
    const maxK = Math.max(redK + 40, Math.ceil(peakK) + 20, 60);

    chart.setOption({
      animation: false,
      grid: { top: 28, right: 36, bottom: 24, left: 38 },
      legend: { top: 0, left: 0, itemWidth: 10, itemHeight: 8, textStyle: { color: pal.muted, fontSize: 10.5 } },
      tooltip: baseTooltip(pal),
      xAxis: baseCategoryAxis(pal, cats, true),
      yAxis: [
        { type: 'value', min: 0, max: maxK, splitLine: { lineStyle: { color: pal.grid } }, axisLabel: { color: pal.muted, fontSize: 9.5, formatter: '{value}k' } },
        { type: 'value', min: 0, max: 100, splitLine: { show: false }, axisLabel: { color: pal.ok, fontSize: 9.5, formatter: '{value}%' } },
      ],
      series: [
        {
          name: t('pulse.series_cached'),
          type: 'line',
          stack: 'tok',
          smooth: 0.2,
          showSymbol: false,
          itemStyle: { color: pal.accent },
          lineStyle: { width: 1.5, color: pal.accent },
          areaStyle: { color: isDark() ? 'rgba(56,189,248,0.22)' : 'rgba(2,132,199,0.16)' },
          data: cachedK,
        },
        {
          name: t('pulse.series_fresh'),
          type: 'line',
          stack: 'tok',
          smooth: 0.2,
          showSymbol: false,
          itemStyle: { color: pal.indigo },
          lineStyle: { width: 1.5, color: pal.indigo },
          areaStyle: { color: isDark() ? 'rgba(129,140,248,0.22)' : 'rgba(99,102,241,0.16)' },
          data: freshK,
          markLine: {
            silent: true,
            symbol: 'none',
            label: { color: pal.muted, fontSize: 9, position: 'insideStartTop', formatter: '{b}' },
            data: [
              { yAxis: yellowK, name: `${yellowK}k`, lineStyle: { type: 'dashed', color: pal.warn, width: 1 } },
              { yAxis: redK, name: `${redK}k`, lineStyle: { type: 'dashed', color: pal.err, width: 1 } },
            ],
          },
        },
        {
          name: t('pulse.series_hit_rate'),
          type: 'line',
          yAxisIndex: 1,
          smooth: 0.25,
          showSymbol: false,
          itemStyle: { color: pal.ok },
          lineStyle: { width: 2, color: pal.ok },
          data: hitPct,
        },
      ],
    }, true);
  }

  function renderUsageChart(report, metric = 'cost') {
    const chart = ensureChart('usage', 'chart-usage');
    const pal = palette();
    const series = report?.series || [];
    if (!chart) return;
    if (!series.length) {
      chart.clear();
      return;
    }

    const currency = report.currency || 'USD';
    const labels = series.map((pt) => pt.label);
    const isTokens = metric === 'tokens';
    const values = series.map((pt) =>
      isTokens
        ? Math.round((pt.total_tokens / 1_000_000) * 1000) / 1000
        : Math.round(pt.estimated_cost * 100) / 100
    );

    const yFormatter = (v) =>
      isTokens ? `${v}M` : fmtCurrency(v, currency);

    chart.setOption({
      animation: false,
      grid: { top: 14, right: 10, bottom: 22, left: isTokens ? 42 : 48 },
      tooltip: baseTooltip(pal, {
        axisPointer: { type: 'shadow' },
        formatter: (items) => {
          const pt = series[items?.[0]?.dataIndex ?? 0];
          if (!pt) return '';
          const denom = pt.input_tokens + pt.cache_read_tokens;
          const hit = denom > 0 ? Math.round((pt.cache_read_tokens / denom) * 100) : 0;
          return [
            `<b>${escapeHtml(pt.iso_key)}</b>`,
            `${escapeHtml(t('usage.kpi_tokens'))}: <b>${fmtTokens(pt.total_tokens)}</b> (${hit}% cache)`,
            `${escapeHtml(t('usage.kpi_cost'))}: <b>${escapeHtml(fmtCurrency(pt.estimated_cost, currency))}</b>`,
            `${escapeHtml(t('usage.kpi_saved'))}: <b>${escapeHtml(fmtCurrency(pt.cache_saved, currency))}</b>`,
          ].join('<br/>');
        },
      }),
      xAxis: baseCategoryAxis(pal, labels, true),
      yAxis: {
        type: 'value',
        splitNumber: 3,
        splitLine: { lineStyle: { color: pal.grid } },
        axisLabel: { color: pal.muted, fontSize: 9.5, formatter: yFormatter },
      },
      series: [
        {
          name: isTokens ? t('usage.metric_tokens') : t('usage.metric_cost'),
          type: 'bar',
          barMaxWidth: series.length > 35 ? 6 : 12,
          data: values,
          itemStyle: {
            color: isTokens ? pal.accent : pal.ok,
            borderRadius: [2, 2, 0, 0],
          },
        },
      ],
    }, true);
  }

  root.charts = {
    resizeAllCharts,
    renderHeroChart,
    renderWaterfallChart,
    renderFullContextChart,
    renderUsageChart,
  };
})();
