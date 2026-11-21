/**
 * AGY Hyper Deck — Antigravity Native Sidecar + Standalone HTTP Bridge.
 * Zero npm dependencies. Connects directly to the Rust daemon over Unix Domain Socket (<0.5ms)
 * and gracefully falls back to `agy-hyper snapshot` CLI if the daemon is cold.
 */
import { execFile, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, unlinkSync, writeFileSync } from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import { homedir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const LOCALE_CODES = ['en', 'zh-CN', 'ja'];
const STATIC_FILES = {
  '/styles.css': ['styles.css', 'text/css; charset=utf-8'],
  '/app.js': ['app.js', 'application/javascript; charset=utf-8'],
  '/modules/api.js': ['modules/api.js', 'application/javascript; charset=utf-8'],
  '/modules/charts.js': ['modules/charts.js', 'application/javascript; charset=utf-8'],
  '/modules/render.js': ['modules/render.js', 'application/javascript; charset=utf-8'],
  '/echarts-core.min.js': ['vendor/echarts-core.min.js', 'application/javascript; charset=utf-8'],
  '/i18next.min.js': ['vendor/i18next.min.js', 'application/javascript; charset=utf-8'],
  '/vendor/echarts-core.min.js': ['vendor/echarts-core.min.js', 'application/javascript; charset=utf-8'],
  '/vendor/i18next.min.js': ['vendor/i18next.min.js', 'application/javascript; charset=utf-8'],
  '/locales/en.json': ['locales/en.json', 'application/json; charset=utf-8'],
  '/locales/zh-CN.json': ['locales/zh-CN.json', 'application/json; charset=utf-8'],
  '/locales/ja.json': ['locales/ja.json', 'application/json; charset=utf-8'],
};
const NO_CACHE_HEADERS = { 'Cache-Control': 'no-store' };

function resolveDataDir() {
  if (process.env.AGY_HYPER_DATA_DIR) return process.env.AGY_HYPER_DATA_DIR;
  // Canonical single state directory shared across Antigravity hooks, MCP, Sidecar & CLI.
  return join(homedir(), '.gemini', 'antigravity', 'agy-hyper');
}

function resolveRunDir() {
  return join(resolveDataDir(), 'run');
}

function resolveSocketPath() {
  return join(resolveRunDir(), 'agy-hyper-v1.sock');
}

function writeSidecarPid() {
  try {
    const runDir = resolveRunDir();
    mkdirSync(runDir, { recursive: true });
    const pidFile = join(runDir, 'sidecar.pid');
    writeFileSync(pidFile, `${process.pid}\n`, 'utf8');
    const cleanup = () => {
      try {
        if (existsSync(pidFile) && readFileSync(pidFile, 'utf8').trim() === String(process.pid)) {
          unlinkSync(pidFile);
        }
      } catch {
        // ignore cleanup race
      }
    };
    process.on('exit', cleanup);
    process.on('SIGTERM', () => { cleanup(); process.exit(0); });
    process.on('SIGINT', () => { cleanup(); process.exit(0); });
  } catch {
    // non-fatal
  }
}

/**
 * Render index.html with styles.css, i18n locale bundles, and modular app scripts inlined on GET /.
 * Guarantees zero missing assets, zero CORS module-graph failures inside sandboxed gateways, and instant boot.
 */
const APP_SCRIPTS = [
  'modules/api.js',
  'modules/charts.js',
  'modules/render.js',
  'app.js',
];

function renderIndexHtml() {
  let html = readFileSync(join(__dirname, 'index.html'), 'utf8');
  try {
    const css = readFileSync(join(__dirname, 'styles.css'), 'utf8').replace(/<\/style/gi, '<\\/style');
    html = html.replace(
      '<link rel="stylesheet" href="/styles.css">',
      () => `<style id="hyper-inline-styles">\n${css}\n</style>`
    );
  } catch {
    // fallback to external stylesheet
  }

  try {
    const locales = {};
    for (const code of LOCALE_CODES) {
      const p = join(__dirname, 'locales', `${code}.json`);
      if (existsSync(p)) {
        locales[code] = JSON.parse(readFileSync(p, 'utf8'));
      }
    }
    const safeJson = JSON.stringify(locales).replace(/<\//g, '<\\/');
    html = html.replace(
      '<!-- INLINE_LOCALES -->',
      () => `<script>window.__HYPER_LOCALES__ = ${safeJson};</script>`
    );
  } catch {
    // fallback to runtime fetch
  }

  try {
    const inlined = APP_SCRIPTS.map((rel) => {
      const src = readFileSync(join(__dirname, rel), 'utf8').replace(/<\/script/gi, '<\\/script');
      return `<script data-module="${rel}">\n${src}\n</script>`;
    }).join('\n');
    html = html.replace('<!-- INLINE_APP_SCRIPTS -->', () => inlined);
  } catch {
    const tags = APP_SCRIPTS.map((rel) => `<script src="/${rel}"></script>`).join('\n');
    html = html.replace('<!-- INLINE_APP_SCRIPTS -->', () => tags);
  }
  return html;
}

function resolveHyperBin() {
  const candidates = [
    process.env.AGY_HYPER_BIN,
    join(__dirname, '..', '..', 'bin', process.platform === 'win32' ? 'agy-hyper.exe' : 'agy-hyper'),
    join(__dirname, '..', '..', '..', 'target', 'release', process.platform === 'win32' ? 'agy-hyper.exe' : 'agy-hyper'),
    join(__dirname, '..', '..', '..', 'target', 'debug', process.platform === 'win32' ? 'agy-hyper.exe' : 'agy-hyper'),
    join(homedir(), '.local', 'bin', 'agy-hyper'),
  ].filter(Boolean);
  for (const c of candidates) {
    if (existsSync(c)) return c;
  }
  return 'agy-hyper';
}

function callDaemon(reqObj, timeoutMs = 1500) {
  return new Promise((resolve, reject) => {
    let client;
    let authPrefix = '';
    if (process.platform === 'win32') {
      const runDir = resolveRunDir();
      const addrFile = join(runDir, 'agy-hyper-v1.addr');
      const tokenFile = join(runDir, 'agy-hyper-v1.token');
      if (!existsSync(addrFile) || !existsSync(tokenFile)) {
        return reject(new Error('daemon loopback endpoint absent'));
      }
      const [host, portStr] = readFileSync(addrFile, 'utf8').trim().split(':');
      const port = Number(portStr);
      if (!host || !Number.isFinite(port)) {
        return reject(new Error('invalid daemon loopback address'));
      }
      authPrefix = `${readFileSync(tokenFile, 'utf8').trim()}\n`;
      client = net.createConnection({ host, port });
    } else {
      const sock = resolveSocketPath();
      if (!existsSync(sock)) {
        return reject(new Error('daemon socket absent'));
      }
      client = net.createConnection(sock);
    }

    let buf = '';
    let settled = false;
    const timer = setTimeout(() => {
      if (!settled) {
        settled = true;
        client.destroy();
        reject(new Error('daemon rpc timeout'));
      }
    }, timeoutMs);

    client.on('connect', () => {
      client.write(`${authPrefix}${JSON.stringify(reqObj)}\n`);
    });
    client.on('data', (chunk) => {
      buf += chunk.toString('utf8');
      const nl = buf.indexOf('\n');
      if (nl !== -1 && !settled) {
        settled = true;
        clearTimeout(timer);
        client.end();
        try {
          resolve(JSON.parse(buf.slice(0, nl)));
        } catch (err) {
          reject(err);
        }
      }
    });
    client.on('error', (err) => {
      if (!settled) {
        settled = true;
        clearTimeout(timer);
        reject(err);
      }
    });
  });
}

function runCliJson(args) {
  return new Promise((resolve, reject) => {
    execFile(resolveHyperBin(), args, { timeout: 4000, maxBuffer: 4 * 1024 * 1024 }, (err, stdout, stderr) => {
      if (err) return reject(new Error(stderr || err.message));
      try {
        resolve(JSON.parse(stdout));
      } catch (e) {
        reject(e);
      }
    });
  });
}

async function getSnapshot(conversationId, transcriptPath) {
  const t0 = performance.now();
  try {
    const resp = await callDaemon({
      op: 'snapshot',
      conversation_id: conversationId || null,
      transcript_path: transcriptPath || null,
    });
    if (resp && resp.status === 'ok') {
      return {
        ...resp.data,
        transport: 'uds-daemon',
        rpc_rtt_ms: Math.round((performance.now() - t0) * 100) / 100,
      };
    }
  } catch {
    // Fall back to direct CLI snapshot (also auto-spawns daemon if available)
  }
  const args = ['snapshot'];
  if (conversationId) args.push('--conversation-id', conversationId);
  if (transcriptPath) args.push('--transcript-path', transcriptPath);
  const data = await runCliJson(args);
  return {
    ...data,
    transport: 'cli-fallback',
    rpc_rtt_ms: Math.round((performance.now() - t0) * 100) / 100,
  };
}

async function setProfile(profile) {
  try {
    const resp = await callDaemon({ op: 'set_profile', profile });
    if (resp?.status === 'ok') return { ok: true, config: resp.data };
    if (resp?.status === 'err') return { ok: false, error: resp.message };
  } catch {
    // fallback CLI
  }
  await new Promise((res, rej) => {
    execFile(resolveHyperBin(), ['config', 'profile', String(profile)], { timeout: 3000 }, (err, _o, e) => {
      if (err) rej(new Error(e || err.message));
      else res();
    });
  });
  return { ok: true };
}

async function setToggle(key, value) {
  try {
    const resp = await callDaemon({ op: 'set_toggle', key, value: Boolean(value) });
    if (resp?.status === 'ok') return { ok: true, config: resp.data };
    if (resp?.status === 'err') return { ok: false, error: resp.message };
  } catch {
    // fallback CLI
  }
  await new Promise((res, rej) => {
    execFile(resolveHyperBin(), ['config', 'toggle', String(key), value ? 'true' : 'false'], { timeout: 3000 }, (err, _o, e) => {
      if (err) rej(new Error(e || err.message));
      else res();
    });
  });
  return { ok: true };
}

async function rollbackTx(txId, force = false) {
  try {
    const resp = await callDaemon({ op: 'rollback_tx', tx_id: txId, force: Boolean(force) }, 4000);
    if (resp?.status === 'ok') return { ok: true, result: resp.data };
    if (resp?.status === 'err') return { ok: false, error: resp.message };
  } catch {
    // fallback CLI
  }
  const args = ['tx', 'rollback', String(txId)];
  if (force) args.push('--force');
  const result = await runCliJson(args);
  return { ok: true, result };
}

const SUPPORTED_FX_CODES = ['CNY', 'JPY', 'EUR', 'GBP', 'HKD', 'SGD', 'KRW'];

async function getUsageSummary(windowName = '7d', currency = '', tzOffsetMins = 0, forceSync = false) {
  const win = String(windowName || '7d');
  const tz = Number.isFinite(Number(tzOffsetMins)) ? Math.trunc(Number(tzOffsetMins)) : 0;
  try {
    const resp = await callDaemon(
      {
        op: 'usage_summary',
        window: win,
        currency: currency || null,
        tz_offset_mins: tz,
        force_sync: Boolean(forceSync),
      },
      4500
    );
    if (resp?.status === 'ok') return resp.data;
    if (resp?.status === 'err') return { error: resp.message };
  } catch {
    // fallback CLI
  }
  const args = ['usage', '--json', '--window', win, '--tz-offset-mins', String(tz)];
  if (currency) args.push('--currency', String(currency));
  if (forceSync) args.push('--force-sync');
  return await runCliJson(args);
}

async function setCurrency(currency) {
  const code = String(currency || 'USD').trim().toUpperCase();
  try {
    const resp = await callDaemon({ op: 'set_currency', currency: code });
    if (resp?.status === 'ok') return { ok: true, config: resp.data };
    if (resp?.status === 'err') return { ok: false, error: resp.message };
  } catch {
    // fallback CLI
  }
  await new Promise((res, rej) => {
    execFile(resolveHyperBin(), ['config', 'currency', code], { timeout: 3000 }, (err, _o, e) => {
      if (err) rej(new Error(e || err.message));
      else res();
    });
  });
  return { ok: true };
}

async function fetchJsonWithTimeout(url, timeoutMs = 4500) {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), timeoutMs);
  try {
    const res = await fetch(url, {
      method: 'GET',
      headers: { Accept: 'application/json' },
      signal: ctrl.signal,
    });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    return await res.json();
  } finally {
    clearTimeout(timer);
  }
}

