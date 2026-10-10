/*! NOVA Live — HTML-first partial page updates. Served by NOVA at /_nova/live.js */
(() => {
  'use strict';
  // Loaded twice (e.g. included by two templates)? Initialize once.
  if (window.NovaLive) return;

  const VERSION = '__NOVA_VERSION__';
  const TARGET = /^#[A-Za-z][\w-]*$/;
  const CHANNEL = /^[a-z0-9][a-z0-9._:_-]{0,63}$/;
  const MIN_POLL_MS = 2000;

  const controllers = new Map(); // target id -> AbortController of the newest request
  const latest = new Map();      // target id -> number of the newest request
  const busyForms = new WeakSet();
  let requestCounter = 0;

  const sameOrigin = (url) => url.origin === location.origin;

  function emit(el, name, detail, cancelable = false) {
    return el.dispatchEvent(new CustomEvent('nova:' + name, { bubbles: true, cancelable, detail }));
  }

  /** Only `#id` selectors naming an element that opted in with data-nova-live. */
  function resolveTarget(selector) {
    if (!selector || !TARGET.test(selector)) return null;
    const el = document.getElementById(selector.slice(1));
    return el && el.hasAttribute('data-nova-live') ? el : null;
  }

  function parseDuration(value) {
    const m = /^\s*(\d+(?:\.\d+)?)\s*(ms|s|m)?\s*$/.exec(value || '');
    if (!m) return null;
    const n = parseFloat(m[1]) * ({ ms: 1, s: 1000, m: 60000 }[m[2] || 's']);
    return Math.max(MIN_POLL_MS, n);
  }

  /**
   * New content for `target` from a response. A full page (or any HTML that
   * contains an element with the target's id) contributes that element's
   * children; anything else is treated as the fragment itself. Scripts in
   * fetched HTML never run: they are removed before insertion.
   */
  function extract(html, target) {
    const doc = new DOMParser().parseFromString(html, 'text/html');
    doc.querySelectorAll('script').forEach((s) => s.remove());
    const match = doc.getElementById(target.id);
    const source = match || doc.body;
    return {
      // Copy first: adopting a node removes it from the live childNodes list.
      nodes: Array.from(source.childNodes).map((n) => document.adoptNode(n)),
      title: match && doc.title ? doc.title : null,
    };
  }

  function swap(target, html, info) {
    const { nodes, title } = extract(html, target);
    if (!emit(target, 'before-swap', info, true)) return false;
    const focusInside = target.contains(document.activeElement);
    target.replaceChildren(...nodes);
    if (title && info.push) document.title = title;
    // Keep keyboard and screen-reader users oriented: user-initiated updates
    // (and updates that removed the focused element) move focus to the region.
    if (info.moveFocus || focusInside) {
      if (!target.hasAttribute('tabindex')) target.setAttribute('tabindex', '-1');
      target.focus({ preventScroll: true });
    }
    emit(target, 'after-swap', info);
    syncSubscriptions();
    return true;
  }

  /**
   * Fetch `url` and swap the result into `target`.
   * Resolves to { ok, reason } — reason is set when the caller should fall back.
   */
  async function load(target, url, opts = {}) {
    const id = target.id;
    const method = (opts.method || 'GET').toUpperCase();
    const info = { url: url.href, method, source: opts.source || 'api', push: !!opts.push, moveFocus: !!opts.moveFocus };
    if (!emit(target, 'before-request', info, true)) return { ok: false, reason: 'cancelled' };

    // A newer request for the same region wins: abort the older one.
    controllers.get(id)?.abort();
    const controller = new AbortController();
    controllers.set(id, controller);
    const n = ++requestCounter;
    latest.set(id, n);
    const current = () => latest.get(id) === n;

    target.setAttribute('aria-busy', 'true');
    try {
      const res = await fetch(url.href, {
        method,
        body: opts.body,
        signal: controller.signal,
        credentials: 'same-origin',
        headers: { 'Nova-Live': '1', 'Nova-Target': '#' + id, Accept: 'text/html' },
      });
      if (!current()) return { ok: false, reason: 'stale' };
      const finalUrl = new URL(res.url);
      const type = res.headers.get('content-type') || '';
      if (!sameOrigin(finalUrl)) return { ok: false, reason: 'cross-origin' };
      if (!/^text\/html\b/i.test(type)) return { ok: false, reason: 'not-html' };
      const acceptable = opts.acceptStatus ? opts.acceptStatus(res.status) : res.ok;
      if (!acceptable) return { ok: false, reason: 'status', status: res.status };
      const html = await res.text();
      if (!current()) return { ok: false, reason: 'stale' };
      info.status = res.status;
      info.url = finalUrl.href;
      if (!swap(target, html, info)) return { ok: false, reason: 'cancelled' };
      if (opts.push) pushHistory(target, finalUrl);
      return { ok: true, status: res.status, url: finalUrl };
    } catch (err) {
      if (err.name === 'AbortError') return { ok: false, reason: 'stale' };
      emit(target, 'error', { ...info, error: String(err) });
      return { ok: false, reason: 'network' };
    } finally {
      if (current()) {
        target.removeAttribute('aria-busy');
        controllers.delete(id);
      }
    }
  }

  // ---- History ------------------------------------------------------------

  function pushHistory(target, url) {
    // Remember how to rebuild the entry we are leaving.
    if (!history.state || !history.state.nova) {
      history.replaceState({ nova: { url: location.href, target: target.id } }, '');
    }
    if (url.href !== location.href) {
      history.pushState({ nova: { url: url.href, target: target.id } }, '', url.href);
    }
  }

  addEventListener('popstate', async (e) => {
    const state = e.state && e.state.nova;
    if (!state) return;
    const target = resolveTarget('#' + state.target);
    if (!target) return location.reload();
    const r = await load(target, new URL(state.url, location.href), { source: 'history' });
    if (!r.ok && r.reason !== 'stale') location.reload();
  });

  // ---- Links --------------------------------------------------------------

  document.addEventListener('click', (e) => {
    if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    const link = e.target instanceof Element && e.target.closest('a[data-nova-target][href]');
    if (!link || link.hasAttribute('download') || (link.target && link.target !== '_self')) return;
    const url = new URL(link.href, location.href);
    if (!sameOrigin(url)) return;
    const target = resolveTarget(link.getAttribute('data-nova-target'));
    if (!target) return; // misconfigured: behave like a normal link
    e.preventDefault();
    const push = link.getAttribute('data-nova-history') !== 'false';
    load(target, url, { source: 'link', push, moveFocus: true }).then((r) => {
      // Anything unexpected: fall back to an ordinary page load.
      if (!r.ok && r.reason !== 'stale' && r.reason !== 'cancelled') location.assign(url.href);
    });
  });

  // ---- Forms --------------------------------------------------------------

  document.addEventListener('submit', async (e) => {
    const form = e.target;
    if (e.defaultPrevented || !(form instanceof HTMLFormElement)) return;
    const submitter = e.submitter || null;
    const selector = (submitter && submitter.getAttribute('data-nova-target')) || form.getAttribute('data-nova-target');
    const target = resolveTarget(selector);
    if (!target) return;
    const method = ((submitter && submitter.getAttribute('formmethod')) || form.getAttribute('method') || 'GET').toUpperCase();
    const action = new URL((submitter && submitter.getAttribute('formaction')) || form.getAttribute('action') || location.href, location.href);
    if (!sameOrigin(action) || !['GET', 'POST'].includes(method)) return;

    e.preventDefault();
    if (busyForms.has(form)) return; // duplicate submission
    busyForms.add(form);
    form.setAttribute('aria-busy', 'true');
    const buttons = Array.from(form.querySelectorAll('button, input[type=submit], input[type=image]')).filter((b) => !b.disabled);
    buttons.forEach((b) => { b.disabled = true; });

    const data = new FormData(form, submitter);
    let url = action;
    let body;
    if (method === 'GET') {
      url = new URL(action.href);
      url.search = new URLSearchParams(data).toString();
    } else {
      body = form.enctype === 'multipart/form-data' ? data : new URLSearchParams(data);
    }
    const historyAttr = form.getAttribute('data-nova-history');
    try {
      const r = await load(target, url, {
        method,
        body,
        source: 'form',
        moveFocus: true,
        // GET forms are navigations; POST only updates the URL after a redirect (PRG).
        push: historyAttr !== 'false' && method === 'GET',
        // Validation errors (4xx with HTML) are rendered in place.
        acceptStatus: (s) => s < 500,
      });
      if (r.ok && method === 'POST' && historyAttr !== 'false' && r.url && r.url.href !== action.href) {
        pushHistory(target, r.url);
      }
      if (r.ok && r.status < 300 && form.hasAttribute('data-nova-reset')) form.reset();
      if (!r.ok && r.reason !== 'stale' && r.reason !== 'cancelled') {
        // GET is safe to repeat as a normal navigation; POST is never resubmitted.
        if (method === 'GET') location.assign(url.href);
        else target.setAttribute('data-nova-error', r.reason + (r.status ? ' ' + r.status : ''));
      } else {
        target.removeAttribute('data-nova-error');
      }
    } finally {
      buttons.forEach((b) => { b.disabled = false; });
      form.removeAttribute('aria-busy');
      busyForms.delete(form);
    }
  });

  // ---- Refreshing regions (polling and server events) -----------------------

  function sourceOf(el) {
    const url = new URL(el.getAttribute('data-nova-src') || location.href, location.href);
    return sameOrigin(url) ? url : null;
  }

  function refresh(el) {
    if (!el.id || controllers.has(el.id)) return; // a request is already in flight
    const url = sourceOf(el);
    if (url) load(el, url, { source: 'refresh' });
  }

  const nextPoll = new WeakMap();
  setInterval(() => {
    if (document.hidden) return;
    const now = Date.now();
    document.querySelectorAll('[data-nova-live][data-nova-poll]').forEach((el) => {
      const every = parseDuration(el.getAttribute('data-nova-poll'));
      if (!every || !el.id) return;
      const due = nextPoll.get(el);
      if (due === undefined) { nextPoll.set(el, now + every); return; }
      if (now >= due) {
        nextPoll.set(el, now + every);
        refresh(el);
      }
    });
  }, 500);

  let source = null;
  let subscribed = '';
  const pending = new Map(); // element -> timer, coalesces bursts of events

  function subscribers(channel) {
    return Array.from(document.querySelectorAll('[data-nova-live][data-nova-subscribe]')).filter((el) =>
      channel === '*' || el.getAttribute('data-nova-subscribe').split(/[\s,]+/).includes(channel));
  }

  function onInvalidate(channel) {
    subscribers(channel).forEach((el) => {
      clearTimeout(pending.get(el));
      pending.set(el, setTimeout(() => { pending.delete(el); refresh(el); }, 50));
    });
  }

  /** One EventSource for the union of channels on the page. */
  function syncSubscriptions() {
    if (!window.EventSource) return;
    const channels = new Set();
    document.querySelectorAll('[data-nova-live][data-nova-subscribe]').forEach((el) => {
      el.getAttribute('data-nova-subscribe').split(/[\s,]+/).forEach((c) => { if (CHANNEL.test(c)) channels.add(c); });
    });
    const key = Array.from(channels).sort().join(',');
    if (key === subscribed) return;
    subscribed = key;
    if (source) source.close();
    source = null;
    if (!key) return;
    source = new EventSource('/_nova/live/events?channels=' + encodeURIComponent(key));
    source.addEventListener('invalidate', (e) => onInvalidate(e.data));
    source.addEventListener('reset', () => onInvalidate('*'));
  }

  function init() {
    syncSubscriptions();
    document.documentElement.setAttribute('data-nova-live-ready', VERSION);
  }

  window.NovaLive = Object.freeze({
    version: VERSION,
    /** Programmatic update: NovaLive.load('#region', '/path'). */
    load: (selector, href, opts) => {
      const target = resolveTarget(selector);
      const url = new URL(href, location.href);
      if (!target || !sameOrigin(url)) return Promise.resolve({ ok: false, reason: 'invalid' });
      return load(target, url, { source: 'api', ...(opts || {}) });
    },
    refresh: (selector) => { const t = resolveTarget(selector); if (t) refresh(t); },
  });

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', init);
  else init();
})();
