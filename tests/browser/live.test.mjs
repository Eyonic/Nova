// NOVA Live browser tests: real Chromium against a running NOVA stack.
import { chromium } from 'playwright';

const BASE = `http://showcase.localhost:${process.env.NOVA_PORT || 8088}`;
let passed = 0;
let failed = 0;

async function test(name, fn) {
  try {
    await fn();
    passed++;
    console.log(`  \x1b[32m✓\x1b[0m ${name}`);
  } catch (e) {
    failed++;
    console.log(`  \x1b[31m✗\x1b[0m ${name}\n      ${String(e.message || e).split('\n')[0]}`);
  }
}
const assert = (cond, msg) => { if (!cond) throw new Error(msg); };

const browser = await chromium.launch();
const context = await browser.newContext();

/** Open the demo and plant a marker that only survives if the page never reloads. */
async function open(path = '/live.php') {
  const page = await context.newPage();
  await page.goto(BASE + path);
  await page.waitForSelector('html[data-nova-live-ready]');
  await page.evaluate(() => { window.__noReload = true; });
  return page;
}
const noReload = (page) => page.evaluate(() => window.__noReload === true);
const productsText = (page) => page.locator('#products').innerText();

console.log('\nNOVA Live (Chromium)');

await test('link updates only the target region, URL follows, no reload', async () => {
  const page = await open();
  await page.click('nav.pills a:has-text("Decor")');
  await page.waitForFunction(() => document.querySelector('#products').innerText.includes('Showing decor'));
  assert(await noReload(page), 'page reloaded');
  assert(new URL(page.url()).search === '?category=decor', 'URL not updated: ' + page.url());
  const text = await productsText(page);
  assert(text.includes('Coral vase') && !text.includes('Aurora lamp'), 'wrong products: ' + text);
  await page.close();
});

await test('Back and Forward restore region content without reloading', async () => {
  const page = await open();
  await page.click('nav.pills a:has-text("Lighting")');
  await page.waitForFunction(() => document.querySelector('#products').innerText.includes('Showing lighting'));
  await page.click('nav.pills a:has-text("Textiles")');
  await page.waitForFunction(() => document.querySelector('#products').innerText.includes('Showing textiles'));
  await page.goBack();
  await page.waitForFunction(() => document.querySelector('#products').innerText.includes('Showing lighting'));
  await page.goBack();
  await page.waitForFunction(() => document.querySelector('#products').innerText.includes('Showing all'));
  await page.goForward();
  await page.waitForFunction(() => document.querySelector('#products').innerText.includes('Showing lighting'));
  assert(await noReload(page), 'history navigation reloaded the page');
  await page.close();
});

await test('a slow older response never overwrites a newer one', async () => {
  const page = await open();
  await page.route('**/live.php?category=lighting', async (route) => {
    await new Promise((r) => setTimeout(r, 1500));
    await route.continue().catch(() => {});
  });
  await page.click('nav.pills a:has-text("Lighting")');
  await page.click('nav.pills a:has-text("Textiles")');
  await page.waitForFunction(() => document.querySelector('#products').innerText.includes('Showing textiles'));
  await page.waitForTimeout(2000);
  const text = await productsText(page);
  assert(text.includes('Showing textiles'), 'stale response won: ' + text);
  await page.close();
});

await test('requests identify themselves with Nova-Live and Nova-Target headers', async () => {
  const page = await open();
  const req = page.waitForRequest((r) => r.url().includes('category=decor'));
  await page.click('nav.pills a:has-text("Decor")');
  const h = (await req).headers();
  assert(h['nova-live'] === '1' && h['nova-target'] === '#products', JSON.stringify(h));
  await page.close();
});

await test('validation errors render in place (422), focus moves to the region', async () => {
  const page = await open();
  await page.fill('#note', '');
  await page.click('#note-form button[type=submit]');
  await page.waitForSelector('#note-errors');
  assert((await page.innerText('#note-errors')).includes('Write something first'), 'no error message');
  assert(await page.evaluate(() => document.activeElement && document.activeElement.id === 'note-form'), 'focus not moved');
  assert(await noReload(page), 'page reloaded');
  await page.close();
});

await test('duplicate submissions are prevented while a post is in flight', async () => {
  const page = await open();
  let posts = 0;
  await page.route('**/live.php', async (route) => {
    if (route.request().method() === 'POST') {
      posts++;
      await new Promise((r) => setTimeout(r, 800));
    }
    await route.continue();
  });
  await page.fill('#note', 'double click test');
  await page.evaluate(() => {
    const form = document.querySelector('#note-form form');
    form.requestSubmit();
    form.requestSubmit();
    form.requestSubmit();
  });
  await page.waitForFunction(() => document.querySelector('#notes').innerText.includes('double click test'), null, { timeout: 5000 });
  assert(posts === 1, `expected 1 POST, saw ${posts}`);
  await page.close();
});

await test('a post in one tab updates another tab via server events', async () => {
  const other = await open();
  const page = await open();
  const note = 'live ' + Date.now();
  await page.fill('#note', note);
  await page.click('#note-form button[type=submit]');
  // Same tab: form reset after success, list refreshed.
  await page.waitForFunction((n) => document.querySelector('#notes').innerText.includes(n), note);
  assert((await page.inputValue('#note')) === '', 'form not reset');
  // Other tab: only an invalidation arrives over SSE; it refetches the region itself.
  await other.waitForFunction((n) => document.querySelector('#notes').innerText.includes(n), note, { timeout: 5000 });
  assert(await noReload(other), 'other tab reloaded');
  await other.close();
  await page.close();
});

