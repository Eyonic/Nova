// Visit -> deploy a new bundle version -> revisit, in one browser session.
// Reports bytes on the wire for the JS bundle on each visit.
import { chromium } from 'playwright';
import fs from 'node:fs';
const base = process.env.URL, pub = '/site';
const browser = await chromium.launch();
const ctx = await browser.newContext();
const page = await ctx.newPage();
const errors = [];
page.on('pageerror', (e) => errors.push(String(e).slice(0, 120)));
const cdp = await ctx.newCDPSession(page);
await cdp.send('Network.enable');
const seen = {};
cdp.on('Network.responseReceived', (e) => { if (e.response.url.includes('/build/assets/')) seen[e.requestId] = { url: e.response.url.split('/').pop(), enc: e.response.headers['content-encoding'] || e.response.headers['Content-Encoding'] || '-', dict: !!(e.response.headers['use-as-dictionary']) }; });
const done = [];
const sent = [];
page.on('requestfinished', async (r) => { if (r.url().includes('/build/assets/')) { const h = await r.allHeaders(); sent.push({ url: r.url().split('/').pop(), ae: h['accept-encoding'], ad: h['available-dictionary'] || null }); } });
cdp.on('Network.loadingFinished', (e) => { if (seen[e.requestId]) done.push({ ...seen[e.requestId], kb: +(e.encodedDataLength / 1024).toFixed(1) }); });
await page.goto(base + '/', { waitUntil: 'load' });
await page.waitForTimeout(300);
// Deploy v2: same library plus a small change, new fingerprint.
const v1 = fs.readFileSync(pub + '/build/assets/app-AAAA1111.js', 'utf8');
fs.writeFileSync(pub + '/build/assets/app-BBBB2222.js', v1 + '\n;window.__appVersion = 2;');
fs.writeFileSync(pub + '/build/manifest.json', JSON.stringify({ 'src/main.js': { file: 'assets/app-BBBB2222.js', isEntry: true } }));
for (let i = 0; i < 60; i++) { // wait until NOVA's Optimizer published the new bundle
  const r = await fetch(base + '/build/assets/app-BBBB2222.js', { headers: { 'accept-encoding': 'br' } });
  if (r.headers.get('content-encoding') === 'br' && r.headers.get('content-length')) break;
  await new Promise((r) => setTimeout(r, 500));
}
await page.goto(base + '/', { waitUntil: 'load' });
await page.waitForTimeout(300);
const title = await page.title();
const appVersion = await page.evaluate(() => window.__appVersion ?? null);
const fastLoaded = await page.evaluate(() => typeof window.FASTFoundation !== 'undefined' || Object.keys(window).some((k) => k.toLowerCase().includes('fast')));
console.log(JSON.stringify({ visits: done, page_title_after_deploy: title, app_version_running: appVersion, errors, library_loaded: fastLoaded, request_headers: sent }));
console.log('chromium', browser.version());
await browser.close();
