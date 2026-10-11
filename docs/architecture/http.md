# HTTP, delivery and operations

Everything between the network and a site: protocols, TLS, proxies,
compression, caching, per-site rules, limits, background processes and
reloads. Configuration lives in `config/nova.toml`; every key below is
optional unless stated otherwise.

## Protocols and TLS

| Listener | Protocols |
|---|---|
| `server.listen` (TCP) | HTTP/1.1, HTTP/2 (prior knowledge) |
| `server.tls.listen` (TCP) | HTTPS: HTTP/1.1 and HTTP/2 via ALPN |
| `server.tls.listen` (UDP) | HTTP/3 over QUIC, advertised with `Alt-Svc` |

```toml
[server.tls]
enabled = true
listen = "0.0.0.0:8443"
public_port = 443               # port clients use (redirects, Alt-Svc)
http3 = true
acme = true                     # Let's Encrypt
acme_email = "ops@example.com"
acme_challenge = "tls-alpn-01"  # or "http-01": validated on port 80 (server.listen),
                                # works behind CDNs/load balancers that terminate TLS
# acme_directory = "https://ca.internal:9000/acme/acme/directory"   # private CA (step-ca)
# acme_ca_file = "/etc/nova/ca/root.pem"                            # its root certificate
self_signed = true              # fallback for local names and while ACME is pending
hsts_max_age_secs = 31536000

[[server.tls.cert]]             # certificates managed elsewhere
hosts = ["*.example.org"]
cert = "/etc/nova/certs/example.org.pem"
key = "/etc/nova/certs/example.org.key"
```

TLS 1.2 and 1.3 via rustls with the aws-lc-rs provider. Key exchange
prefers the post-quantum hybrid `X25519MLKEM768` (as Chrome, Firefox and
Cloudflare do) and falls back to X25519/ECDHE for older clients. It costs
about 11% of new-handshake throughput, nothing on established connections.

