// Measures what NOVA serves for each image under different Accept headers.
const PROBES = [
  { label: 'Full width, AVIF browser', accept: 'image/avif,image/webp,*/*', w: null },
  { label: 'Full width, WebP browser', accept: 'image/webp,*/*', w: null },
  { label: 'Full width, legacy browser', accept: '*/*', w: null },
  { label: '640px, AVIF browser', accept: 'image/avif,*/*', w: 640 },
  { label: '320px, WebP browser', accept: 'image/webp,*/*', w: 320 },
];
const kb = (b) => `${(b / 1024).toFixed(b < 10240 ? 1 : 0)} KB`;

async function probe(url, p) {
  const u = p.w ? `${url}?w=${p.w}` : url;
  const r = await fetch(u, { headers: { Accept: p.accept }, cache: 'no-store' });
  const blob = await r.blob();
  const type = (r.headers.get('content-type') || '').replace('image/', '');
  return { type, bytes: blob.size };
}

async function measureCard(card) {
  const url = card.dataset.url;
  const orig = Number(card.dataset.bytes);
  const body = card.querySelector('.measure tbody');
  const rows = [];
  for (const p of PROBES) {
    try {
      const { type, bytes } = await probe(url, p);
      const isOrig = !p.w && bytes === orig;
      const cls = isOrig ? 'original' : type;
      const saved = Math.max(0, 1 - bytes / orig);
      rows.push(`<tr><td>${p.label}</td><td><span class="fmt ${cls}">${isOrig ? 'original' : type}</span></td>
        <td><span class="saving" style="width:${Math.round(saved * 60)}px"></span>${kb(bytes)}</td></tr>`);
    } catch (e) {
      rows.push(`<tr><td>${p.label}</td><td colspan="2" class="muted">failed</td></tr>`);
    }
  }
  body.innerHTML = rows.join('');
}

function browserTotals() {
  // What the <img> tags on this page actually cost (same-origin, so sizes are visible).
  const entries = performance.getEntriesByType('resource').filter((e) => e.initiatorType === 'img');
  let total = 0;
  for (const card of document.querySelectorAll('.card')) {
    const img = card.querySelector('.frame img');
    const entry = entries.find((e) => e.name === img.currentSrc);
    const got = card.querySelector('.got');
    if (entry && entry.encodedBodySize) {
      const orig = Number(card.dataset.bytes);
      got.textContent = `Your browser: ${kb(entry.encodedBodySize)} for ${new URL(img.currentSrc).search || 'full size'} (${Math.round((1 - entry.encodedBodySize / orig) * 100)}% smaller than the original)`;
    } else if (!img.complete) {
      got.textContent = 'Your browser: not loaded yet (lazy)';
    }
  }
  for (const e of entries) total += e.encodedBodySize || 0;
  const orig = [...document.querySelectorAll('.card')].reduce((s, c) => s + Number(c.dataset.bytes), 0);
  document.getElementById('dl-total').textContent = kb(total);
  document.getElementById('dl-saved').textContent = total ? `${Math.round((1 - total / orig) * 100)}%` : '…';
}

async function measureAll() {
  document.querySelectorAll('.measure tbody').forEach((b) => (b.innerHTML = '<tr><td colspan="3" class="muted">measuring…</td></tr>'));
  await Promise.all([...document.querySelectorAll('.card')].map(measureCard));
  browserTotals();
}

document.getElementById('remeasure').addEventListener('click', measureAll);
window.addEventListener('load', () => { measureAll(); setTimeout(browserTotals, 1500); });
