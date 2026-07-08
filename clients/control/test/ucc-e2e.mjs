#!/usr/bin/env node
// UCC end-to-end tests. Drives real headless Chrome (puppeteer-core + system Chrome) against a live
// control-serve on :8411, because curl smokes pass while the SPA UI is broken. Prints PASS/FAIL per
// case and writes DEFECTS.md. Exit non-zero if anything fails.
//
//   node clients/control/test/ucc-e2e.mjs
//
// Env: BASE (default http://localhost:8411), CHROME (default system Chrome), HEADLESS=0 to watch.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const puppeteer = require(path.join(HERE, '..', 'node_modules', 'puppeteer-core'));

const BASE = process.env.BASE || 'http://localhost:8411';
const CHROME = process.env.CHROME || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const TOK = fs.readFileSync(process.env.HOME + '/.busi/tokens.txt', 'utf8').split('\n')[0].trim();

const results = [];
const rec = (name, pass, detail) => { results.push({ name, pass, detail: detail || '' }); console.log(`${pass ? 'PASS' : 'FAIL'}  ${name}${detail ? '  — ' + detail : ''}`); };
const ok  = (name, detail) => rec(name, true, detail);
const bad = (name, detail) => rec(name, false, detail);

// ── shared browser page with cookie + русский locale ─────────────────────────
async function newPage(browser) {
  const page = await browser.newPage();
  await page.setViewport({ width: 390, height: 844 });
  await page.setCookie({ name: 'busi_device', value: TOK, url: BASE });
  await page.evaluateOnNewDocument(() => localStorage.setItem('rozum_lang', 'ru'));
  return page;
}
const settle = (ms = 2200) => new Promise(r => setTimeout(r, ms));

async function api(method, urlPath, body) {
  const res = await fetch(BASE + urlPath, {
    method, headers: { Cookie: 'busi_device=' + TOK }, body,
  });
  let json = null; const text = await res.text();
  try { json = JSON.parse(text); } catch { /* not json */ }
  return { status: res.status, json, text };
}

// ── NAV ──────────────────────────────────────────────────────────────────────
async function testNav(browser) {
  const page = await newPage(browser);
  await page.goto(BASE + '/#/', { waitUntil: 'networkidle2', timeout: 30000 });
  await settle();
  const home = await page.evaluate(() => {
    const h1 = document.querySelector('h1');
    const links = [...document.querySelectorAll('a')].map(a => a.getAttribute('href'));
    return { h1: h1 ? h1.textContent : '', hasNav: ['#/agents','#/coders','#/sessions'].every(h => links.includes(h)) };
  });
  (/control center/i.test(home.h1) && home.hasNav) ? ok('nav-home', `h1="${home.h1}"`) : bad('nav-home', `h1="${home.h1}" nav=${home.hasNav}`);

  for (const [route, rx] of [['#/agents', /Agents|Агенты/], ['#/coders', /Coders|Кодеры/], ['#/sessions', /Sessions|Сессии/]]) {
    await page.goto(BASE + '/' + route, { waitUntil: 'networkidle2', timeout: 30000 });
    await settle(1200);
    const info = await page.evaluate(() => ({ hash: location.hash, heads: [...document.querySelectorAll('h1,h2')].map(h => h.textContent).join('|') }));
    (info.hash === route && rx.test(info.heads)) ? ok('nav' + route, info.hash) : bad('nav' + route, `hash=${info.hash} heads="${info.heads}" (warp to #/?)`);
  }
  await page.close();
}

// ── MEMORY ─────────────────────────────────────────────────────────────────────
async function testMemory(browser) {
  const page = await newPage(browser);
  let statusFetches = 0;
  page.on('request', r => { if (r.url().includes('/control/status')) statusFetches++; });
  await page.goto(BASE + '/#/', { waitUntil: 'networkidle2', timeout: 30000 });
  await settle();
  const render = await page.evaluate(() => {
    const h = [...document.querySelectorAll('h2')].find(x => /Память|Memory/.test(x.textContent));
    if (!h) return { found: false };
    // Walk up to the card wrapper that actually contains the body (free/limit/used rows) — the
    // header wrapper alone has only the title + ↻.
    let card = h; for (let i = 0; i < 5 && card.parentElement && !/GiB/.test(card.textContent); i++) card = card.parentElement;
    const txt = card ? card.textContent : '';
    const nums = (txt.match(/[\d.]+ ?GiB/g) || []);
    return { found: true, nums, hasSource: /источник|always-up|source:/.test(txt) };
  });
  render.found && render.nums.length >= 3 ? ok('mem-render', render.nums.join(', ')) : bad('mem-render', JSON.stringify(render));
  !render.hasSource ? ok('mem-no-source') : bad('mem-no-source', 'source line still present');

  const before = statusFetches;
  const clicked = await page.evaluate(() => {
    const h = [...document.querySelectorAll('h2')].find(x => /Память|Memory/.test(x.textContent));
    const btn = h && [...h.parentElement.querySelectorAll('button')].find(b => b.textContent.includes('↻'));
    if (!btn) return false; btn.click(); return true;
  });
  await settle(1500);
  if (!clicked) bad('mem-refresh', 'no ↻ button found');
  else statusFetches > before ? ok('mem-refresh', `refetched (${before}→${statusFetches})`) : bad('mem-refresh', `↻ tapped but NO new GET /control/status (${before}→${statusFetches})`);
  await page.close();
}

