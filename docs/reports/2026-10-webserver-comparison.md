# NOVA vs the main web servers (October 2026)

Measured on one machine (16 cores, Linux 7.2, rootless Podman, host
networking): every server got cores 0-7, the load generator (`oha`) cores
8-15. Same files for every server, default-ish production configs, access
logging **on** everywhere, compression **off** in the client (so nobody
compresses; see "compression" below for why that matters). 15 s per
scenario after a 5 s warm-up. Single runs: treat differences under ~10% as
noise. Feature and security notes come from online research (sources at the
end); the numbers are our own.

Reproduce: `REF=1 tests/performance/run.sh` (NOVA vs nginx + PHP-FPM).

## 1. Benchmark results

Requests per second (higher is better):

| Scenario | NOVA before | **NOVA now** | nginx 1.29 | Caddy 2.10 | Apache 2.4 (event) | H2O 2.2.6 |
|---|---:|---:|---:|---:|---:|---:|
| 1.2 KB HTML, HTTP/1.1, 256 conns | 77.6k | **140.4k** | 197.5k | 72.0k | 98.4k | 212.0k |
| 134 KB text, HTTP/1.1, 64 conns | 41.9k | **89.7k** | 157.4k | 57.4k | 52.3k | 105.9k |
| 477 KB JPEG, HTTP/1.1, 64 conns | 18.1k | **24.1k** | 54.5k | 39.0k | 19.3k | 30.1k |
| 1.2 KB HTML, HTTP/2 + TLS, 256 streams | 62.6k | **110.3k** | 188.2k | 58.8k | 43.5k | 229.9k |
| PHP "hello", 64 conns | 37.0k | **39.2k** | 44.9k (php-fpm) | – | – | – |

p99 latency (small file, HTTP/1.1): NOVA now 5.9 ms, nginx 8.1 ms, Caddy
38.3 ms, Apache 17.5 ms, H2O 6.0 ms. Peak memory during the run: NOVA 93 MB
*including* its 16 PHP-FPM workers; nginx 60 MB + php-fpm 22 MB; Caddy 91 MB;
Apache 101 MB; H2O 47 MB.

"Before" is the original code (with the security fixes); "now" includes the
hot-path work done in this round (section 3 of the feedback below).

NOVA without access logging: 174k (small), 102k (134 KB), 132k (HTTP/2).
NOVA with the rate limiter disabled: same as with it enabled (within 2%).

### Where NOVA stands

* **Small and medium static files:** now ahead of Caddy and Apache, behind
  nginx and H2O (71% of nginx on small files, 88% without access logs).
* **Large files:** clearly behind nginx (44%). nginx uses `sendfile`
  (zero-copy from the page cache); NOVA copies every byte through user space.
* **HTTP/2:** 59% of nginx and 48% of H2O. Single-threaded-per-connection
  hyper h2 plus rustls; worth profiling separately.
* **PHP:** 87% of nginx + php-fpm on a hello-world, i.e. the FastCGI path is
  fine; real apps are dominated by PHP itself.
* **Compression:** with `Accept-Encoding` sent and the Optimizer disabled,
  NOVA compresses static files *on every request*: the 134 KB file drops to
  ~1k req/s. With the Optimizer enabled (the default) files are precompressed
  in the background, so this only bites on files the Optimizer does not
  handle and on PHP output. nginx does not compress static files by default
  at all.

## 2. What the others do better than NOVA

**Performance**

1. Zero-copy large files (`sendfile`, nginx/Apache; kTLS in nginx for TLS).
2. HTTP/2 throughput (nginx ~1.7x, H2O ~2x NOVA on small files).
3. Cheap access logging (nginx buffered log writes; NOVA spends ~19% of
   small-file throughput on JSON log formatting).
4. Response caching: nginx `proxy_cache`/`fastcgi_cache`, LiteSpeed
   LSCache. Cached WordPress pages are served without touching PHP, which is
   where the large published "LiteSpeed/nginx is N times faster for
   WordPress" numbers come from.
