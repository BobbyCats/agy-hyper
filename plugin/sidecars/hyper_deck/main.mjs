/**
 * AGY Hyper Deck — Antigravity Native Sidecar + Standalone HTTP Bridge.
 * Zero npm dependencies. Connects directly to the Rust daemon over Unix Domain Socket (<0.5ms)
 * and gracefully falls back to `agy-hyper snapshot` CLI if the daemon is cold.
 */
import { execFile, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, renameSync, unlinkSync, writeFileSync } from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import { homedir, tmpdir } from 'node:os';
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

const UPDATE_REPO = 'BobbyCats/agy-hyper';
const UPDATE_TTL_MS = 6 * 3600 * 1000;
let updateCheckInFlight = null;

function resolveUpdateStatePath() {
  return join(resolveDataDir(), 'update_state.json');
}

function readInstalledVersion() {
  try {
    const pkgPath = join(__dirname, '..', '..', 'plugin.json');
    if (existsSync(pkgPath)) {
      const parsed = JSON.parse(readFileSync(pkgPath, 'utf8'));
      const ver = String(parsed?.version || '').trim().replace(/^v/i, '');
      if (/^\d+\.\d+\.\d+/.test(ver)) return ver;
    }
  } catch {
    // fallback to built-in version
  }
  return '0.5.1';
}

function parseSemverStrict(raw) {
  const clean = String(raw || '').trim().replace(/^v/i, '');
  const m = /^(\d+)\.(\d+)\.(\d+)(?:\+[0-9A-Za-z.-]+)?$/.exec(clean);
  if (!m) return null;
  return [Number(m[1]), Number(m[2]), Number(m[3])];
}

function isNewerSemver(currentVersion, latestVersion) {
  const cand = parseSemverStrict(latestVersion);
  if (!cand) return false;
  const curClean = String(currentVersion || '').trim().replace(/^v/i, '').split('+')[0] || '';
  const dashIdx = curClean.indexOf('-');
  const curBase = dashIdx === -1 ? curClean : curClean.slice(0, dashIdx);
  const curHasPrerelease = dashIdx !== -1 && curClean.slice(dashIdx + 1).length > 0;
  const cur = parseSemverStrict(curBase);
  if (!cur) return false;
  if (cand[0] !== cur[0]) return cand[0] > cur[0];
  if (cand[1] !== cur[1]) return cand[1] > cur[1];
  if (cand[2] !== cur[2]) return cand[2] > cur[2];
  return curHasPrerelease;
}

function readUpdateState() {
  const currentVersion = readInstalledVersion();
  const base = {
    checked_at_ms: 0,
    current_version: currentVersion,
    latest_version: null,
    update_available: false,
    release_url: null,
    published_at: null,
    dismissed_version: null,
    last_error: null,
  };
  try {
    const p = resolveUpdateStatePath();
    if (!existsSync(p)) return base;
    const raw = JSON.parse(readFileSync(p, 'utf8'));
    const latest = raw?.latest_version ? String(raw.latest_version).trim().replace(/^v/i, '') : null;
    const dismissed = raw?.dismissed_version ? String(raw.dismissed_version).trim().replace(/^v/i, '') : null;
    return {
      checked_at_ms: Math.max(0, Math.trunc(Number(raw?.checked_at_ms) || 0)),
      current_version: currentVersion,
      latest_version: latest,
      update_available: Boolean(latest && isNewerSemver(currentVersion, latest)),
      release_url: raw?.release_url
        ? String(raw.release_url)
        : latest
          ? `https://github.com/${UPDATE_REPO}/releases/tag/v${latest}`
          : null,
      published_at: raw?.published_at ? String(raw.published_at) : null,
      dismissed_version: dismissed,
      last_error: raw?.last_error ? String(raw.last_error) : null,
    };
  } catch {
    return base;
  }
}