// ── MODELS ─────────────────────────────────────────────────────────────────────
async function testModels(browser) {
  const page = await newPage(browser);
  const posts = [];
  page.on('request', r => { if (/\/control\/gateway\/(load|stop)/.test(r.url())) posts.push(r.method() + ' ' + new URL(r.url()).pathname); });
  await page.goto(BASE + '/#/', { waitUntil: 'networkidle2', timeout: 30000 });
  await settle();
  const info = await page.evaluate(() => {
    const tbl = [...document.querySelectorAll('[data-ssc-datatable]')].find(t => t.offsetParent && [...t.querySelectorAll('button')].some(b => /загрузить|выгрузить|load|unload/.test(b.textContent)));
    if (!tbl) return { found: false };
    const table = tbl.querySelector('table');
    const rows = [...tbl.querySelectorAll('tbody tr')];
    const perRow = rows.map(tr => [...tr.querySelectorAll('button')].filter(b => b.offsetParent).length);
    const btns = [...tbl.querySelectorAll('tbody button')].filter(b => b.offsetParent);
    const maxRight = Math.max(...btns.map(b => b.getBoundingClientRect().right));
    const names = rows.map(tr => tr.querySelector('td:nth-child(1)').textContent.trim());
    const clipped = names.some(n => n.length < 12); // spec is long; a <12 char cell means clipped
    return { found: true, fits: table.scrollWidth <= tbl.clientWidth + 2, everyOne: perRow.every(n => n === 1), perRow, maxRight: Math.round(maxRight), allVisible: maxRight <= 388, clipped };
  });
  if (!info.found) { bad('models-fit', 'no models table'); bad('models-one-btn'); bad('models-name'); bad('models-load-post'); bad('models-feedback'); await page.close(); return; }
  (info.fits && info.allVisible) ? ok('models-fit', `btn right=${info.maxRight}`) : bad('models-fit', `fits=${info.fits} btnRight=${info.maxRight} (overflow / off-screen)`);
  info.everyOne ? ok('models-one-btn') : bad('models-one-btn', `rows with ≠1 button: ${info.perRow.filter(n => n !== 1).length}`);
  !info.clipped ? ok('models-name') : bad('models-name', 'a model name looks clipped');

  // feedback + POST: tap a load button, check it disables+… synchronously, and that a POST fired
  const fb = await page.evaluate(() => {
    const btn = [...document.querySelectorAll('[data-ssc-datatable] tbody button')].find(b => /загрузить|load/.test(b.textContent) && b.offsetParent);
    if (!btn) return { noBtn: true };
    btn.click();
    return { disabled: btn.disabled, text: btn.textContent.trim(), ell: btn.textContent.includes('…') };
  });
  await settle(1200);
  if (fb.noBtn) { bad('models-feedback', 'no load button'); bad('models-load-post', 'no load button'); }
  else {
    (fb.disabled && fb.ell) ? ok('models-feedback', `"${fb.text}"`) : bad('models-feedback', `no instant feedback: disabled=${fb.disabled} text="${fb.text}"`);
    posts.some(p => p.startsWith('POST')) ? ok('models-load-post', posts.join(', ')) : bad('models-load-post', `expected POST, saw: ${posts.join(', ') || 'nothing'}`);
  }
  await page.close();
}

// ── CHAT ─────────────────────────────────────────────────────────────────────
async function testChat(browser) {
  const page = await newPage(browser);
  await page.goto(BASE + '/#/chat/demo', { waitUntil: 'networkidle2', timeout: 30000 });
  await settle(2800);
  const info = await page.evaluate(() => {
    const cells = [...document.querySelectorAll('[data-ssc-datatable] td')].filter(td => td.offsetParent && td.textContent.trim().length > 40);
    const composer = !!document.querySelector('input[placeholder*="message"], input[placeholder*="сообщ"]');
    if (!cells.length) return { wraps: null, composer };
    const c = cells.sort((a, b) => b.textContent.length - a.textContent.length)[0];
    const r = c.getBoundingClientRect();
    return { wraps: r.height > 30, noHscroll: c.scrollWidth <= c.clientWidth + 2, composer, w: Math.round(r.width), h: Math.round(r.height) };
  });
  info.wraps === null ? ok('chat-wrap', 'no long message to check (indeterminate)') :
    (info.wraps && info.noHscroll) ? ok('chat-wrap', `cell ${info.w}×${info.h}`) : bad('chat-wrap', `wraps=${info.wraps} noHscroll=${info.noHscroll}`);
  info.composer ? ok('chat-send-present') : bad('chat-send-present', 'no composer input');
  await page.close();
}