5. PHP worker mode (FrankenPHP) or LSAPI keep the app booted between requests.

**Features**

6. **Reverse proxy to any backend** (Node, Python, Go, WebSocket, gRPC) with
   load balancing and health checks: every competitor has it, NOVA does not.
7. **ACME breadth**: HTTP-01, DNS-01 (wildcard certificates), on-demand TLS
   (Caddy), ARI renewal info. NOVA only does TLS-ALPN-01, which fails behind
   a TLS-terminating CDN/load balancer.
8. **TLS extras**: OCSP stapling, mTLS client certificates, ECH (Caddy,
   recent nginx), post-quantum hybrid key exchange (X25519MLKEM768, on by
   default in Caddy and in rustls' aws-lc-rs provider; NOVA uses the `ring`
   provider, which does not offer it).
9. **103 Early Hints** (nginx, Apache, Caddy, H2O).
10. **WAF** integration (ModSecurity / Coraza with the OWASP Core Rule Set).
11. **Dynamic configuration and extensibility**: Caddy's admin API,
    Traefik's Docker labels, nginx/OpenResty Lua and njs, Go/wasm plugins.
12. **OpenTelemetry tracing** (Caddy, Traefik, nginx module).
13. **Maturity**: decades of fuzzing, packaging, CVE process; `.htaccess`
    compatibility (LiteSpeed) for shared-hosting migrations.

**Where NOVA is ahead**: memory-safe core (Rust) *and* per-site uid +
Landlock kernel sandbox (only LiteSpeed + CloudLinux CageFS is comparable),
built-in AVIF/WebP + responsive images, JS/CSS minification, zstd, supervised
cron tasks and queue workers, partial page updates (NOVA Live), one
container, and (after this round) a p99 latency on small files level with
H2O and lower than nginx, Caddy and Apache in this test.

## 3. What we changed in this round (the five feedback points)

| # | Feedback | Done |
|---|---|---|
| 1 | Tighten proxy trust, sandbox fails closed | Only the configured forwarding header is read (a client-sent `Forwarded` passing through an `X-Forwarded-For` proxy could spoof any IP, including admin ranges); scheme taken from the same hop; unverifiable hops stop the walk; shipped config trusts no proxies (it trusted all private ranges, which under Docker NAT is every client); `0.0.0.0/0` rejected; PROXY protocol accepted only from `trusted_proxies`. Landlock `require` now demands filesystem (ABI 1) **and** TCP (ABI 4) enforcement instead of accepting "partially enforced", and the HTTP worker is held to it too (it always ran best-effort). |
| 2 | HTTP/3 per-client resource gap | QUIC connections now share the per-IP connection budget with TCP; addresses holding half their budget must pass a QUIC Retry first (spoofed UDP sources cannot lock out real clients); QUIC handshake bounded by `header_read_timeout`; HTTP/3 request headers capped at 64 KiB. |
| 3 | Profile rate limiter, logging, static files | Profiled with `perf`. Static files: one blocking-pool hop for stat+canonicalize instead of two, and a 64 MiB in-memory cache for files up to 1 MiB (keyed by size+mtime, so edits show immediately). Logging: written by a dedicated thread (no dropped lines). Rate limiter: the global sweep mutex taken on every request is now an atomic. Result: +81% small files, +114% 134 KB, +33% JPEG, +76% HTTP/2. |
| 4 | Clarify arbitrary HTTP backends | README "What NOVA serves" table and spec limitation: static + PHP-FPM only, no generic reverse proxy/WebSocket/gRPC; how to combine with a proxy today; `[site.proxy]` on the roadmap. |
| 5 | Repeatable performance + security regression tests | `tests/performance/run.sh` (baseline file, fails on >15% regression, optional nginx reference). 7 new protocol-abuse integration checks (CL+TE, duplicate Content-Length, obs-fold, bad chunk size, unknown Transfer-Encoding, PHP-suffix path trick, NUL byte). One finding: a request with both `Content-Length` and `Transfer-Encoding` is accepted and framed by `Transfer-Encoding` (hyper drops the length before NOVA sees the request, which RFC 9112 allows), but hyper keeps the connection open where the RFC requires closing it, and nginx rejects such requests outright. NOVA cannot detect this above hyper; see improvement 3. New unit tests for proxy trust, file cache and Landlock require mode. |

