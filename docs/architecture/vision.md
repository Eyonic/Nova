# NOVA vision: pages that load only what they need

Status: concept and roadmap (October 2026). Nothing in this document is
implemented unless it says **exists**.

Three ideas, one product:

1. **Optimized by default, never locked in.** The server makes every page
   as small and fast as it can without the developer doing anything, and
   every optimization can be inspected, tuned or switched off per site, per
   path or per response.
2. **One container per application.** Site, PHP, background work *and* the
   database ship and run as a single unit.
3. **Live pages.** When data changes, from anywhere, only the parts of the
   page that show that data update, in every open browser.

The building blocks already exist in NOVA: the Optimizer (images, scripts,
precompression, unreferenced-file report), NOVA Live (regions, forms,
polling, SSE invalidation channels), supervised PHP-FPM, tasks and workers,
and per-site databases. The vision connects them.

---

## 1. Load only what the page needs

### What exists

| Area | Today |
|---|---|
| Images | AVIF/WebP negotiation, `?w=` responsive widths, budget warnings (**exists**) |
| Scripts/styles | conservative minification, brotli/zstd/gzip precompression, immutable caching of fingerprinted builds (**exists**) |
| Audit | duplicate and unreferenced files report (**exists**) |

### Next steps (in order of value per effort)

1. **HTML rewriting on the way out** (streaming, e.g. the `lol_html` crate,
   so no full buffering):
   * add `width`/`height` to `<img>` from the known image size (no layout shift),
   * add `loading="lazy"` and `decoding="async"` below the first N images,
   * turn `<img src="/images/x.jpg">` into `srcset` over the configured widths,
   * add `fetchpriority="high"` to the first large image (LCP candidate).
   Every rewrite is idempotent and skipped when the attribute is already set:
   the developer's markup always wins.