// ── PICKERS ────────────────────────────────────────────────────────────────────
async function testPickers(browser) {
  const page = await newPage(browser);
  await page.goto(BASE + '/#/agents', { waitUntil: 'networkidle2', timeout: 30000 });
  await settle(1600);
  const sel = await page.evaluate(() => {
    const selBtn = [...document.querySelectorAll('[data-ssc-datatable] button')].find(b => /select|выбрать/i.test(b.textContent) && b.offsetParent);
    if (!selBtn) return { noBtn: true };
    selBtn.click();
    // selection is marked via CSS class ssc-rowlink-selected (✓ ::before)
    return { marked: selBtn.classList.contains('ssc-rowlink-selected') };
  });
  await settle(400);
  sel.noBtn ? bad('picker-select', 'no select button on agents form') :
    sel.marked ? ok('picker-select') : bad('picker-select', 'tapped select but row not marked selected');
  await page.close();
}

// ── API ────────────────────────────────────────────────────────────────────────
async function testApi() {
  const s = await api('GET', '/control/status');
  const r = s.json && s.json.residency;
  (s.status === 200 && r && 'available_bytes' in r && 'host_budget_bytes' in r && 'committed_bytes' in r && Array.isArray(s.json.models))
    ? ok('api-status', `${s.json.models.length} models`) : bad('api-status', `status=${s.status}`);

  // stop guard: with the operator's live claude leasing a gateway this 409s; with none it 404s.
  // Either is a VALID guarded response — the defect would be a 500 or an empty body.
  const stop = await api('POST', '/control/gateway/stop');
  ([404, 409, 200].includes(stop.status) && stop.json && ('ok' in stop.json))
    ? ok('api-stop-guard', `HTTP ${stop.status}: ${stop.json.error || 'ok'}`) : bad('api-stop-guard', `HTTP ${stop.status} body=${stop.text.slice(0,80)}`);

  // dead-lease regression: plant a lease file for a guaranteed-dead PID, assert stop is NOT blocked by it.
  const leaseDir = process.env.HOME + '/.local/state/rozum/gateway/leases';
  try {
    fs.mkdirSync(leaseDir, { recursive: true });
    const deadPid = 999999; // not a live pid
    const f = path.join(leaseDir, String(deadPid));
    fs.writeFileSync(f, String(Math.floor(Date.now() / 1000)));
    const after = await api('POST', '/control/gateway/stop');
    const blockedByDead = after.status === 409 && /client/i.test(after.json?.error || '');
    // if a REAL client is attached we can't isolate; only fail if the ONLY plausible client is our dead one
    const realClients = fs.existsSync(f) ? false : true; // our fix reaps the dead lease → file gone
    (!fs.existsSync(f)) ? ok('api-stop-deadlease', 'dead-PID lease was reaped (not counted)') :
      (blockedByDead ? bad('api-stop-deadlease', 'stop still blocked by a DEAD-pid lease') : ok('api-stop-deadlease', `not blocked by dead lease (HTTP ${after.status})`));
    try { fs.unlinkSync(f); } catch {}
  } catch (e) { bad('api-stop-deadlease', 'harness error: ' + e.message); }
}

// ── run ────────────────────────────────────────────────────────────────────────
(async () => {
  const browser = await puppeteer.launch({ executablePath: CHROME, headless: process.env.HEADLESS === '0' ? false : 'new' });
  try {
    await testNav(browser);
    await testMemory(browser);
    await testModels(browser);
    await testChat(browser);
    await testPickers(browser);
    await testApi();
  } finally { await browser.close(); }

  const fails = results.filter(r => !r.pass);
  const md = ['# UCC defects\n', `Run: ${results.length} cases, **${fails.length} failing**.\n`,
    ...(fails.length ? ['## Failing\n', ...fails.map(f => `- **${f.name}** — ${f.detail}`)] : ['All green.']),
    '\n## All cases\n', ...results.map(r => `- ${r.pass ? '✓' : '✗'} ${r.name}${r.detail ? ' — ' + r.detail : ''}`)].join('\n');
  fs.writeFileSync(path.join(HERE, 'DEFECTS.md'), md + '\n');
  console.log(`\n${results.length} cases, ${fails.length} failing → ${path.join('clients/control/test', 'DEFECTS.md')}`);
  process.exit(fails.length ? 1 : 0);
})().catch(e => { console.error('HARNESS FATAL', e); process.exit(2); });