## 4. What we can improve next (prioritized)

**Security / correctness**

1. Run `cargo audit` / `cargo deny` in CI and subscribe to RustSec for
   hyper, h2, h3, quinn, rustls (h2 0.4.20 already includes the
   "MadeYouReset" fix).
2. Fuzz the request path (HTTP/1 parser edge cases, path normalization,
   FastCGI param building, image decoders) with `cargo fuzz`.
3. Requests with both `Content-Length` and `Transfer-Encoding`: hyper
   handles them by `Transfer-Encoding` but does not close the connection
   (RFC 9112 §6.3 "MUST close"). Propose the fix upstream (hyper) or reject
   them in a thin pre-parser; until then NOVA should not sit behind a proxy
   that forwards both headers.
4. Image optimizer hardening: cap decoded pixel count (decompression bombs),
   restrict `?w=` to configured widths (already the case: verify in tests),
   decode in the sandboxed worker only.
5. Under rootless Podman / Docker userland-proxy every client shows up as the
   gateway IP: per-IP limits then apply to *all* clients together. Document
   this prominently and recommend `network_mode: host` or a PROXY-protocol
   front for production.

**Performance**

6. `sendfile`/`splice` for large static files on plain HTTP/1.1 (and kTLS
   later): the biggest remaining gap (2.3x vs nginx on the JPEG).
7. Cheaper access logs: pre-formatted line writer instead of the generic
   JSON formatter, optional sampling, or a binary/structured sink.
8. HTTP/2: profile the TLS + h2 path (connection-level flow control
   windows, write buffer sizes, `max_concurrent_streams`).
9. ~~Startup: the Script Optimizer's text pass waited behind image
   encodes~~ **fixed in this round**: text and image jobs shared one
   2-permit semaphore, so a cold start with AVIF encoding delayed new JS/CSS
   by ~30 s (one integration check was flaky because of it). Text jobs now
   have their own permits.
10. Cache negative lookups and canonical paths per request burst
   (`readlink` is still ~3% of CPU on small files).

**Features (by impact)**

11. HTTP-01 and DNS-01 ACME (wildcards), ARI.
12. `[site.proxy]`: HTTP/WebSocket reverse proxy to one upstream per site
    (Node/Python/Go apps), with health checks.
13. FastCGI micro-cache (1-10 s, only for responses without cookies or
    `Cache-Control: private`), plus purge from PHP via a header.
14. 103 Early Hints (blocked on hyper's API; alternative: `Link: preload`
    on the final response, which NOVA can already send).
15. Post-quantum key exchange: switch rustls to the aws-lc-rs provider.
16. OpenTelemetry traces (request → PHP → DB timing).
17. The vision items in [vision.md](../architecture/vision.md): HTML
    rewriting for lazy/responsive images, embedded database, database
    change channels for NOVA Live.

## Method notes and caveats

* One machine, client and server on the same host (loopback): absolute
  numbers are higher than over a real network, ratios are what matter.
* H2O 2.2.6 is the newest prebuilt image available; current H2O is faster
  and supports more. Caddy and nginx ran their default (stock) builds.
* Rootless containers lack cpuset delegation, so pinning used `taskset` on
  each container's own cgroup threads (see `tests/performance/run.sh`).
* Published third-party numbers (for context, not reproduced here):
  ComputingForGeeks (Mar 2026, 4 vCPU, small static file): nginx 60k,
  HAProxy 57k, Caddy 52k, Apache 47k req/s; the ordering matches ours.
  Vendor WordPress benchmarks (LiteSpeed vs nginx) measure page caching,
  not the HTTP core.