function writeUpdateStateAtomic(nextState) {
  const dir = resolveDataDir();
  mkdirSync(dir, { recursive: true });
  const target = resolveUpdateStatePath();
  const payload = JSON.stringify(nextState, null, 2);
  const tmp = join(dir, `.update_state.${process.pid}.${Date.now()}.tmp`);
  try {
    writeFileSync(tmp, payload, { mode: 0o600 });
    renameSync(tmp, target);
  } catch {
    try {
      if (existsSync(tmp)) unlinkSync(tmp);
    } catch {
      // ignore
    }
    writeFileSync(target, payload, { mode: 0o600 });
  }
  return nextState;
}

function isUpdateCheckEnabled() {
  if (process.env.AGY_HYPER_NO_UPDATE_CHECK) return false;
  try {
    const cfgPath = join(resolveDataDir(), 'config.json');
    if (existsSync(cfgPath)) {
      const cfg = JSON.parse(readFileSync(cfgPath, 'utf8'));
      if (cfg && cfg.enable_update_check === false) return false;
    }
  } catch {
    // default enabled
  }
  return true;
}

async function fetchReleaseRedirectTag(method, timeoutMs = 3500) {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), timeoutMs);
  try {
    const res = await fetch(`https://github.com/${UPDATE_REPO}/releases/latest`, {
      method,
      redirect: 'manual',
      headers: { 'User-Agent': 'agy-hyper-deck' },
      signal: ctrl.signal,
    });
    const loc = res.headers.get('location') || '';
    const m = /\/releases\/tag\/v?([0-9]+\.[0-9]+\.[0-9]+)(?=$|[/?#])/i.exec(loc);
    if (!m) return null;
    const latestVersion = m[1];
    const releaseUrl = loc.startsWith('http')
      ? loc
      : `https://github.com${loc.startsWith('/') ? '' : '/'}${loc}`;
    return { latestVersion, releaseUrl, publishedAt: null };
  } finally {
    clearTimeout(timer);
  }
}

async function checkReleaseUpdate(force = false) {
  if (process.env.AGY_HYPER_NO_UPDATE_CHECK) {
    return readUpdateState();
  }
  const prev = readUpdateState();
  if (!force) {
    if (!isUpdateCheckEnabled()) return prev;
    if (prev.checked_at_ms > 0 && Date.now() - prev.checked_at_ms < UPDATE_TTL_MS) {
      return prev;
    }
  }
  if (updateCheckInFlight) return updateCheckInFlight;

  updateCheckInFlight = (async () => {
    let resolved = null;
    let lastErr = null;

    for (const method of ['HEAD', 'GET']) {
      try {
        resolved = await fetchReleaseRedirectTag(method, 3500);
        if (resolved?.latestVersion) break;
      } catch (e) {
        lastErr = String(e?.message || e);
      }
    }

    if (!resolved?.latestVersion) {
      try {
        const apiPayload = await fetchJsonWithTimeout(
          `https://api.github.com/repos/${UPDATE_REPO}/releases/latest`,
          3500
        );
        const rawTag = String(apiPayload?.tag_name || '').trim().replace(/^v/i, '');
        if (parseSemverStrict(rawTag) && !apiPayload?.prerelease && !apiPayload?.draft) {
          resolved = {
            latestVersion: rawTag,
            releaseUrl:
              apiPayload?.html_url || `https://github.com/${UPDATE_REPO}/releases/tag/v${rawTag}`,
            publishedAt: apiPayload?.published_at ? String(apiPayload.published_at) : null,
          };
        }
      } catch (e) {
        lastErr = String(e?.message || e);
      }
    }

    const latestDisk = readUpdateState();
    const currentVersion = readInstalledVersion();
    const now = Date.now();

    if (resolved?.latestVersion) {
      const latestVersion = resolved.latestVersion;
      const next = {
        checked_at_ms: now,
        current_version: currentVersion,
        latest_version: latestVersion,
        update_available: isNewerSemver(currentVersion, latestVersion),
        release_url:
          resolved.releaseUrl || `https://github.com/${UPDATE_REPO}/releases/tag/v${latestVersion}`,
        published_at: resolved.publishedAt || latestDisk.published_at || null,
        dismissed_version: latestDisk.dismissed_version || null,
        last_error: null,
      };
      return writeUpdateStateAtomic(next);
    }

    const fallbackState = {
      ...latestDisk,
      checked_at_ms: now,
      current_version: currentVersion,
      last_error: lastErr || 'unable to resolve latest release tag',
    };
    return writeUpdateStateAtomic(fallbackState);
  })().finally(() => {
    updateCheckInFlight = null;
  });

  return updateCheckInFlight;
}

function dismissReleaseUpdate(version) {
  const cur = readUpdateState();
  const raw = String(version ?? '').trim();
  let ver = null;
  if (raw && !/^(clear|none)$/i.test(raw)) {
    ver = raw.replace(/^v/i, '').trim() || null;
  } else if (!raw && cur.latest_version) {
    ver = String(cur.latest_version).trim().replace(/^v/i, '') || null;
  }
  return writeUpdateStateAtomic({
    ...cur,
    dismissed_version: ver,
  });
}

function execHyperUpdate(args) {
  return new Promise((resolve, reject) => {
    execFile(
      resolveHyperBin(),
      args,
      { timeout: 90000, maxBuffer: 4 * 1024 * 1024 },
      (err, stdout, stderr) => {
        if (err) {
          const detail = String(stderr || stdout || err.message || err).trim();
          return reject(new Error(detail || 'update command failed'));
        }
        resolve({ stdout: String(stdout || ''), stderr: String(stderr || '') });
      }
    );
  });
}

async function applyReleaseUpdate(isSidecarMode = false) {
  let out;
  try {
    out = await execHyperUpdate(['update', '--json']);
  } catch (err) {
    const msg = String(err?.message || err);
    if (msg.includes('--json') || msg.includes('unexpected argument')) {
      out = await execHyperUpdate(['update']);
    } else {
      return { ok: false, error: msg };
    }
  }

  let result = null;
  const trimmed = out.stdout.trim();
  if (trimmed) {
    try {
      result = JSON.parse(trimmed);
    } catch {
      result = { message: trimmed };
    }
  } else {
    result = { installed_version: readInstalledVersion() };
  }

  if (result && (result.ok === false || result.error)) {
    return { ok: false, error: String(result.error || 'update failed'), result };
  }

  const installedVersion = String(
    result?.installed_version || result?.version || readInstalledVersion()
  )
    .trim()
    .replace(/^v/i, '');
  const curState = readUpdateState();
  writeUpdateStateAtomic({
    ...curState,
    checked_at_ms: Date.now(),
    current_version: installedVersion || curState.current_version,
    update_available: Boolean(
      curState.latest_version && isNewerSemver(installedVersion || curState.current_version, curState.latest_version)
    ),
    last_error: null,
  });

  if (isSidecarMode) {
    const t = setTimeout(() => process.exit(0), 600);
    if (typeof t.unref === 'function') t.unref();
  }
  return { ok: true, restarting: Boolean(isSidecarMode), result };
}

function scheduleUpdateRadar() {
  if (process.env.AGY_HYPER_NO_UPDATE_CHECK) return;
  const runBg = () => {
    if (!isUpdateCheckEnabled()) return;
    checkReleaseUpdate(false).catch(() => {});
  };
  const initTimer = setTimeout(runBg, 3500);
  if (typeof initTimer.unref === 'function') initTimer.unref();
  const loopTimer = setInterval(runBg, UPDATE_TTL_MS);
  if (typeof loopTimer.unref === 'function') loopTimer.unref();
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
      const snap = {
        ...resp.data,
        transport: 'uds-daemon',
        rpc_rtt_ms: Math.round((performance.now() - t0) * 100) / 100,
      };
      if (!snap.update_state) snap.update_state = readUpdateState();
      return snap;
    }
  } catch {
    // Fall back to direct CLI snapshot (also auto-spawns daemon if available)
  }
  const args = ['snapshot'];
  if (conversationId) args.push('--conversation-id', conversationId);
  if (transcriptPath) args.push('--transcript-path', transcriptPath);
  const data = await runCliJson(args);
  const snap = {
    ...data,
    transport: 'cli-fallback',
    rpc_rtt_ms: Math.round((performance.now() - t0) * 100) / 100,
  };
  if (!snap.update_state) snap.update_state = readUpdateState();
  return snap;
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