With `http-01`, NOVA answers `/.well-known/acme-challenge/<token>` on the
plain listener before redirects, auth and rate limits, so port 80 must
reach `server.listen`. A private ACME directory on another port is
reachable from the sandboxed worker (Landlock allows the directory's port).

Certificate per SNI host: configured files first, then ACME for public names
(not `localhost`, `*.localhost`, `*.test`, `*.local`, IPs ...), then a
self-signed certificate persisted in `<state>/tls/self-signed/` so browsers
keep their exception across restarts. ACME account keys and certificates are
cached in `<state>/tls/acme/` (worker-owned, mode 0700).

HSTS and the automatic HTTP→HTTPS redirect apply only to hosts with a
publicly trusted certificate (ACME or configured files); a self-signed
`localhost` is never pinned to HTTPS.

## Proxies and the real client

```toml
[server]
trusted_proxies = ["10.0.0.5"]          # the proxies themselves; default: none
forwarded_header = "x-forwarded-for"    # or "forwarded" (RFC 7239); only this one is read
proxy_protocol = false                  # PROXY v1/v2 header; only trusted_proxies may connect
admin_allow = ["127.0.0.0/8", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "::1/128", "fc00::/7"]
```

The client IP is the first untrusted address walking the configured header
from the nearest hop; forged entries further left are ignored, and an
entry that is not an address (`unknown`, an obfuscated `Forwarded` name)
stops the walk at the last proxy that vouched for it. The scheme comes from
the same hop (`X-Forwarded-Proto` is paired per hop when it lists one value
per address, otherwise the nearest proxy's value counts). PHP receives the
client as `REMOTE_ADDR`, plus `HTTPS=on` when it used HTTPS at the edge.
`/_nova/metrics` and `/_nova/optimize/status` answer only clients in
`admin_allow` (404 otherwise); health endpoints stay public.

Trust is deliberately narrow:

* Only the header named by `forwarded_header` is read. A proxy that
  maintains `X-Forwarded-For` passes a client-sent `Forwarded` through
  untouched (and vice versa), so reading both would let clients choose.
* List proxy addresses, not networks. With Docker port publishing, clients
  may appear to come from the bridge gateway (`172.17.0.1`); trusting
  `172.16.0.0/12` would make every client a "proxy" that can claim any
  address, including one in `admin_allow`. `0.0.0.0/0` is rejected.
* With `proxy_protocol = true`, connections from peers outside
  `trusted_proxies` are closed before anything is read, and the setting
  requires `trusted_proxies`.

## Compression

| Source | When |
|---|---|
| Script Optimizer objects | text assets in the document root, precompressed in the background (brotli 11, zstd 19, gzip 9; lighter levels above 512 KiB) |
| `file.br` / `file.zst` / `file.gz` next to a static file | build output that ships its own precompressed files (must be at least as new as the original) |
| On the fly (zstd 3, brotli 5, gzip 6) | PHP output, error pages and anything else compressible |

Stored and precompressed objects prefer brotli (best ratio, cost paid once).
On the fly NOVA prefers zstd when the client accepts it (Chrome, Edge,
Firefox): a tenth of brotli's CPU for about 12% more bytes on HTML, which
turned a CPU-bound Laravel page from 1 030 into 4 700 req/s. Explicit
q-values in `Accept-Encoding` still decide.

Never compressed: images other than SVG/ICO, fonts already compressed
(WOFF/WOFF2), archives, `text/event-stream`, 206 responses, responses with
`Cache-Control: no-transform` or their own `Content-Encoding`, and bodies
smaller than `server.compression.min_size` (1 KiB). Compressed responses
carry a weak ETag and `Vary: Accept-Encoding`.

```toml
[server.compression]
enabled = true
min_size = "1KiB"
precompressed = true
```

## Script Optimizer

Runs with the image optimizer, only when files change (inotify, with
`optimize.scan_interval_secs` polling as a safety net):

1. **Minify** JavaScript (oxc) and CSS (lightningcss). Conservative: no
   top-level renaming, no property mangling, no dead-code removal, license
   comments kept, no browser-target lowering. Files that already look
   minified (`.min.`, long lines) are left alone.
2. **Validate**: the output must parse; otherwise the original is used.
3. **Precompress** and publish objects; the manifest
   (`<state>/optimize/sites/<site>/text.json`) is replaced atomically.
4. **Report** per site: sizes before/after, duplicate files and JS/CSS files
   no other project file mentions (`nova optimize`, `/_nova/optimize/status`).
   These are candidates for review; NOVA never removes code on its own.

```toml
[optimize]
scripts = true
minify = true
text_max_bytes = "8MiB"
watch = true
```

Objects that no manifest references are garbage-collected (at most every
10 minutes, objects younger than an hour are kept).

## Compression dictionaries (RFC 9842, automatic)

Fingerprinted JS/CSS (Vite/Mix builds served as immutable) carry
`Use-As-Dictionary: match="/build/assets/app-*.js"`. After a deploy, a
returning Chromium visitor asks for `app-NEW.js` with
`Available-Dictionary` (the hash of its cached `app-OLD.js`); NOVA answers
with only the difference, a zstd frame against the old file
(`Content-Encoding: dcz`, `Vary: Accept-Encoding, Available-Dictionary`),
when that is smaller than the normal encoding. Versions are remembered in
memory (64 MiB including computed deltas); unknown hashes, other browsers
and a just-restarted NOVA get the usual brotli/zstd/gzip. Measured with a
275 KB library: the updated bundle downloads as 0.6 KB instead of 52.7 KB
(`tests/browser/dictionaries/`).

## Caching

| File | `Cache-Control` |
|---|---|
| Listed in a Vite manifest (`build/manifest.json`, `.vite/manifest.json`, ...) | `public, max-age=31536000, immutable` |
| Laravel Mix file requested with its `mix-manifest.json` `?id=` | immutable |
| Matching `[site.cache] immutable` globs | immutable |
| Matching a `[[site.cache.rule]]` | the rule's value |
| Anything else | `public, max-age=<max_age_secs>` or revalidate with ETag (default) |

```toml
[site.cache]
auto_immutable = true
immutable = ["fonts/**"]
max_age_secs = 0

[[site.cache.rule]]
path = "downloads/**"
cache_control = "private, no-store"
```

## Per-site rules

```toml
security_headers = true          # nosniff, Referrer-Policy, X-Frame-Options (+ HSTS, see above)
canonical_host = "www.example.com"
https_redirect = true            # default: on for hosts with a trusted certificate

[site.headers]                   # added unless the application sets them
Content-Security-Policy = "default-src 'self'"

[[site.redirect]]
from = "/blog/*"                 # exact path, or prefix ending in *
to = "https://blog.example.com/$1"
status = 301                     # 301 302 303 307 308; the query is kept

[site.error_pages]               # for NOVA's own errors (401, 404, 405, 408, 413, 50x)
404 = "/404.html"

[site.auth]                      # HTTP basic auth, bcrypt hashes (htpasswd -nbB)
realm = "Staging"
users_file = "/srv/sites/example/config/htpasswd"   # or users_env = "VAR"
paths = ["/admin/**"]            # empty = whole site
except = ["/.well-known/**"]
```

Order per request: HTTPS redirect → canonical host → redirects → auth →
file / PHP resolution.

## Limits

```toml
[server]
max_connections = 4096
request_body_timeout_secs = 60   # an upload that sends nothing this long is cut off (408)
header_read_timeout_secs = 15    # also covers the PROXY header and TLS handshake

[server.rate_limit]
requests_per_sec = 100.0         # token bucket per client IP (IPv6 per /64)
burst = 400
max_connections_per_ip = 256
exempt = []                      # admin_allow is always exempt
```

Over the limit: `429` with `Retry-After`.

The connection caps apply to HTTP/3 too: QUIC connections count against the
same `max_connections` and the same per-IP budget as TCP (one budget per
address across both). Because UDP source addresses can be forged, an address
that already holds half of its budget must answer a QUIC Retry (address
validation) before it gets more, so spoofed packets cannot lock a real
client out. The QUIC handshake shares `header_read_timeout_secs`, and HTTP/3
request headers are limited to 64 KiB.

Small static files (up to 1 MiB) are served from a 64 MiB in-memory cache
keyed by path, size and modification time, so edits show up on the next
request without any invalidation step.

Files larger than the memory cache (over 1 MiB) are sent with
`sendfile(2)` on plain HTTP/1.1 (zero-copy from the page cache; TLS,
HTTP/2 and HTTP/3 stream them as before). This uses a vendored hyper with
hyperium/hyper PR #4214 (experimental branch only).

In production, path lookups (stat + symlink resolution) are also cached
for 1 second, like nginx's `open_file_cache`: a changed or deleted file
may be served in its previous state for up to a second (new files appear
immediately; development mode never caches). On FUSE filesystems such as
Unraid's `/mnt/user` this is what makes static serving fast.

## Background processes

```toml
[[site.task]]                    # cron, container local time (TZ)
name = "scheduler"
schedule = "* * * * *"           # 5 fields, names, ranges, steps, @hourly/@daily/...
command = ["php", "artisan", "schedule:run"]
timeout_secs = 3600

[[site.worker]]                  # kept running, restarted with backoff
name = "queue"
command = ["php", "artisan", "queue:work", "--sleep=3", "--max-time=3600"]
processes = 2
```

Commands run as the site's uid inside its Landlock sandbox, with the site's
environment (database, `env`, framework settings), the project directory as
working directory and `HOME`/`TMPDIR` in the site's state directory. A
leading `php` gets the same OPcache/realpath tuning and memory limit as the
pool. Runs never overlap; output goes to the log (`nova::task`).

## PHP performance

Every FPM master starts with OPcache (128 MiB, 20 000 files, interned
strings, timestamps checked every 2 s in production and on every request in
development), a 4 MiB realpath cache and the tracing JIT (default since it
measured +7% on uncached WordPress; `jit = "off"` disables it). Workers are
recycled every 10 000 requests (`[site.php] max_requests`; +5% on WordPress
compared with 1 000):

```toml
[php]
opcache_memory = "128MiB"
opcache_revalidate_secs = 2
jit = "tracing"                  # tracing (default), function, off
jit_buffer = "64MiB"
```

Startup-only settings (`opcache.*`, `realpath_cache*`) can be overridden per
site in `[site.php.ini]`; they are passed to that site's master.

### PHP worker mode (opt-in)

FPM boots the application for every request; for Laravel that is most of
the time spent. In worker mode the application stays booted in
long-running PHP processes that answer request after request:

```toml
[site.php]
front_controller = "index.php"
worker = "laravel"        # NOVA's built-in Octane worker: composer require laravel/octane
max_children = 8          # workers
max_requests = 10000      # each worker is replaced after this many requests
```

`worker = "laravel"` runs Laravel Octane's worker (sandboxing, listeners,
state flushing) with NOVA's own transport: the request is built the way
PHP-FPM fills the superglobals and the response serialized straight from
Symfony's Response, without Octane's PSR-7 round trip (12-13% faster than
Octane's RoadRunner worker, which also works:
`worker = "vendor/laravel/octane/bin/roadrunner-worker"` with
`spiral/roadrunner-http` and `spiral/roadrunner-cli`). Urlencoded bodies
stay available raw (`$request->getContent()`, e.g. for webhook signatures).

NOVA speaks the RoadRunner worker protocol (goridge frames over the
worker's stdin/stdout, JSON request context), so anything written for
RoadRunner runs unchanged: Laravel Octane's RoadRunner worker, Symfony with
`runtime/roadrunner-symfony-nyholm`, Spiral, Yii, Mezzio or a plain
`spiral/roadrunner-http` loop. No RoadRunner binary is involved.

Measured on Laravel 13 (8 workers, same machine, uncompressed responses):

| | JSON route | welcome page (70 KB) |
|---|---:|---:|
| NOVA + PHP-FPM | 2 270 req/s | 2 170 req/s |
| RoadRunner 3.0 + Octane | 7 090 req/s | 5 210 req/s |
| NOVA worker mode, Octane's RoadRunner worker | 8 220 req/s | 6 090 req/s |
| **NOVA worker mode, `worker = "laravel"`** | **9 290 req/s** | **6 900 req/s** |
| FrankenPHP 1.13 worker mode + Octane | 10 020 req/s | 8 610 req/s |

FrankenPHP embeds PHP in the server process (one ZTS PHP shared by all
sites); NOVA keeps PHP out of process, as each site's own uid inside its
Landlock sandbox, which costs about 0.2-0.3 ms per request in transport.

Symfony 8 with `composer require runtime/roadrunner-symfony-nyholm` (also
tested: 5 800 -> 25 000 req/s on a JSON route, 4 800 -> 19 300 on a Twig page):

```toml
[site.env]
APP_ENV = "prod"
APP_RUNTIME = 'Runtime\RoadRunnerSymfonyNyholm\Runtime'
APP_CACHE_DIR = "/var/lib/nova/sites/shop/cache"   # the project is read-only to PHP

[site.php]
front_controller = "index.php"
worker = "public/index.php"
```

NOVA sets `APP_RUNTIME_MODE=web=1&worker=1` for workers (unless the site
sets it): without it Symfony treats a CLI worker as a console process and
renders exceptions with its CLI renderer, full details included, even in
production. Plain RoadRunner setups have that leak.

How it works and what NOVA adds:

* Only front-controller requests go to the workers; other `.php` files
  still run in FPM, and the micro-cache, tag purges, NOVA Live, compression
  and HTML features apply to worker responses as to any PHP response.
* A per-site manager (`nova php-workers`) runs as the site's uid inside its
  Landlock sandbox, like FPM, and keeps the workers alive. It connects to a
  socket NOVA listens on; NOVA accepts only the site's uid (`SO_PEERCRED`).
* Form bodies (`application/x-www-form-urlencoded`, `multipart/form-data`)
  are parsed with PHP's own rules (`a[b][]`, `max_input_vars`,
  `max_file_uploads`) inside the sandbox; uploads go to a private temp
  directory per worker and are deleted after the request, including the
  copies Symfony's PSR-7 bridge makes (which leak under RoadRunner).
* A worker that exceeds `timeout_secs` is killed and replaced (504); one
  that crashes is replaced (502), with backoff only if it dies before
  serving anything. A client that disconnects does not cost a worker: the
  response is read to the end and the worker reused.
* `max_requests` recycles workers gracefully; reloads (SIGHUP) keep idle
  workers warm and move them to the new NOVA process; no request is lost
  (a request that crosses a closing connection is retried, as it never ran).
* In development mode workers restart automatically when PHP code, `.env` or
  `composer.lock` change (polled once a second), because a booted
  application never sees new code otherwise.
* The workers get FPM's limits: `memory_limit`, `disable_functions`,
  `open_basedir` (when the kernel sandbox is optional), OPcache + JIT.
  `max_execution_time` is not used (it would count the whole worker life);
  NOVA's timeout replaces it.

Kernels without Landlock (Unraid's, for one) make NOVA fall back to PHP's
`open_basedir`, which costs every file operation a full path check: on
Unraid, Laravel's file-session garbage collection (a directory scan on 2%
of requests) took 2.7 s instead of 0.11 s over 14 500 sessions, about
55 ms per request on average, in FPM and worker mode alike. On such hosts
use `SESSION_DRIVER=redis`, `database` or `cookie`.

Worker mode keeps application state between requests: code must not leak
per-request state into globals or singletons (Octane documents the rules).
WordPress and other classic PHP apps are not built for that; keep them on
FPM (the default), where they are unaffected.

### PHP micro-cache (opt-in)

```toml
[site.php]
micro_cache_secs = 2             # 0 = off (default)
```

Shares a PHP response between visitors for a few seconds, so a traffic
spike costs one PHP execution per page per interval instead of one per
visitor (a 20 ms page: 787 → 163 000 req/s in `tests/performance`).
Only responses that are the same for everyone are stored:

* request: `GET` without `Cookie` or `Authorization` (logged-in users and
  sessions always reach PHP); the key is host, scheme, path, query and the
  NOVA Live `Nova-Live` / `Nova-Target` headers;
* response: `200`, no `Set-Cookie`, no `Cache-Control: private`,
  `no-store` or `no-cache`, no `Vary` other than `Accept-Encoding`, not
  `text/event-stream`, at most 1 MiB (32 MiB in total).

After expiry a page stays usable for `micro_cache_grace_secs` (default
10): the first request refreshes it while everyone else gets the previous
version immediately (`nova-cache: stale`), and if PHP fails (5xx, timeout,
pool down) the last good page is served instead of an error. A purge
(`Nova-Publish`, database change) removes pages completely, grace included.

**Precise purges with tags.** By default any `Nova-Publish` clears the
whole site's cached pages. Tag pages instead and purge only what changed:

```php
header('Nova-Cache-Tags: post-42, home');   // on the article page (stripped)
header('Nova-Purge: post-42');              // in the request that saves it
```

`Nova-Purge: *` clears the site. A database change feed
(`[services.database.*.changes]`) purges pages tagged `db:<table>` plus
every untagged page. Measured (10 cached articles, 60 readers, one article
saved 10x/s): PHP runs 914 -> 151-183, reader throughput +19%. A WordPress
must-use plugin is enough, e.g. `wp-content/mu-plugins/nova-tags.php`:

```php
<?php
add_action('template_redirect', function () {
    $tags = ['home'];
    if (is_singular()) $tags[] = 'post-' . get_queried_object_id();
    header('Nova-Cache-Tags: ' . implode(', ', $tags));
});
add_action('save_post', fn ($id) => header("Nova-Purge: post-$id, home"));
add_action('comment_post', fn ($c, $ok, $data) => header('Nova-Purge: post-' . $data['comment_post_ID']), 10, 3);
```

PHP opts a page out with `header('Cache-Control: no-store')`. A
`Nova-Publish` from the site clears its cached pages, so NOVA Live
refreshes always see new data. Responses carry `nova-cache: hit` (with
`Age`) or `miss`. Cache-eligible misses are buffered before sending; pages
that stream output with `flush()` should send `Cache-Control: no-store`.

## Automatic image markup (`html_rewrite`, experimental)

```toml
[[site]]
html_rewrite = true      # default false
```

HTML responses NOVA sends uncompressed (PHP output, plain static HTML) are
streamed through a rewriter that adds, to every `<img>` and only where the
markup does not set it already: `loading="lazy"` after the first two
images, `decoding="async"`, `fetchpriority="high"` on the first image,
`width`/`height` from the Optimizer (no layout shift) and a `srcset` over
the Optimizer's responsive widths (`sizes="auto, 100vw"` for lazy images).
`data-nova-keep` leaves an image alone. Precompressed static HTML is not
rewritten yet (that belongs in the Optimizer pipeline).

Measured on a naive 12-image gallery (Chromium, Fast 4G): load 1.2 s ->
0.37 s, phone LCP 0.72 s -> 0.35 s, desktop CLS 0.24-0.27 -> 0, image
bytes before load 974 KB -> 59 KB, whole page on a phone 974 KB -> 110 KB.
Reproduce: `tests/browser/rewrite/` (page + Playwright script).

## Early Hints (`early_hints`, on by default)

While PHP builds a page, the browser waits for the HTML before it can even
start fetching the stylesheets and scripts. NOVA remembers, per page path,
what the page's `<head>` loads (learned from its own responses, refreshed
every minute) and sends it as `103 Early Hints` the moment a navigation
arrives, before PHP runs:

