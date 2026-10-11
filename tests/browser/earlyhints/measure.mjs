// Navigate to the page N times (fresh context, empty cache) over HTTP/2 on
// a throttled connection and report TTFB, first contentful paint, when the
// stylesheet finished, and load. With early_hints the stylesheet download
// overlaps the server's 200 ms instead of starting after the HTML arrives.
import { chromium } from 'playwright';
const url = process.env.URL;
const runs = Number(process.env.RUNS || 5);
const latency = Number(process.env.LATENCY || 80);
// The test certificate is trusted by its key (not ignored as an error:
// Chrome processes no Early Hints on connections with certificate errors).
const browser = await chromium.launch({ args: [`--ignore-certificate-errors-spki-list=${process.env.SPKI}`] });
const out = [];
for (let i = 0; i < runs; i++) {
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  const cdp = await ctx.newCDPSession(page);
  await cdp.send('Network.enable');
  await cdp.send('Network.emulateNetworkConditions', {
    offline: false, latency, downloadThroughput: (20e6 / 8), uploadThroughput: (5e6 / 8) });
  let early = 0;
  cdp.on('Network.responseReceivedEarlyHints', () => { early++; });
  await page.goto(url, { waitUntil: 'load', timeout: 60000 });
  const m = await page.evaluate(() => {
    const nav = performance.getEntriesByType('navigation')[0];
    const fcp = performance.getEntriesByName('first-contentful-paint')[0];
    const css = performance.getEntriesByType('resource').find((e) => e.name.endsWith('/eh/app.css'));
    return { protocol: nav.nextHopProtocol, ttfb: nav.responseStart, fcp: fcp ? fcp.startTime : null,
      css_start: css ? css.startTime : null, css_init: css ? css.initiatorType : null,
      css_end: css ? css.responseEnd : null, load: nav.loadEventEnd };
  });
  out.push({ ...m, early_hints: early });
  if (process.env.DEBUG) console.log(JSON.stringify(out[out.length - 1]));
  await ctx.close();
}
await browser.close();
const med = (k) => { const v = out.map((o) => o[k]).sort((a, b) => a - b); return Math.round(v[Math.floor(v.length / 2)]); };
console.log(JSON.stringify({ protocol: out[0].protocol, runs, latency_ms: latency, early_hints_seen: out.filter((o) => o.early_hints).length,
  ttfb_ms: med('ttfb'), fcp_ms: med('fcp'), css_done_ms: med('css_end'), load_ms: med('load') }));