async function syncLiveFxRates() {
  const providers = [
    {
      name: 'frankfurter',
      url: `https://api.frankfurter.dev/v1/latest?base=USD&symbols=${SUPPORTED_FX_CODES.join(',')}`,
    },
    {
      name: 'open-er-api',
      url: 'https://open.er-api.com/v6/latest/USD',
    },
  ];

  let rawRates = null;
  let providerName = '';
  let lastErr = 'network unavailable';

  for (const p of providers) {
    try {
      const payload = await fetchJsonWithTimeout(p.url, 4500);
      if (payload && typeof payload.rates === 'object' && payload.rates) {
        rawRates = payload.rates;
        providerName = p.name;
        break;
      }
    } catch (e) {
      lastErr = String(e?.message || e);
    }
  }

  if (!rawRates) {
    return { ok: false, error: `FX sync failed (${lastErr})` };
  }

  const rates = { USD: 1.0 };
  let validCount = 0;
  for (const code of SUPPORTED_FX_CODES) {
    const val = Number(rawRates[code]);
    if (Number.isFinite(val) && val > 0 && val < 1_000_000) {
      rates[code] = Math.round(val * 10000) / 10000;
      validCount += 1;
    }
  }
  if (validCount === 0) {
    return { ok: false, error: 'FX provider returned no usable rates' };
  }

  try {
    const resp = await callDaemon({ op: 'update_fx_rates', rates, source: 'live' }, 3000);
    if (resp?.status === 'ok') {
      return { ok: true, provider: providerName, rates, config: resp.data };
    }
    if (resp?.status === 'err') return { ok: false, error: resp.message };
  } catch {
    // fallback: persist via config.json atomic write when daemon is cold
  }

  try {
    const cfgPath = join(resolveDataDir(), 'config.json');
    let cur = {};
    if (existsSync(cfgPath)) {
      cur = JSON.parse(readFileSync(cfgPath, 'utf8'));
    }
    cur.fx_rates = { ...(cur.fx_rates || {}), ...rates };
    cur.fx_source = 'live';
    cur.fx_updated_at_ms = Date.now();
    mkdirSync(resolveDataDir(), { recursive: true });
    writeFileSync(cfgPath, JSON.stringify(cur, null, 2), { mode: 0o600 });
    return { ok: true, provider: providerName, rates, config: cur };
  } catch (e) {
    return { ok: false, error: String(e?.message || e) };
  }
}