async function getTxDiff(txId, maxLines = 240) {
  const id = String(txId || '').trim();
  if (!id) return { ok: false, error: 'missing tx_id' };
  const cap = Number.isFinite(Number(maxLines)) ? Math.trunc(Number(maxLines)) : 240;
  try {
    const resp = await callDaemon({ op: 'inspect_tx_diff', tx_id: id, max_lines: cap }, 3000);
    if (resp?.status === 'ok') return { ok: true, diff: resp.data };
    if (resp?.status === 'err') return { ok: false, error: resp.message };
  } catch {
    // fallback CLI
  }
  const diff = await runCliJson(['tx', 'diff', id, '--max-lines', String(cap), '--json']);
  return { ok: true, diff };
}

async function getAuditReport(days = 14, maxSessions = 80, tzOffsetMins = 0) {
  const d = Number.isFinite(Number(days)) ? Math.max(1, Math.min(90, Math.trunc(Number(days)))) : 14;
  const s = Number.isFinite(Number(maxSessions)) ? Math.max(5, Math.min(200, Math.trunc(Number(maxSessions)))) : 80;
  const tz = Number.isFinite(Number(tzOffsetMins)) ? Math.trunc(Number(tzOffsetMins)) : 0;
  try {
    const resp = await callDaemon({ op: 'audit_report', days: d, max_sessions: s, tz_offset_mins: tz }, 8000);
    if (resp?.status === 'ok') return resp.data;
    if (resp?.status === 'err') return { error: resp.message };
  } catch {
    // fallback CLI
  }
  return await runCliJson(['audit', '--days', String(d), '--sessions', String(s), '--tz-offset-mins', String(tz), '--json']);
}

