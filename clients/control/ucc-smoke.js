#!/usr/bin/env node
// UCC browser smoke — the bug classes that shipped broken pages on 2026-07-07, as a deploy gate.
// Drives the LIVE deployed page (default http://localhost:8411) in headless Chrome and checks:
//   1. the SPA initializes without page errors (blank-page class: ucc-duplicate-const-fix),
//   2. hash navigation re-renders (BUG-008: compiler/std skew dropped the hashchange hook),
//   3. an in-page button click does NOT warp to #/ (BUG-009: hidden-dialog click-eater)
//      and the agent picker label updates,
//   4. launch-form formBody fields are resolved to NUMERIC bridge ids (BUG-010: by-name
//      signals resolved to nothing and every launch POSTed empty values),
//   5. the sessions table advertises the status column (async-launch feedback surface).
// Read-only: one signal-button click, no launches, no writes.
//
// Soft-skips (exit 0 with a warning) when puppeteer-core, Chrome, or the busi token are
// missing — the deploy still hard-fails on any CHECK failure. UCC_SMOKE=0 skips in deploy.
const fs = require('fs');

let puppeteer;
try { puppeteer = require('puppeteer-core'); }
catch (_e) { console.error('ucc-smoke: puppeteer-core not installed (npm i in clients/control) — SKIPPED'); process.exit(0); }

const CHROME = process.env.UCC_SMOKE_CHROME || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
if (!fs.existsSync(CHROME)) { console.error('ucc-smoke: Chrome not found — SKIPPED'); process.exit(0); }
const BASE = process.env.UCC_SMOKE_BASE || 'http://localhost:8411';
let tok = '';
try { tok = fs.readFileSync(process.env.HOME + '/.busi/tokens.txt', 'utf8').split('\n')[0].trim(); } catch (_e) {}
if (!tok) { console.error('ucc-smoke: no busi token (~/.busi/tokens.txt) — SKIPPED'); process.exit(0); }

(async () => {
  const browser = await puppeteer.launch({ executablePath: CHROME, headless: 'new' });
  const fails = [];
  const ok = (name, cond, extra) => {
    if (cond) console.log(`  ✓ ${name}`);
    else { console.error(`  ✗ ${name}${extra ? ' — ' + extra : ''}`); fails.push(name); }
  };
  try {
    const page = await browser.newPage();
    await page.setViewport({ width: 390, height: 844 });
    const pageErrors = [];
    page.on('pageerror', e => pageErrors.push(e.message));
    await page.setCookie({ name: 'busi_device', value: tok, url: BASE });

    // 1. init
    await page.goto(BASE + '/#/', { waitUntil: 'networkidle2', timeout: 30000 });
    await new Promise(r => setTimeout(r, 1500));
    ok('SPA initializes without page errors', pageErrors.length === 0, pageErrors[0]);

    // 2. navigation re-renders
    await page.goto(BASE + '/#/sessions', { waitUntil: 'networkidle2', timeout: 30000 });
    await new Promise(r => setTimeout(r, 1500));
    const nav = await page.evaluate(() =>
      [...document.querySelectorAll('h2')].some(h => h.offsetParent && /Live sessions|Живые сессии|Живі сесії/.test(h.textContent)));
    ok('hash navigation renders #/sessions', nav);

    // 3. click survives (no hidden-dialog warp) + picker label updates
    const click = await page.evaluate(() => {
      const btn = [...document.querySelectorAll('[data-ssc-set]')].find(el =>
        el.offsetParent && el.getAttribute('data-ssc-set-val') === '"codex"');
      if (!btn) return { found: false };
      btn.click();
      return { found: true, sig: btn.getAttribute('data-ssc-set') };
    });
    await new Promise(r => setTimeout(r, 600));
    const after = await page.evaluate(sig => ({
      hash: location.hash,
      label: document.querySelector(`[data-ssc-text="${sig}"]`)?.textContent || '',
    }), click.sig || '');
    ok('agent-picker click stays on #/sessions', click.found && after.hash === '#/sessions', `hash=${after.hash}`);
    ok('agent-picker click updates the label', after.label === 'codex', `label=${after.label}`);

    // 4. formBody fields resolved to numeric bridge ids
    const fields = await page.evaluate(() => {
      const b = [...document.querySelectorAll('[data-ssc-fetch-body-fields]')].find(el =>
        el.offsetParent && /запустить|launch session/.test(el.textContent));
      return b ? b.getAttribute('data-ssc-fetch-body-fields') : null;
    });
    ok('launch formBody fields resolve to numeric ids',
      !!fields && /^\[\["agent","\d+"\]/.test(fields), String(fields).slice(0, 60));

    // 5. status column present
    const statusCol = await page.evaluate(() =>
      [...document.querySelectorAll('th')].some(th => th.offsetParent && /status|статус/.test(th.textContent)));
    ok('sessions table has the status column', statusCol);
  } finally {
    await browser.close();
  }
  if (fails.length) { console.error(`ucc-smoke: ${fails.length} check(s) FAILED`); process.exit(1); }
  console.log('ucc-smoke: all checks passed');
})().catch(e => { console.error('ucc-smoke FATAL:', e.message); process.exit(1); });
