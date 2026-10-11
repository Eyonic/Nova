# Where NOVA stands (11 October 2026)

Follow-up to [the first comparison](2026-10-webserver-comparison.md), after
the experiment rounds logged in [docs/experiments](../experiments/README.md)
(#1-#28). All numbers are our own measurements; methods and caveats at the
end.

## 1. HTTP core vs nginx (same machine, loopback, `REF=1 tests/performance/run.sh`)

| Scenario | NOVA | nginx 1.29 (+ php-fpm) | NOVA / nginx |
|---|---:|---:|---:|
| 1 KB HTML, HTTP/1.1, 256 conns | 188 409 | 196 438 | 96% (p99 3.4 vs 7.8 ms) |
| 1 KB HTML, HTTP/2 + TLS | 178 469 | 187 156 | 95% |
| 100 KB text | 156 884 | 174 420 | 90% |
| 100 KB text, brotli | **191 396** | 170 903 | **112%** |
| 1 KB HTML, brotli | 187 268 | 199 105 | 94% |
| PHP "hello" (FPM) | **51 451** | 45 517 | **113%** |
| 470 KB JPEG | 25 084 | 59 706 | 42% |

In the first report NOVA was at 71% of nginx on small files and 59% on
HTTP/2. It is now within 4-10% on plain static files, ahead on compressed
files and on PHP-FPM, with a lower p99. The JPEG gap only exists on
loopback (25 000 x 470 KB is 11.7 GB/s, far beyond any network card);
behind Docker's bridge sendfile measured 8% slower, so it is not used for
files served from memory.

## 2. PHP application servers (Laravel 13, 8 workers, uncompressed)

| Server | JSON route | Welcome page (70 KB) |
|---|---:|---:|
| NOVA + PHP-FPM (classic) | 2 270 | 2 170 |
| RoadRunner 3.0 + Octane | 7 090 | 5 210 |
| NOVA worker mode, Octane's RoadRunner worker | 8 220 | 6 090 |
| **NOVA worker mode, `worker = "laravel"`** | **9 290** | **6 900** |
| FrankenPHP 1.13 worker mode + Octane | 10 020 | 8 610 |

Symfony 8 (`runtime/roadrunner-symfony-nyholm` on NOVA): 5 800 -> 25 000
req/s on a JSON route, 4 800 -> 19 300 on a Twig page.

After the request-path work of experiments #40-#41 (same lab, later run):
Laravel `/up` NOVA 11 700 vs FrankenPHP 12 300 req/s (95%), JSON route
8 500 vs 9 600 (88%), 70 KB page about 76%. On Unraid with a 4-CPU limit
for both: 5 015 vs 6 050 (83%; it was 57% before the fix).

NOVA beats RoadRunner by 16-32% and runs Laravel at 3-4x its FPM speed.
FrankenPHP stays ahead (8-25%) because it runs PHP inside the server
process: one ZTS PHP for all sites. NOVA keeps every site's PHP out of
process, under the site's own uid and Landlock sandbox; that isolation
costs about 0.2-0.3 ms per request in transport, which we chose to keep.

With a browser's `Accept-Encoding` the order changes again: NOVA now
compresses on the fly with zstd (a tenth of brotli's CPU), so the Laravel
welcome page went from 1 030 to 4 700 req/s under Chrome's header.

## 3. Real hardware (Unraid, Ryzen 9 5900X, 4-CPU container, this round vs the stable build)

| Scenario | stable | now | |
|---|---:|---:|---:|
| Static HTML | 39 753 | 64 446 | +62% |
| PHP page (FPM) | 11 590 | 16 328 | +41% |
| WordPress, uncached (cookie) | 71 | 81 | +14% |
| Laravel `/up` (FPM -> `worker = "laravel"`) | 590 | 3 274 | 5.5x |

## 4. What NOVA does that the others do not do together

| | NOVA | nginx | Caddy | FrankenPHP | RoadRunner | LiteSpeed |
|---|---|---|---|---|---|---|
| PHP worker mode | yes (Octane, Symfony, RR workers) | no | via FrankenPHP | yes | yes | LSAPI |
| Per-site uid + kernel sandbox for PHP | yes (Landlock) | no | no | no | no | with CageFS |
| Page cache with tag purges | yes (`Nova-Cache-Tags`, DB change channels) | fastcgi_cache (no tags) | no | no | no | LSCache |
| Automatic 103 Early Hints | yes (learned per page) | relays an upstream's 103 | configured by hand | sent from PHP code | no | not checked |
| Compression dictionaries (RFC 9842) | yes (automatic for fingerprinted assets) | no | no | no | no | no |
| zstd on the fly, precompressed brotli/zstd/gzip | yes (automatic) | gzip (brotli/zstd as modules) | zstd + gzip (brotli: plugin) | via Caddy | no | gzip + brotli |
| AVIF/WebP + responsive images | yes (automatic) | no | no | no | no | via its WordPress plugin + QUIC.cloud |
| Live partial page updates | yes (NOVA Live) | no | no | Mercure (separate) | no | no |
| Cron tasks + queue workers supervised | yes | no | no | no | jobs plugin | no |
| Load balancing + health checks | yes (least_conn/round_robin, passive + active, safe retry) | yes | yes | via Caddy | n/a | yes |
| OpenTelemetry tracing (incl. PHP child spans) | yes (OTLP/HTTP, W3C trace context) | module | yes | via Caddy | yes | not checked |
| ACME DNS-01 / wildcard certificates | yes (Cloudflare, any DNS via hook) | no (external tools) | yes (DNS plugins) | via Caddy | n/a | not checked |
| Request inspection (WAF) | yes (built-in libinjection + rules; full OWASP CRS via auth_request) | ModSecurity module | Coraza plugin | via Caddy | no | ModSecurity |
| External authorization / SSO (forward auth) | yes (`[site.auth_request]`) | auth_request | forward_auth | via Caddy | no | not checked |
| Admin API | status, config, validate, reload, purge | no (Plus only) | full config API | via Caddy | RPC | WebAdmin |
| ACME DNS-01 providers | Cloudflare, RFC 2136, any via hook | external tools | many (plugins) | via Caddy | n/a | not checked |

Added after this report's first version (same day): load balancing,
OpenTelemetry tracing, ACME DNS-01 (Cloudflare, RFC 2136, hooks) with
wildcard certificates and wildcard site hosts, the WAF, external
authorization (SSO and the full OWASP CRS), the admin API and a faster
PHP request path; see experiments #29-#42.

## 5. Where others still lead

* **FrankenPHP**: 8-25% more Laravel throughput (in-process PHP; see §2).
* **nginx / H2O**: a few percent on plain static files over loopback; zero
  copy for files served from memory on loopback.
* **Features**: a CRS engine inside the server (NOVA runs the full OWASP
  CRS through the official ModSecurity image via `[site.auth_request]`,
  or its own faster rule set), token-API DNS providers other than
  Cloudflare (RFC 2136 and hooks cover the rest), a configuration API
  that accepts new configuration (NOVA's validates it and reloads the
  file).
* **Maturity**: decades of production use, fuzzing, packaging.

## Method notes

* Lab: 16-core desktop, Linux 7.2, rootless Podman with host networking;
  load generator `oha` pinned to 4 cores; 3 runs of 5-8 s, medians.
  Laravel 13.35 production config, array sessions (file sessions measure
  Laravel's session garbage collection, not the server).
* FrankenPHP and RoadRunner ran their official images/binaries with JIT,
  OPcache and 8 workers like NOVA.
* Unraid: `ab.sh` A/B between the stable and experimental stacks, 3 runs.
* Browser numbers (Early Hints) come from `tests/browser/earlyhints`
  (headless Chromium, emulated latency).