async function getAuditSvg(days = 14, theme = 'dark', tzOffsetMins = 0, share = false) {
  const d = Number.isFinite(Number(days)) ? Math.max(1, Math.min(90, Math.trunc(Number(days)))) : 14;
  const th = String(theme || 'dark').toLowerCase() === 'light' ? 'light' : 'dark';
  const tz = Number.isFinite(Number(tzOffsetMins)) ? Math.trunc(Number(tzOffsetMins)) : 0;
  if (share) {
    const card = await getAuditShareCard(d, th, tz, false);
    if (card?.ok && card.svg) return card.svg;
    throw new Error(card?.error || 'share card SVG unavailable');
  }
  const tmpSvg = join(tmpdir(), `agy-hyper-audit-${process.pid}-${Date.now()}.svg`);
  try {
    await new Promise((resolve, reject) => {
      execFile(
        resolveHyperBin(),
        ['audit', '--days', String(d), '--sessions', '80', '--theme', th, '--tz-offset-mins', String(tz), '--svg', tmpSvg, '--json'],
        { timeout: 10000, maxBuffer: 4 * 1024 * 1024 },
        (err, _out, stderr) => {
          if (err) reject(new Error(stderr || err.message));
          else resolve();
        }
      );
    });
    return readFileSync(tmpSvg, 'utf8');
  } finally {
    try {
      if (existsSync(tmpSvg)) unlinkSync(tmpSvg);
    } catch {
      // ignore
    }
  }
}