await test('polling refreshes a region periodically', async () => {
  const page = await open();
  const before = await page.innerText('#clock .big');
  await page.waitForFunction((b) => document.querySelector('#clock .big').innerText !== b, before, { timeout: 9000 });
  assert(await noReload(page), 'page reloaded');
  await page.close();
});

await test('every node of a multi-element fragment is kept (regression)', async () => {
  const page = await open();
  await page.route('**/live.php?category=decor', (route) =>
    route.fulfill({ contentType: 'text/html', body: '<p>a</p>\n<p>b</p>\n<p>c</p>\n<ul><li>d</li></ul>' }));
  await page.click('nav.pills a:has-text("Decor")');
  await page.waitForFunction(() => document.querySelector('#products li'));
  const n = await page.evaluate(() => document.querySelectorAll('#products p').length);
  assert(n === 3, `expected 3 paragraphs, got ${n}`);
  await page.close();
});

await test('scripts inside fetched HTML never execute', async () => {
  const page = await open();
  await page.route('**/live.php?category=decor', (route) =>
    route.fulfill({
      contentType: 'text/html',
      body: '<p>safe</p><script>window.__pwned = 1</script><img src=x onerror="">',
    }));
  await page.click('nav.pills a:has-text("Decor")');
  await page.waitForFunction(() => document.querySelector('#products').innerText.includes('safe'));
  assert(await page.evaluate(() => window.__pwned === undefined), 'script executed');
  assert(await page.evaluate(() => document.querySelectorAll('#products script').length === 0), 'script element inserted');
  await page.close();
});

/** Whether NOVA Live intercepted a click (we cancel any real navigation afterwards). */
async function intercepted(page, html) {
  return page.evaluate((html) => {
    const holder = document.createElement('div');
    holder.innerHTML = html;
    document.body.append(holder);
    let prevented = null;
    const spy = (e) => { prevented = e.defaultPrevented; e.preventDefault(); };
    window.addEventListener('click', spy);
    holder.querySelector('a').click();
    window.removeEventListener('click', spy);
    holder.remove();
    return prevented;
  }, html);
}

await test('cross-origin links and non-opted-in targets are left alone', async () => {
  const page = await open();
  assert(await intercepted(page, '<a href="/live.php?category=decor" data-nova-target="#products">ok</a>') === true, 'eligible link not enhanced');
  assert(await intercepted(page, '<a href="http://localhost:9/x" data-nova-target="#products">x</a>') === false, 'cross-origin link intercepted');
  assert(await intercepted(page, '<a href="/live.php" data-nova-target="#clock-missing">x</a>') === false, 'missing target intercepted');
  assert(await intercepted(page, '<a href="/live.php" data-nova-target="body">x</a>') === false, 'non-id selector intercepted');
  assert(await intercepted(page, '<a href="/live.php" data-nova-target="#products" target="_blank">x</a>') === false, 'target=_blank intercepted');
  // An element without data-nova-live is never a valid target.
  await page.evaluate(() => { const d = document.createElement('div'); d.id = 'plain'; document.body.append(d); });
  assert(await intercepted(page, '<a href="/live.php" data-nova-target="#plain">x</a>') === false, 'non-live target intercepted');
  await page.close();
});

await test('a non-HTML response falls back to a normal page load', async () => {
  const page = await open();
  await page.route('**/live.php?category=decor', (route, req) =>
    req.headers()['nova-live'] ? route.fulfill({ contentType: 'application/json', body: '{}' }) : route.continue());
  await page.click('nav.pills a:has-text("Decor")');
  await page.waitForFunction(() => window.__noReload === undefined, null, { timeout: 5000 });
  assert(new URL(page.url()).search === '?category=decor', 'did not navigate');
  await page.close();
});

await test('loading the runtime twice initializes it once', async () => {
  const page = await open();
  await page.evaluate(() => new Promise((resolve) => {
    const s = document.createElement('script');
    s.src = '/_nova/live.js?again';
    s.onload = resolve;
    document.head.append(s);
  }));
  let fetches = 0;
  page.on('request', (r) => { if (r.url().includes('category=decor')) fetches++; });
  await page.click('nav.pills a:has-text("Decor")');
  await page.waitForFunction(() => document.querySelector('#products').innerText.includes('Showing decor'));
  await page.waitForTimeout(300);
  assert(fetches === 1, `expected 1 request, saw ${fetches}`);
  await page.close();
});

await test('without JavaScript everything works as normal links and forms', async () => {
  const ctx = await browser.newContext({ javaScriptEnabled: false });
  const page = await ctx.newPage();
  await page.goto(BASE + '/live.php');
  await page.click('nav.pills a:has-text("Decor")');
  await page.waitForURL('**/live.php?category=decor');
  assert((await page.title()).includes('decor'), 'full page not served');
  assert((await productsText(page)).includes('Coral vase'), 'products missing');
  await page.fill('#note', 'no-js note');
  await page.click('#note-form button[type=submit]');
  await page.waitForURL('**/live.php');
  assert((await page.innerText('#notes')).includes('no-js note'), 'note not saved via normal post');
  await ctx.close();
});

await browser.close();
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