2. **Learn what each route uses.** NOVA sees every HTML response, so it can
   record per route which stylesheets, scripts, fonts and images the page
   references. From that:
   * **103 Early Hints / `Link: preload`** for the critical CSS and fonts of
     that route (needs informational responses, see the limits in spec.md),
   * a **per-route weight report** ("/checkout ships 410 KB of JS, 280 KB
     unused by any route"),
   * **unused CSS pruning per route**, opt-in, using the observed class names
     and ids (PurgeCSS-style, safelist via config), served as a separate
     fingerprinted file so the original stays untouched.
3. **Compression dictionaries** (IETF Compression Dictionary Transport, `Use-As-Dictionary` /
   `Available-Dictionary`, `dcb`/`dcz` encodings, supported in Chromium): when
   `app-v42.js` replaces `app-v41.js`, returning visitors download only the
   delta, often 90%+ smaller. The Optimizer already keeps every version's
   precompressed objects, so it has the dictionaries.
4. **Font subsetting** to the characters a site actually uses (with a
   safety margin and per-site opt-out).
5. **Speculation Rules**: emit `<script type="speculationrules">` for
   `data-nova-target`-less same-site links (prefetch on hover), opt-in.

### Developer freedom (applies to every item above)

```toml
[site.optimize]
html_rewrite = true          # global switch per site
lazy_images = "auto"         # "auto" | "off"
prune_css = false            # opt-in only
exclude = ["/admin/**"]      # paths NOVA never touches
```

* `data-nova-keep` on an element: never rewritten.
* PHP response header `Nova-Optimize: off` (stripped before sending):
  that response passes through untouched.
* `/_nova/optimize/status` explains every decision ("img hero.jpg: srcset
  added, 4 widths, saves 380 KB on mobile").

---

## 2. Site and database in one container

Today the database is a second Compose service. The vision is
`docker run nova` with everything inside, while keeping the external option.

### Design

```toml
[services.database.main]
driver = "mariadb"
embedded = true              # NOVA starts and supervises mariadbd itself
data_dir = "/var/lib/nova/db"
memory = "256MiB"            # innodb_buffer_pool_size etc. derived from this
```

* `nova serve` (the root supervisor) starts `mariadbd` under its own uid,
  inside a Landlock sandbox (write: its data dir and socket only; no TCP
  listen by default, unix socket only), exactly like a PHP-FPM master.
* Per-site database + user are created on start from `[site.database]`
  (what `docker/mariadb/init/01-sites.sh` does today), passwords generated
  and stored in the state volume when no `password_env` is given.
* Readiness (`/_nova/health/ready`) already checks databases.
* Graceful shutdown order: stop accepting → drain HTTP → stop workers/tasks
  → stop PHP → `mariadbd` clean shutdown.
* **Backups built in**: `nova backup` (logical dump per site, or
  `mariadb-backup` for the whole instance) to the state volume or an
  S3-compatible bucket, plus a scheduled `[[backup]]` entry using the
  existing cron engine.
* **SQLite as the zero-config tier**: `driver = "sqlite"` gives each site a
  file in its own state dir (already private to the site uid and sandbox).
  Good for small sites; same live features (section 3) via the SQLite
  update hook in PHP or WAL watching.
* External databases stay supported (`embedded = false`, today's behaviour)
  for scaling out.

Trade-offs to accept consciously: a bigger image (~+200 MB for MariaDB),
database upgrades now happen with NOVA upgrades (pin the MariaDB major in
the image tag), and one container is one failure domain (fine for the
single-node target; use external DB for HA).

---

## 3. Live pages driven by the database

### What exists

NOVA Live (**exists**): regions marked `data-nova-live`, links and forms
that update one region, polling, and `data-nova-subscribe="channel"`
regions that refetch when PHP sends `Nova-Publish: channel`. The event
stream carries **channel names only**; every browser refetches through the
normal request path with its own session, so authorization is never
bypassed.

The gap: only changes made *during a PHP request that remembers to send the
header* reach browsers. Changes by cron tasks, queue workers, an admin tool
or another app are invisible.

### Database change channels

```html
<ul id="orders" data-nova-live data-nova-subscribe="db:orders"> … </ul>
<span id="stock-42" data-nova-live data-nova-subscribe="db:products#42"> … </span>
```

* With the embedded database (section 2) NOVA reads the MariaDB **row-based
  binlog** as a replication client (e.g. `mysql_async`'s binlog stream). Each
  committed row event becomes an invalidation on channel `db:<table>` and,
  when the primary key is a single column, `db:<table>#<id>`.
* Channels are scoped to the site that owns the database (the binlog event
  carries the schema name), so site A can never observe site B's changes.
* Only *names* travel to the browser, never row data: no new data-leak path.
* **Coalescing**: bursts (an import touching 10 000 rows) are merged per
  channel over a short window (default 150 ms), and table-level channels are
  rate limited, so one bulk update causes one refresh.
* **Thundering herd protection**: when 2 000 browsers refetch the same
  region at once, identical requests without cookies/authorization are
  answered from a 1-second micro-cache; personalized requests go to PHP
  as today, spread with a small random delay in the client.
* Without the embedded database: a tiny PHP helper
  (`nova_publish('db:orders')`) and a documented trigger-based fallback.

### Smoother updates

* **DOM morphing** instead of replacing a region (idiomorph-style): keeps
  focus, caret, scroll position, open `<details>` and video state.
* **View Transitions** for region swaps when the browser supports them.
* **Prefetch on hover** for `data-nova-target` links.
* **Server-side includes for regions** (`<nova-region src="/fragments/cart">`):
  the shell page can be cached aggressively while regions stay dynamic
  (the "islands" model, without a JavaScript framework).

---

## Roadmap

| Phase | Deliverable | Depends on |
|---|---|---|
| A | HTML rewriting: img dimensions, lazy, srcset, fetchpriority; `Nova-Optimize: off`; status explanations | — |
| B | Embedded MariaDB supervisor, per-site provisioning, `nova backup` | — |
| C | Binlog → `db:` channels, coalescing, micro-cache for refetch storms | B |
| D | DOM morphing, View Transitions, prefetch in `live.js` | — |
| E | Per-route usage learning, Early Hints, weight report | A |
| F | Compression dictionaries, CSS pruning (opt-in), font subsetting | E |
| G | SQLite driver with the same live channels | C |

Each phase ships with integration checks in `tests/integration/run.sh` and
browser tests in `tests/browser/` (as NOVA Live does today), and a
before/after measurement in `tests/performance/` (see the performance
baseline in the comparison report).