async function getAuditShareCard(days = 14, theme = 'dark', tzOffsetMins = 0, includePng = false) {
  const d = Number.isFinite(Number(days)) ? Math.max(1, Math.min(90, Math.trunc(Number(days)))) : 14;
  const th = String(theme || 'dark').toLowerCase() === 'light' ? 'light' : 'dark';
  const tz = Number.isFinite(Number(tzOffsetMins)) ? Math.trunc(Number(tzOffsetMins)) : 0;
  const wantPng = Boolean(includePng);

  try {
    const resp = await callDaemon(
      {
        op: 'audit_share_card',
        days: d,
        max_sessions: 80,
        tz_offset_mins: tz,
        theme: th,
        include_png: wantPng,
      },
      12000
    );
    if (resp?.status === 'ok' && resp.data?.svg) {
      return { ok: true, ...resp.data };
    }
  } catch {
    // fall back to CLI share-card export
  }

  const stamp = `${process.pid}-${Date.now()}`;
  const tmpSvg = join(tmpdir(), `agy-hyper-share-${stamp}.svg`);
  const tmpPng = join(tmpdir(), `agy-hyper-share-${stamp}.png`);
  try {
    const args = [
      'audit',
      '--days',
      String(d),
      '--sessions',
      '80',
      '--theme',
      th,
      '--tz-offset-mins',
      String(tz),
      '--share-svg',
      tmpSvg,
      '--json',
    ];
    if (wantPng) {
      args.push('--share-png', tmpPng);
    }
    const stdout = await new Promise((resolve, reject) => {
      execFile(
        resolveHyperBin(),
        args,
        { timeout: 15000, maxBuffer: 6 * 1024 * 1024 },
        (err, out, stderr) => {
          if (err) reject(new Error(stderr || err.message));
          else resolve(out || '');
        }
      );
    });
    let verdict = 'HEALTHY';
    try {
      const parsed = JSON.parse(stdout);
      if (parsed?.verdict) verdict = String(parsed.verdict);
    } catch {
      // ignore non-json stdout
    }
    const svg = existsSync(tmpSvg) ? readFileSync(tmpSvg, 'utf8') : '';
    if (!svg.trim().startsWith('<svg')) {
      return { ok: false, error: 'share card SVG generation failed' };
    }
    const pngBase64 = wantPng && existsSync(tmpPng) ? readFileSync(tmpPng).toString('base64') : null;
    return {
      ok: true,
      svg,
      png_base64: pngBase64,
      svg_width: 920,
      svg_height: 500,
      png_width: 1920,
      png_height: 1080,
      theme: th,
      verdict,
      redacted: true,
    };
  } catch (err) {
    return { ok: false, error: String(err?.message || err) };
  } finally {
    for (const p of [tmpSvg, tmpPng]) {
      try {
        if (existsSync(p)) unlinkSync(p);
      } catch {
        // ignore
      }
    }
  }
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
  if (!s || s.length > 512) {
    return { ok: false, error: 'clipboard text must be 1..512 chars' };
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

  app.api('/api/tx/diff', async (data) => {
    try {
      return await getTxDiff(data?.txId || data?.tx_id || '', data?.maxLines ?? 240);
    } catch (e) {
      return { ok: false, error: String(e?.message || e) };
    }
  }, 'GET');

  app.api('/api/audit', async (data) => {
    try {
      return await getAuditReport(data?.days ?? 14, data?.sessions ?? 80, data?.tzOffsetMins ?? 0);
    } catch (e) {
      return { error: String(e?.message || e) };
    }
  }, 'GET');

  app.api('/api/audit/svg', async (data) => {
    try {
      const share = data?.share === '1' || data?.share === 'true' || data?.share === true;
      const svg = await getAuditSvg(data?.days ?? 14, data?.theme || 'dark', data?.tzOffsetMins ?? 0, share);
      return new Response(svg, {
        contentType: 'image/svg+xml; charset=utf-8',
        headers: NO_CACHE_HEADERS,
      });
    } catch (e) {
      return { ok: false, error: String(e?.message || e) };
    }
  }, 'GET');

  app.api('/api/audit/share-card', async (data) => {
    try {
      const wantPng = data?.includePng === '1' || data?.includePng === 'true' || data?.includePng === true || data?.format === 'png';
      const card = await getAuditShareCard(data?.days ?? 14, data?.theme || 'dark', data?.tzOffsetMins ?? 0, wantPng);
      if (data?.format === 'svg' && card?.ok && card.svg) {
        return new Response(card.svg, {
          contentType: 'image/svg+xml; charset=utf-8',
          headers: NO_CACHE_HEADERS,
        });
      }
      return card;
    } catch (e) {
      return { ok: false, error: String(e?.message || e) };
    }
  }, 'GET');

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

  app.api('/api/update/status', async (data) => {
    try {
      const force = data?.force === '1' || data?.force === 'true' || data?.force === true;
      const st = force ? await checkReleaseUpdate(true) : readUpdateState();
      return { ok: true, ...st };
    } catch (e) {
      return { ok: false, error: String(e?.message || e) };
    }
  }, 'GET');

  app.api('/api/update/dismiss', async (data) => {
    try {
      const st = dismissReleaseUpdate(data?.version || '');
      return { ok: true, ...st };
    } catch (e) {
      return { ok: false, error: String(e?.message || e) };
    }
  }, 'POST');

  app.api('/api/update/apply', async () => {
    try {
      return await applyReleaseUpdate(true);
    } catch (e) {
      return { ok: false, error: String(e?.message || e) };
    }
  }, 'POST');

  scheduleUpdateRadar();
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
      if (pathname === '/api/tx/diff' && req.method === 'GET') {
        return sendJson(
          200,
          await getTxDiff(url.searchParams.get('txId') || '', url.searchParams.get('maxLines') || '240')
        );
      }
      if (pathname === '/api/audit' && req.method === 'GET') {
        return sendJson(
          200,
          await getAuditReport(
            url.searchParams.get('days') || '14',
            url.searchParams.get('sessions') || '80',
            url.searchParams.get('tzOffsetMins') || '0'
          )
        );
      }
      if (pathname === '/api/audit/svg' && req.method === 'GET') {
        const share = url.searchParams.get('share') === '1' || url.searchParams.get('share') === 'true';
        const svg = await getAuditSvg(
          url.searchParams.get('days') || '14',
          url.searchParams.get('theme') || 'dark',
          url.searchParams.get('tzOffsetMins') || '0',
          share
        );
        res.writeHead(200, { 'Content-Type': 'image/svg+xml; charset=utf-8', ...NO_CACHE_HEADERS });
        res.end(svg);
        return;
      }
      if (pathname === '/api/audit/share-card' && req.method === 'GET') {
        const fmt = url.searchParams.get('format') || 'json';
        const wantPng = url.searchParams.get('includePng') === '1' || url.searchParams.get('includePng') === 'true' || fmt === 'png';
        const card = await getAuditShareCard(
          url.searchParams.get('days') || '14',
          url.searchParams.get('theme') || 'dark',
          url.searchParams.get('tzOffsetMins') || '0',
          wantPng
        );
        if (fmt === 'svg' && card?.ok && card.svg) {
          res.writeHead(200, { 'Content-Type': 'image/svg+xml; charset=utf-8', ...NO_CACHE_HEADERS });
          res.end(card.svg);
          return;
        }
        return sendJson(card?.ok ? 200 : 500, card);
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
      if (pathname === '/api/update/status' && req.method === 'GET') {
        const force = url.searchParams.get('force') === '1' || url.searchParams.get('force') === 'true';
        const st = force ? await checkReleaseUpdate(true) : readUpdateState();
        return sendJson(200, { ok: true, ...st });
      }
      if (pathname === '/api/update/dismiss' && req.method === 'POST') {
        const body = await readBody(req);
        const st = dismissReleaseUpdate(body.version || '');
        return sendJson(200, { ok: true, ...st });
      }
      if (pathname === '/api/update/apply' && req.method === 'POST') {
        return sendJson(200, await applyReleaseUpdate(false));
      }
      sendJson(404, { error: 'not found' });
    } catch (err) {
      sendJson(500, { ok: false, error: String(err?.message || err) });
    }
  });

  scheduleUpdateRadar();
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