```
HTTP/2 103
link: </wp-includes/css/style.min.css?ver=7.1>; rel=preload; as=style
link: </js/app.js>; rel=preload; as=script
link: <https://fonts.googleapis.com>; rel=preconnect
```

Chrome, Edge and Firefox start those downloads during the server's think
time (HTTP/2 and HTTP/3; browsers ignore 103 on HTTP/1.1). Measured with
`tests/browser/earlyhints` (Chromium, 200 ms of PHP, 160 KB stylesheet):
page load 392 -> 296 ms at 80 ms latency, 461 -> 367 ms at 150 ms.

Only navigations get hints (`Sec-Fetch-Mode: navigate`, or `Accept:
text/html`), only for requests that reach PHP (micro-cache hits are
answered at once), at most eight links: same-origin stylesheets, scripts,
module scripts and explicit preloads, plus up to three `preconnect`s for
third-party origins. `early_hints = false` on a `[[site]]` turns it off.

## Instant navigation (`speculation_rules`, opt-in)

```toml
[[site]]
speculation_rules = true
```

HTML pages get a Speculation Rules block (Chromium browsers): same-site
links are prefetched when the visitor is about to click them (moderate
eagerness: hover or pointer-down), never links to logout, login, admin,
API, cart, checkout or `?action=`/`?add-to-cart=` URLs, and never links
marked `rel=nofollow`, `download`, `target=_blank` or
`data-nova-noprefetch`. A page that ships its own `<script
type="speculationrules">` is left alone. `No-Vary-Search` keeps prefetched
pages usable when links carry `utm_*`, `gclid` or `fbclid`.

