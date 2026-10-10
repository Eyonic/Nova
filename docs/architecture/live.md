# NOVA Live

Partial page updates for ordinary HTML and PHP sites, without a frontend
framework. Opt-in per element; pages without NOVA Live attributes behave
exactly as before, and every enhanced link or form still works without
JavaScript.

Status legend: **implemented** (tested), *planned*.

## Using it

```html
<script src="/_nova/live.js" defer></script>

<!-- A region that may be replaced. Needs data-nova-live and an id. -->
<div id="products" data-nova-live aria-live="polite"> … </div>

<!-- Same-origin links/forms that update a region instead of the page. -->
<a href="/products?category=lamps" data-nova-target="#products">Lamps</a>

<form method="post" action="/notes" data-nova-target="#note-form" data-nova-reset> … </form>

<!-- Live data: polling, and refresh on server events. -->
<div id="clock" data-nova-live data-nova-poll="5s" data-nova-src="/clock"> … </div>
<div id="notes" data-nova-live data-nova-subscribe="notes"> … </div>
```

| Attribute | On | Meaning | Status |
|---|---|---|---|
| `data-nova-live` | region | may be replaced; required for every target | implemented |
| `data-nova-target="#id"` | `a`, `form`, submit button | update that region instead of navigating | implemented |
| `data-nova-history="false"` | link, form | don't push a history entry | implemented |
| `data-nova-reset` | form | reset after a 2xx response | implemented |
| `data-nova-poll="10s"` | region | refetch periodically (min 2 s, paused in hidden tabs) | implemented |
| `data-nova-src="/url"` | region | URL to refetch for poll/subscribe (default: current page) | implemented |
| `data-nova-subscribe="a b"` | region | refetch when the server publishes one of these channels | implemented |

JavaScript API: `NovaLive.load('#id', '/url')`, `NovaLive.refresh('#id')`.
Events (bubbling, on the region): `nova:before-request` and `nova:before-swap`
(cancelable), `nova:after-swap` (initialize widgets here), `nova:error`.

## Protocol

**Requests.** The runtime sends `Nova-Live: 1` and `Nova-Target: #id`,
with the page's cookies (`credentials: same-origin`). They pass through the
normal dispatcher and PHP unchanged; NOVA adds nothing that could bypass
application authentication, authorization or CSRF checks.

**Responses.** PHP may:

* return the full page: the runtime takes the children of the element with
  the target's id, or
* check `$_SERVER['HTTP_NOVA_LIVE']` / `HTTP_NOVA_TARGET` and return just the
  fragment (an optimization, never required).

NOVA appends `Vary: Nova-Live, Nova-Target` to PHP responses so caches never
mix fragments and full pages.

**Status handling.** Links, polling and history accept 2xx only; anything
else falls back (links do a normal navigation, polling keeps the old
content). Forms render 2xx and 4xx in place (validation errors, e.g. 422);
on 5xx or network failure a GET form navigates normally and a POST is
**never** resubmitted (the region gets `data-nova-error`).
Redirects are followed; after Post/Redirect/Get the URL is updated.

**Server events.** `GET /_nova/live/events?channels=a,b` (per site, by Host)
is a Server-Sent Events stream served by NOVA itself:

```text
retry: 3000
: connected

event: invalidate
data: notes

: ping
```

PHP publishes by sending a response header on a successful (< 400) response:

```php
header('Nova-Publish: notes');          // comma-separated channels
```

NOVA strips the header, and every subscribed browser on **that site**
refetches its regions through the normal request path. The stream carries
channel names only, never content: each viewer receives exactly what PHP
renders for their own session. No PHP worker is held per connected browser.
`event: reset` (after a subscriber falls behind) refreshes all subscribed
regions.

## Database change channels (experimental branch)

Changes made **anywhere** (PHP, cron tasks, queue workers, an admin tool,
another application, plain SQL) can refresh regions, without PHP sending
`Nova-Publish`:

```html
<ul id="orders" data-nova-live data-nova-subscribe="db:orders"> … </ul>
```

```toml
[services.database.main.changes]
user = "nova_cdc"                  # GRANT REPLICATION SLAVE, BINLOG MONITOR ON *.*
password_env = "NOVA_DB_CDC_PASSWORD"
```

MariaDB needs a row-based binary log:
`--log-bin=mariadb-bin --server-id=1 --binlog-format=ROW
--binlog-row-image=MINIMAL --binlog-expire-logs-seconds=86400
--max-binlog-total-size=1G`.

* NOVA follows the binary log as a replication client. Each committed
  change to table `T` in a site's database publishes `db:<t>` (lowercase)
  to **that site only**; other databases are ignored.
* Changes are taken at the commit marker: rolled-back transactions publish
  nothing. Changes are batched every 100 ms (a bulk update is one refresh).
* The site's micro-cache is cleared on every change, so cached pages never
  outlive their data.
* Only table names are used; the stream still carries channel names only.
* Measured on Unraid (WordPress, `UPDATE` from the mysql client): commit to
  browser event average 68 ms, maximum 102 ms.
* Prototype limitation: the reader runs inside the HTTP worker, and a
  replication login can read every database's changes. A production
  version should run it as its own sandboxed process that only emits table
  names.

## Safety rules (all implemented and tested)

* Only `#id` targets that carry `data-nova-live`; anything else is a normal link.
* Same-origin only: links, form actions, `data-nova-src`, and the final URL
  after redirects. Only `text/html` responses are swapped.
* `<script>` elements in fetched HTML are removed and never run.
* A newer request for a region aborts the older one; stale responses are discarded.
* Forms ignore re-submission while a request is in flight.
* The runtime initializes once even if included twice; listeners are
  delegated at the document, so swapped-in content needs no re-binding.
* Focus moves to the updated region after user-initiated updates
  (`tabindex=-1` added when needed); `aria-busy` is set during requests.
* Channels: `[a-z0-9][a-z0-9._:-]{0,63}`, scoped per site; limits
  `live.max_connections` (1024), `live.max_connections_per_ip` (16),
  `live.max_channels` (16); heartbeats every `live.heartbeat_secs`.
* Event streams close on shutdown so graceful drain is not held up.

## Tests

* `tests/browser/live.test.mjs` (headless Chromium, `tests/browser/run.sh`):
  14 tests: region swap without reload, Back/Forward, stale-response race,
  request headers, 422 validation + focus, duplicate-submit prevention,
  cross-tab update via SSE, polling, multi-node fragments, script stripping,
  cross-origin and non-opted-in targets ignored, non-HTML fallback,
  double initialization, full functionality with JavaScript disabled.
* `tests/integration/run.sh` "NOVA Live" section: runtime serving and
  caching, fragment responses, `Vary`, channel validation, CSRF and
  validation in the demo, publish header stripped, cross-site channel
  isolation, metrics, prompt shutdown with open streams.
* Unit tests in `crates/nova-core/src/live.rs`: channel parsing, site and
  channel filtering, connection limits and slot release, close.

## Planned

* *WebSockets* for genuinely bidirectional, low-latency cases.
* *Publishing outside a request* (CLI / cron: `nova publish <site> <channel>`).
* *Morphing* (preserve focus/selection inside a replaced region) instead of
  replacing children.
* *Out-of-band updates* (one response updating several regions).
* *Per-site live limits* in `[site.live]`.

Demo: `http://showcase.localhost:8088/live.php` (`sites/showcase/public/live.php`).