function writeNativeClipboard(text) {
  const s = String(text ?? '');
  if (!s || s.length > 256) {
    return { ok: false, error: 'clipboard text must be 1..256 chars' };
  }
  const candidates =
    process.platform === 'darwin'
      ? [['pbcopy', []]]
      : process.platform === 'win32'
        ? [['clip', []]]
        : [
            ['wl-copy', []],
            ['xclip', ['-selection', 'clipboard']],
            ['xsel', ['--clipboard', '--input']],
          ];
  for (const [cmd, args] of candidates) {
    try {
      const r = spawnSync(cmd, args, { input: s, encoding: 'utf8', timeout: 1500, windowsHide: true });
      if (!r.error && r.status === 0) {
        return { ok: true, method: cmd };
      }
    } catch {
      // try next clipboard backend
    }
  }
  return { ok: false, error: 'no native clipboard utility available' };
}

async function startSidecarNative() {
  const { SidecarApp, Response } = await import('sidecar_sdk');
  const app = new SidecarApp();
  writeSidecarPid();

  app.page('/', () => new Response(renderIndexHtml(), {
    contentType: 'text/html; charset=utf-8',
    headers: NO_CACHE_HEADERS,
  }));

  for (const [route, [file, contentType]] of Object.entries(STATIC_FILES)) {
    app.api(route, () => new Response(readFileSync(join(__dirname, file), 'utf8'), {
      contentType,
      headers: NO_CACHE_HEADERS,
    }), 'GET');
  }

  app.api('/api/snapshot', async (data) => {
    const conv = data?.conversationId || app.config?.conversationId || '';
    try {
      return await getSnapshot(conv, data?.transcriptPath || '');
    } catch (e) {
      return { error: String(e?.message || e) };
    }
  }, 'GET');

  app.api('/api/profile', async (data) => {
    try {
      return await setProfile(data?.profile || 'balanced');
    } catch (e) {
      return { ok: false, error: String(e?.message || e) };
    }
  }, 'POST');

  app.api('/api/toggle', async (data) => {
    try {
      return await setToggle(data?.key || '', Boolean(data?.value));
    } catch (e) {
      return { ok: false, error: String(e?.message || e) };
    }
  }, 'POST');

  app.api('/api/rollback', async (data) => {
    try {
      return await rollbackTx(data?.tx_id || '', Boolean(data?.force));
    } catch (e) {
      return { ok: false, error: String(e?.message || e) };
    }
  }, 'POST');

  app.api('/api/usage', async (data) => {
    try {
      return await getUsageSummary(
        data?.window || '7d',
        data?.currency || '',
        data?.tzOffsetMins ?? 0,
        data?.forceSync === '1' || data?.forceSync === true
      );
    } catch (e) {
      return { error: String(e?.message || e) };
    }
  }, 'GET');

  app.api('/api/currency', async (data) => {
    try {
      return await setCurrency(data?.currency || 'USD');
    } catch (e) {
      return { ok: false, error: String(e?.message || e) };
    }
  }, 'POST');

  app.api('/api/fx/sync', async () => {
    try {
      return await syncLiveFxRates();
    } catch (e) {
      return { ok: false, error: String(e?.message || e) };
    }
  }, 'POST');

  app.api('/api/clipboard', async (data) => {
    return writeNativeClipboard(data?.text || '');
  }, 'POST');

  app.run();
}

