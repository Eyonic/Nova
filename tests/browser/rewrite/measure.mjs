// Load the gallery per viewport on a throttled "Fast 4G" connection and
// report image bytes/requests (Resource Timing), LCP, CLS and load time.
import { chromium } from 'playwright';
const url = process.env.URL;
const views = {
  desktop: { viewport: { width: 1280, height: 800 }, deviceScaleFactor: 1 },
  phone: { viewport: { width: 390, height: 844 }, deviceScaleFactor: 3, isMobile: true },
};
const images = () => performance.getEntriesByType('resource')
  .filter((e) => e.initiatorType === 'img')
  .reduce((a, e) => ({ n: a.n + 1, kb: a.kb + e.encodedBodySize / 1024 }), { n: 0, kb: 0 });
const browser = await chromium.launch();
for (const [name, opts] of Object.entries(views)) {
  const ctx = await browser.newContext(opts);
  const page = await ctx.newPage();
  const cdp = await ctx.newCDPSession(page);
  await cdp.send('Network.enable');
  await cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
  await cdp.send('Network.emulateNetworkConditions', {
    offline: false, latency: 150, downloadThroughput: (9e6 / 8), uploadThroughput: (1.5e6 / 8) });
  await page.addInitScript(() => {
    window.__cls = 0; window.__lcp = 0;
    new PerformanceObserver((l) => { for (const e of l.getEntries()) if (!e.hadRecentInput) window.__cls += e.value; })
      .observe({ type: 'layout-shift', buffered: true });
    new PerformanceObserver((l) => { const e = l.getEntries(); window.__lcp = e[e.length - 1].startTime; })
      .observe({ type: 'largest-contentful-paint', buffered: true });
  });
  const t0 = Date.now();
  await page.goto(url, { waitUntil: 'load', timeout: 120000 });
  const loadMs = Date.now() - t0;
  const atLoad = await page.evaluate(images);
  const lcp = await page.evaluate(() => window.__lcp);
  // Read the whole page: lazy images load as they come into view.
  await page.evaluate(async () => { for (let y = 0; y < document.body.scrollHeight; y += 400) { window.scrollTo(0, y); await new Promise((r) => setTimeout(r, 150)); } });
  await page.waitForLoadState('networkidle');
  const total = await page.evaluate(images);
  const cls = await page.evaluate(() => window.__cls);
  console.log(JSON.stringify({ view: name, load_s: +(loadMs / 1000).toFixed(2), lcp_s: +(lcp / 1000).toFixed(2), cls: +cls.toFixed(3),
    images_at_load: atLoad.n, kb_at_load: Math.round(atLoad.kb), images_total: total.n, kb_total: Math.round(total.kb) }));
  await ctx.close();
}
await browser.close();