Measured (Chromium, Fast 4G, a 300 ms PHP page): hover 400 ms then click,
326 ms -> 107 ms to the loaded page; a 100 ms hover is unchanged.

## Reverse proxy to an application server

```toml
[site.proxy]
upstream = "http://127.0.0.1:3000"   # Node.js, Python (ASGI/WSGI server), Go, Ruby, Java...
paths = ["/**"]                      # globs sent upstream (default: everything)
static_first = true                  # files in the document root are served by NOVA
timeout_secs = 60                    # time allowed for the upstream's response headers
```

Requests for matching paths are forwarded over HTTP/1.1 (keep-alive pool)
whatever protocol the client used (HTTP/1.1, HTTP/2, HTTP/3). Bodies are
streamed both ways (server-sent events and chunked output arrive as they
are produced), WebSocket upgrades are tunneled (`ws://` and `wss://`).
With `static_first`, assets in the document root get NOVA's static path:
image negotiation, precompression, immutable caching.

The upstream receives the client's `Host`, plus `X-Forwarded-For`,
`X-Real-IP`, `X-Forwarded-Proto` and `X-Forwarded-Host` set by NOVA from the
validated client (client-sent forwarding headers are dropped).
Hop-by-hop headers stay on their hop. Site rules (redirects, basic auth,
headers, compression, rate limits, access log `kind="proxy"`) apply as for
any other response. An unreachable upstream gives `502`, a slow one `504`
(both use the site's error pages). The worker's Landlock policy is widened
by exactly the upstream ports.

## Reload without downtime

```sh
docker compose kill -s HUP nova
```

The supervisor validates the new file (an invalid one changes nothing),
reconciles PHP pools (only changed pools restart; FastCGI connects retry for
3 s meanwhile), restarts tasks and workers, then starts a new HTTP worker on
the same ports (`SO_REUSEPORT`). Once it reports ready, the old worker stops
accepting and drains. Requires strict isolation (NOVA started as root, the
default in Compose).

## Access log

One structured line per request (`target: nova::access`): request id, site,
client and peer address, host, method, path, protocol (`HTTP/1.1`, `HTTP/2.0`,
`HTTP/3`), TLS, status, bytes (when known), handler kind, content encoding,
referer, user agent and duration. `server.access_log = false` turns it off.
In JSON mode (production default) the line is formatted by a dedicated
writer instead of the generic `tracing` JSON formatter (same keys and
order, about 8% more small-file throughput); `NOVA_LOG` filtering still
applies.