function readBody(req) {
  return new Promise((resolve) => {
    let raw = '';
    req.on('data', (c) => {
      raw += c;
      if (raw.length > 65536) req.destroy();
    });
    req.on('end', () => {
      try {
        resolve(raw ? JSON.parse(raw) : {});
      } catch {
        resolve({});
      }
    });
  });
}

function startStandaloneHttp() {
  const port = Number(process.env.AGY_HYPER_STANDALONE_PORT || 47821);
  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url || '/', `http://127.0.0.1:${port}`);
    const pathname = url.pathname;

    if (pathname === '/' || pathname === '/index.html') {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8', ...NO_CACHE_HEADERS });
      res.end(renderIndexHtml());
      return;
    }
    if (pathname === '/preload.js') {
      // Safe no-op stub when opened outside Antigravity
      res.writeHead(200, { 'Content-Type': 'application/javascript; charset=utf-8', ...NO_CACHE_HEADERS });
      res.end('window.__AGY_HYPER_STANDALONE__ = true;\n');
      return;
    }
    if (STATIC_FILES[pathname]) {
      const [file, contentType] = STATIC_FILES[pathname];
      res.writeHead(200, { 'Content-Type': contentType, ...NO_CACHE_HEADERS });
      res.end(readFileSync(join(__dirname, file), 'utf8'));
      return;
    }

    const sendJson = (status, obj) => {
      res.writeHead(status, { 'Content-Type': 'application/json; charset=utf-8', ...NO_CACHE_HEADERS });
      res.end(JSON.stringify(obj));
    };

    try {
      if (pathname === '/api/snapshot' && req.method === 'GET') {
        const snap = await getSnapshot(
          url.searchParams.get('conversationId') || '',
          url.searchParams.get('transcriptPath') || ''
        );
        return sendJson(200, snap);
      }
      if (pathname === '/api/profile' && req.method === 'POST') {
        const body = await readBody(req);
        return sendJson(200, await setProfile(body.profile || 'balanced'));
      }
      if (pathname === '/api/toggle' && req.method === 'POST') {
        const body = await readBody(req);
        return sendJson(200, await setToggle(body.key || '', Boolean(body.value)));
      }
      if (pathname === '/api/rollback' && req.method === 'POST') {
        const body = await readBody(req);
        return sendJson(200, await rollbackTx(body.tx_id || '', Boolean(body.force)));
      }
      if (pathname === '/api/usage' && req.method === 'GET') {
        const rep = await getUsageSummary(
          url.searchParams.get('window') || '7d',
          url.searchParams.get('currency') || '',
          url.searchParams.get('tzOffsetMins') || '0',
          url.searchParams.get('forceSync') === '1'
        );
        return sendJson(200, rep);
      }
      if (pathname === '/api/currency' && req.method === 'POST') {
        const body = await readBody(req);
        return sendJson(200, await setCurrency(body.currency || 'USD'));
      }
      if (pathname === '/api/fx/sync' && req.method === 'POST') {
        return sendJson(200, await syncLiveFxRates());
      }
      if (pathname === '/api/clipboard' && req.method === 'POST') {
        const body = await readBody(req);
        return sendJson(200, writeNativeClipboard(body.text || ''));
      }
      sendJson(404, { error: 'not found' });
    } catch (err) {
      sendJson(500, { ok: false, error: String(err?.message || err) });
    }
  });

  server.listen(port, '127.0.0.1', () => {
    console.log(`[agy-hyper-deck] standalone cockpit listening on http://127.0.0.1:${port}`);
  });
}

if (process.env.AGY_HYPER_STANDALONE_PORT) {
  startStandaloneHttp();
} else {
  startSidecarNative().catch(() => {
    startStandaloneHttp();
  });
}
