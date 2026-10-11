# Where NOVA stands (11 October 2026)

Follow-up to [the first comparison](2026-10-webserver-comparison.md), after
the experiment rounds logged in [docs/experiments](../experiments/README.md)
(#1-#28). All numbers are our own measurements; methods and caveats at the
end.


## Head-to-head on a real server (Unraid, 4 cores each, browser `Accept-Encoding`)

Every server pinned to the same 4 cores, load from a container on the
same Docker network, same PHP settings and Laravel environment, 3
interleaved rounds, medians (harness: `tests/compare/`).

| req/s | NOVA | nginx + PHP-FPM | FrankenPHP (Caddy) |
|---|---:|---:|---:|
| static 1 KB HTML | **99 333** | 56 099 | 28 794 |
| static 470 KB JPEG | 20 580 | **21 307** | 5 117 |
| WordPress home (no page cache) | **125** | 110 | 109 |
| Laravel `/up` (worker modes; nginx: FPM) | **5 917** | 1 100 | 4 622 |
| Laravel welcome page (file sessions) | 692 | 311 | **703** |

Over HTTPS, the way browsers connect (same setup, shared self-signed
P-256 certificate for nginx and Caddy, `tests/compare/run-tls.sh`):

| req/s | NOVA | nginx + PHP-FPM | FrankenPHP (Caddy) |
|---|---:|---:|---:|
| HTTP/2 small HTML | **87 993** | 59 912 | 21 499 |
| HTTP/2 470 KB JPEG | **5 182** | 2 458 | 1 957 |
| HTTP/2 WordPress | **129** | 109 | 107 |
| HTTP/2 Laravel `/up` | **6 235** | 1 102 | 4 354 |
| HTTP/3 small HTML | **70 200** | (failed) | 7 400 |
| HTTP/3 WordPress | **133** | (failed) | 28 |

nginx's HTTP/3 (1.27, `listen 443 quic`) reset its streams with
`H3_INTERNAL_ERROR` in this setup, so it has no valid number.

NOVA leads or ties everywhere: +77% over nginx on small pages (stored
brotli/zstd vs gzip on the fly), WordPress +14% over both, Laravel +28%
over FrankenPHP's worker mode and 5.4x nginx + FPM. With its micro-cache
(`micro_cache_secs = 2`) WordPress goes from ~125 to tens of thousands of
req/s, which neither competitor offers built in. FrankenPHP's official image
lacks mysqli (WordPress needs it) and did not compress WordPress output.
On kernels without Landlock and with several PHP sites, NOVA adds
`open_basedir` to keep sites apart (WordPress 85 req/s there; nginx + FPM
with the same setting: 85 too); neither competitor isolates sites at all.

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

On the path real traffic takes (load from a container on the Docker
bridge, both servers capped at 4 CPUs, Unraid): **NOVA 18 100 vs nginx
15 100 req/s on the 470 KB JPEG (+20%) and 74 500 vs 63 000 on 1 KB HTML
(+18%)**. The loopback JPEG gap below is an artifact: nginx's sendfile
wins on loopback and loses behind a bridge (NOVA's sendfile variant: 15 400
there, its memory copy 18 500).

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
for both: 5 160 vs 6 040 (85%; it was 57% before this work).

After the worker transport redesign (experiments #44-#48: NOVA's PHP
extension, queue in the manager, requests sent ahead), interleaved on the
same desktop: 70 KB page 7 650 vs FrankenPHP ~8 300 req/s (92%, was 76%),
small JSON route 9 860 vs ~9 800 (level, with NOVA's access log on;
FrankenPHP logs nothing by default). With a browser's Accept-Encoding
(both compress) on a loaded desktop: 70 KB page 5 800 vs 6 900 (84%),
small route 9 000 vs 9 600. A syscall profile shows about 7 syscalls per
request in NOVA's HTTP process: what remains is the process boundary.

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

* **FrankenPHP**: on a loaded desktop lab, 0-16% more Laravel throughput depending on page size (see §2); on the real-server head-to-head above NOVA leads (`/up` +28%) or ties (welcome page).
* **nginx / H2O**: a few percent on plain static files over loopback only
  (behind a Docker bridge NOVA is ahead, see §1).
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
