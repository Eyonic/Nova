# Experiments (branch `experimental`)

Rules: research first, at most three A/B tests per idea, keep only what is
clearly better (or a deliberate trade-off). Stable baseline: tag
`stable-2026-10-10`. Test beds:

* **Local**: 16-thread desktop, server pinned to 8 cores, load generator
  (oha) on the other 8, `tests/performance/run.sh`.
* **Unraid**: Ryzen 9 5900X server running ~55 other containers; stable
  (`nova-stable`, :38080/:38443) and experimental (`nova-exp`,
  :39080/:39443) stacks side by side, each capped at 4 CPUs / 1 GiB,
  sites on the shfs FUSE share; oha on the host (cores 16-23), 3
  alternating rounds per scenario. A/A noise: ~1%.

| # | Idea | Why (research) | Tests | Result | Decision |
|---|---|---|---|---|---|
| 1 | mimalloc allocator | malloc/free ~5% of worker CPU; work stealing frees across threads | Unraid, local, memory | +1-3% (Unraid), +3-7% (local, h2 most); RSS 58 vs 29 MB right after load, but 28 vs 30 MB (= glibc) after a 5 s settle (see #7) | kept |
| 2 | FastCGI keep-alive pool | nginx `fastcgi_keep_conn`; connect/accept per request | local, Unraid, integration | PHP +17% local (above nginx+fpm), +7% Unraid, p99 lower | kept |
| 3 | Thread-per-core runtimes + SO_REUSEPORT | removes work-stealing wake-ups (monoio/tako research) | local, Unraid | h2 +15% but PHP -10% locally; Unraid within noise, p99 worse (uneven spreading) | rejected, patch in `thread-per-core.patch` |
| 4 | 1 s path-lookup cache | `open_file_cache`; stat + readlink per request, blocking-pool hop | Unraid, local, integration | Unraid static x2.2-4.7 (FUSE), local h2 +45%, small +19% | kept |
| 5 | Lookup cache for Optimizer variants and precompressed siblings (incl. missing) | browser requests (AVIF, br) still did 1-3 uncached stats | Unraid, local, integration | Unraid AVIF/HTML-br/JS-br +27-29% more (now x5-6 stable), p99 ~25 -> 7-10 ms; local br +9% | kept |
| 6 | Micro-cache single flight (`proxy_cache_lock`) | every miss at expiry ran PHP (stampede) | local, unit, integration | 200 ms page, TTL 1 s: PHP runs 167 -> 9, slowest request 9.3 s -> 0.21 s, +8-14% req/s | kept |
| 7 | jemalloc instead of mimalloc | jemalloc returns memory to the OS more eagerly | local, Unraid memory | throughput equal; RSS after settle: jemalloc 37 MB, mimalloc 28 MB, glibc 30 MB | rejected |
| 8 | Store compressed micro-cache renditions | WordPress: cached hits capped at ~2k req/s, each hit re-compressed 69 KB | Unraid WP, local, integration | WordPress cached 2.0k -> 37.7k req/s (p99 77 -> 1.3 ms); 104 KB page x145 | kept |
| 9 | Database change channels (binlog → `db:<table>`) | vision phase C; mysql_async binlog is tested against MariaDB 11/12 | Unraid end-to-end, unit, integration | commit → browser 68 ms avg / 102 ms max; rollback 0 events; other databases 0 events; cached WordPress page purged on SQL edit | kept (prototype) |
| 10 | HTTP/3 without a body pump for GET/HEAD | per-request task + channel even without a body; UDP buffers ruled out (no drops) | local x2, integration | 132.6k -> 141.8k req/s (+7%); POST bodies intact | kept |
| 11 | HTML rewriting for `<img>` (vision phase A) | naive pages load every full-size image and shift layout | Chromium Fast 4G x2, integration | load 1.2 -> 0.37 s, CLS 0.27 -> 0, bytes at load 974 -> 59 KB, phone total 974 -> 110 KB | kept (opt-in) |
| 12 | sendfile(2) for static files (vendored hyper with PR #4214, closed pending a HIP; hyper-util Rewind forwards it) | biggest remaining gap vs nginx; user-space copy of every byte | local, Unraid x2 | 4 MB: local 2.9k -> 13.1k req/s (x4.5), Unraid 927 -> 3,027 (x3.3, p99 76 -> 19 ms); 477 KB from the memory cache: local +91% but Unraid -8% | kept for files > 1 MiB (not in the memory cache); not used for cached files |
| 13 | Micro-cache grace + stale-if-error (Varnish grace, Cloudflare SWR) | expiry made visitors wait on PHP; PHP errors reached visitors | local x2, integration | healthy PHP: unchanged; flaky PHP (500 half the time): errors 1,975 -> 0, 5.6x more responses | kept |
| 14 | Speculation Rules + No-Vary-Search (opt-in) | Chrome/web.dev case studies: tens of % faster navigations | Chromium x2, integration | hover-then-click 326 -> 107 ms; quick clicks unchanged; logout/delete never prefetched | kept |
| 15 | PHP defaults: JIT tracing, max_requests 10 000 (timestamp checks kept) | research: JIT small for web apps, fewer respawns | Unraid WordPress x3 | full profile 104 -> 118% of stable; without validate_timestamps=0: 117%; JIT alone 111%; memory unchanged | kept (defaults) |
| 16 | LTO (fat) + codegen-units = 1 | research: 3-10% for Rust servers | local, Unraid | local h2 +5%, small +5%, 134 KB +12%; Unraid h2 +8%, JS-br +9%, others flat; binary 45 -> 33 MB | kept |
| 17 | Compression Dictionary Transport (RFC 9842, dcz) for fingerprinted JS/CSS | Google/Cloudflare: 60-97% smaller updates; matches "load only what is needed" | Chromium x2 + correctness, integration | deploy update 52.7 KB -> 0.6 KB (-98.9%), Chromium runs the new version without errors | kept |
| 18 | 8 MiB QUIC socket buffers (quinn/Cloudflare guidance) | Unraid HTTP/3 large file 21x slower than HTTP/2, 6,940 UDP drops | Unraid x2, real-client cross-check | server drops 48 -> 0 but no throughput change; the gap is the quinn-based load generator: curl (ngtcp2) over the LAN downloads 4 MB at 15.6 MB/s on HTTP/3 vs 14.5 MB/s on HTTP/2 | rejected (no gain; HTTP/3 is fine for real clients) |
| 19 | Tag-based purges (`Nova-Cache-Tags` / `Nova-Purge`, LiteSpeed-style) | any write cleared the whole site's micro-cache | local x2, unit, integration, real WordPress (Unraid) | one article saved 10x/s among 10 cached: PHP runs 914-991 -> 151-183, readers +18-20%; WordPress mu-plugin: a comment purges only that post, home and other pages stay hits | kept |
| 20 | PHP worker mode (RoadRunner protocol, own manager; user request "remake the PHP runtime") | Laravel boots per request under FPM; RoadRunner/FrankenPHP report 2-4x | local Laravel 13 + Octane x3, Symfony 8 x3, integration (+18 checks), robustness (crash, timeout, client abort, recycle, reload x3 under load) | Laravel JSON 2,270 -> 8,220 req/s (RoadRunner itself 7,090), welcome page 2,170 -> 6,090 (RR 5,210); Symfony 5,800 -> 25,000 and 4,800 -> 19,300; request semantics identical to FPM (forms, uploads, cookies, JSON, streams); 0 failed requests over 3 reloads under load | kept (opt-in per site; WordPress stays on FPM) |
| 21 | OPcache file cache for worker cold starts | new CLI workers recompile the framework (recycle every 50: 535 req/s) | local x1 | PHP writes no file cache while the JIT is on; recycles are rare at max_requests 10,000, so steady-state JIT wins | rejected |
| 22 | Least-recently-used worker selection (FIFO) instead of most-recently-used | idea: frameworks work after responding, so the warm worker may still be busy | local x2 (c=1/4/32) | c=1: 0.67 ms (LIFO) vs 2.4 ms (FIFO); welcome page c=32: 6,017 vs 5,526 req/s | rejected (keep LIFO) |
| 23 | zstd first for on-the-fly compression (brotli stays first for precompressed files) | microbench on real 70 KB pages: br q5 1.2 ms, zstd 3 0.12 ms, +12% bytes; Cloudflare prefers zstd for dynamic content | microbench, local Laravel x3, Unraid x3 (Chrome Accept-Encoding) | Laravel welcome page (worker mode) 1,030 -> 4,700 req/s; Unraid phpinfo +14%; uncached WordPress unchanged (PHP-bound) | kept |
| 24 | Compression window sized to the body (known length, else 512 KiB) instead of brotli 4 MiB / zstd 2 MiB | in-server brotli cost 5x the microbenchmark: a fresh multi-MiB window per response means page faults and zeroing | local Laravel x3 (br-only and Chrome Accept-Encoding) | brotli (Safari) 1,000 -> 2,230 req/s with identical bytes; zstd +5% | kept |
| 25 | 1 MiB worker pipes (F_SETPIPE_SZ) so a 70 KB response never blocks PHP | PHP-bound workers at 96% CPU; 64 KiB pipes block on larger responses | local x2 | 6,800 vs 6,960 req/s, within noise | rejected |
| 26 | Built-in Laravel worker (`worker = "laravel"`): Octane's Worker with NOVA's transport, no PSR-7 | FrankenPHP (in-process, superglobals) measured 10,020 / 8,610 req/s vs 8,260 / 6,090 for Octane's RoadRunner worker on NOVA; PSR-7 conversion both ways is pure overhead | local x3, correctness vs FPM (forms, uploads, cookies, JSON, raw urlencoded body, streams) | 8,260 -> 9,290 req/s (JSON), 6,090 -> 6,900 (welcome page); identical request semantics to FPM; the remaining gap to FrankenPHP is out-of-process transport (~0.2-0.3 ms) | kept |
| 27 | Workers talk to NOVA directly (no manager relay) | the relay is the one extra hop vs in-process servers | instrumented PHP loop (local) | the whole gap between two requests inside a PHP worker is 67 us of 1,240 us (5%); a direct channel could recover about half, at the cost of fd passing, losing warm workers across reloads and moving upload parsing out of the site sandbox | not pursued (bounded at ~2-3%) |
| 28 | Automatic 103 Early Hints (learned from each page's `<head>`, sent before PHP runs) | 2026-10 report gap #9; Cloudflare learns hints the same way; hyper could not send 1xx (added to the vendored hyper's HTTP/2 server, h2 already supports it; HTTP/3 in NOVA's own server) | Chromium x5 per side at 80 and 150 ms latency, integration (+3), real WordPress | page load 392 -> 296 ms (80 ms RTT), 461 -> 367 ms (150 ms); needs a trusted certificate (Chrome ignores 103 on certificate errors) | kept, on by default |
| 29 | Load balancing for `[site.proxy]` (several upstreams, least_conn / round_robin, passive + active health, safe retry) | feature gap vs every competitor (report §5) | unit, lab with 3 backends | even split 10/10/10; one backend killed under load: 200/200 answered (GET retried on another upstream), traffic back on all three after its health check passed | kept |
| 30 | OpenTelemetry tracing (OTLP/HTTP JSON, W3C trace context, server + PHP/proxy child spans) | feature gap (report §5) | unit (trace context, OTLP encoding), Jaeger end to end | an incoming trace is continued; PHP receives a `traceparent` naming its span; Jaeger shows request -> php-fpm with all attributes | kept |
| 31 | ACME DNS-01 (Cloudflare API or hook), wildcard certificates and wildcard site hosts | report gap "ACME breadth" (DNS-01, wildcards) | unit, Pebble + challtestsrv end to end | wildcard + apex certificate issued in 2 s, hot-loaded by the worker without restart, TLS verified for the apex and a wildcard subdomain | kept |
| 32 | Full OWASP CRS via a pure-Rust SecLang engine (zentinel-modsec 0.4, CRS 4.20) | report gap "WAF integration" | 10 attacks / 9 benign requests, timing | 8/10 attacks (missed `1' OR '1'='1` in query and form), blocked a WordPress REST call, 1.6 ms per request | rejected |
| 33 | Built-in WAF (`waf = detect/block`): libinjection SQLi/XSS (barbacane-libinjection, pure Rust) + high-signal rules | #32's lesson: precision and speed first | unit (16 attacks, 17 benign incl. WordPress/Laravel bodies), Unraid crawl in detect mode, perf suite with block | all test attacks found, 0 false positives (unit and real WordPress/Laravel traffic), static 96% / PHP 99% of baseline (noise) | kept, opt-in |
| 34 | Built-in Laravel worker gets `$_SERVER` ready-made from NOVA (the CGI params FPM gets) | PHP glue spent ~53 us per request rebuilding it | local, 3 alternating rounds | no measurable difference (+-10% run-to-run on a loaded desktop); request semantics still identical to FPM | kept for exact FPM parity, not for speed |
| 35 | 1 MiB PHP stream chunks in the built-in worker (one write per response instead of 8 KiB pieces) | idea: syscalls and manager wake-ups per 70 KB page explain part of the FrankenPHP gap | CPU per request of the whole container, 4 alternating pairs | 1.43 vs 1.43 ms (medians) | rejected |
| 36 | sendfile(2) from 256 KiB instead of copying cached files (the 470 KB JPEG gap to nginx) | nginx's zero-copy; loopback benchmark 42% of nginx | local loopback x2, Unraid via Docker port mapping x3 | loopback 20,000 -> 39,400 req/s; Unraid 18,800 -> 15,000 (-20%): through a bridge/veth the copy from memory wins | rejected (the gap only exists on loopback) |
| 37 | Admin API (status, effective config, validate, reload via a supervisor control pipe, purge) | report gap "dynamic configuration API" | integration (+10 checks) | reload over the API swaps workers like SIGHUP; CSRF guard (X-Nova-Admin) and optional bearer token | kept |
| 38 | RFC 2136 + TSIG DNS-01 provider (native, no hook) | report gap "DNS providers beyond Cloudflare" | unit (RFC 4231 HMAC vector, message layout), BIND 9.20 + Pebble end to end | wildcard certificate issued in 2 s through signed dynamic updates | kept |
| 39 | External authorization (`[site.auth_request]`, nginx auth_request / Traefik forwardAuth style) | report gaps "full OWASP CRS" and SSO; #32 showed no Rust CRS engine is ready, the official ModSecurity image is | owasp/modsecurity-crs:nginx and a test SSO service, end to end | CRS: benign GET/form pass (body still reaches PHP), 6/6 attacks 403; SSO: login redirect, identity header passed, forged header dropped | kept |
| 40 | Front controller and route misses through the probe cache (perf profile: 3 blocking-pool hops + stat/readlink per pretty URL) | Unraid: Nova worker 3,460 vs FrankenPHP 6,050 req/s on Laravel `/up`; perf showed readlink/statx/blocking-pool hand-offs per request | CPU per request (trivial worker) x2, Unraid A/B | 157 -> 107 us CPU per request, 40k -> 51k req/s; Unraid `/up` 4,260 -> 4,780 (also for FPM sites: every pretty URL) | kept |
| 41 | Relay without zeroing/re-framing copies; single-threaded manager runtime | manager relay cost 26 us (small) / 67 us (70 KB) per request | CPU per request x2 per variant | copies: no measurable change (kept, simpler); current-thread manager -13% / -10% CPU | kept |
| 42 | open_basedir on hosts without Landlock (Unraid) | FrankenPHP has none | Unraid `/up` | 3,460 -> 4,260 req/s without it; NOVA keeps it there (it is the only file boundary without Landlock) | documented |
| 43 | Large files vs nginx on the realistic path (load from a container on the Docker bridge, no userland proxy; both capped at 4 CPUs) | the 470 KB JPEG "gap" (42% of nginx) came from loopback runs | Unraid x2 per server, plus sendfile vs copy x2 | NOVA 18,100 vs nginx 15,100 req/s (+20%) on the 470 KB JPEG, 74,500 vs 63,000 (+18%) on 1 KB HTML; sendfile 15,400 vs memory copy 18,500 here: NOVA's copy from memory is the faster choice outside loopback | gap closed (loopback artifact) |
| 44 | Native worker API: NOVA's PHP extension provides `frankenphp_handle_request` (PHP fills its own superglobals, `header()`/`echo` work as under FPM) | the RoadRunner JSON glue rebuilt the request in PHP | 16 integration checks, lab vs FrankenPHP | semantics identical to FPM incl. uploads, streaming, fatal errors; NOVA-side CPU 100 -> 86 us per request | kept (`worker = "laravel"`) |
| 45 | Queue in the worker manager, requests multiplexed with ids, next request written to a worker as soon as it answers | instrumented extension: workers idle 98 us between requests (13%) waiting for NOVA's hand-off | lab, timed worker loop, interleaved A/B vs FrankenPHP | wait between requests 200 -> ~55 us; Laravel 70 KB page 6.46k -> 7.65k req/s (+18%, FrankenPHP ~8.3k); tiny response -1.8% (extension's per-request reset) | kept |
| 46 | 1 MiB worker pipes | PHP blocked halfway through writing a 70 KB page into a 64 KB pipe | timed loop | gap between requests 144 -> 125 us; throughput within noise | kept (no cost) |
| 47 | Directory index probes cached as "missing" too | perf: `/` of PHP sites stat'ed index.html/index.htm in the blocking pool on every request | interleaved A/B vs main | PHP-FPM Laravel `/` 2.11k -> 2.23k req/s (+5%) | kept |
| 48 | Access log in 10 ms batches (one `write`) instead of tracing-appender's channel + write per line | access log measured at 60-120 us CPU per request; Caddy/FrankenPHP log nothing by default | CPU per request x2 | /hello 930 -> 884-910 us (-3-5%) | kept |
| 49 | Responsive image markup on by default, skipped for sites without optimized images; static HTML rewritten before compression | CDN-like defaults | 70 KB page with images x2, integration | no measurable CPU; `srcset`/dimensions/lazy on every image | kept |
| 50 | Media files `max-age=3600, stale-while-revalidate=86400` without a cache rule | images revalidated on every view (`max-age=0`) | integration | repeat views reuse images like a CDN | kept |
| 51 | Considered: worker responses straight from PHP to NOVA (skip the manager copy) | manager relay ~57 us per 70 KB page | syscall profile of NOVA's HTTP process | ~7 syscalls per request, nothing wasteful left; the remaining gap to FrankenPHP is the process boundary (per-site isolation), which FrankenPHP does not have | not done |
| 52 | Rewritten static HTML cached per (file version, image manifest, encoding), compressed once at full strength, served with an exact length and a strong ETag | the perf suite caught #49: static 1 KB HTML with br fell to 37% of baseline (rewrite + brotli per request) | perf suite, integration (+3 checks) | 37% -> 89-94% of baseline (noise band), 304 revalidation works | kept |

Considered and skipped in an earlier round (since done as #28): `Link: rel=preload` hints without 103
Early Hints (hyper cannot send 1xx; the browser's preload scanner already
finds `<head>` resources at once, so the research expects little gain for a
fast origin).

## Real applications (Unraid, both stacks, 4 CPUs each)

WordPress 7.1.3 (block theme, MariaDB) and Laravel 13.35 (welcome page,
file sessions), installed side by side on both stacks.

| Page | Stable | Experimental | Note |
|---|---:|---:|---|
| WordPress home, no micro-cache | 44 req/s | 40-46 req/s | PHP-bound (~70 ms render, 4 CPUs) |
| WordPress home, micro-cache 2 s | 1,266-1,955 req/s | **37,737 req/s** | single flight + stored br rendition |
| WordPress with a cookie (bypasses cache) | 68 req/s | 70 req/s | PHP-bound |
| Laravel welcome (sets cookies, never cached) | 137 req/s | 169 req/s | keep-alive + mimalloc; noisy |

Deployment finding: on Unraid, mount sites from `/mnt/cache/appdata/...`,
not `/mnt/user/appdata/...`. Through the shfs FUSE layer, stat on 2000
WordPress files took 1.03 s vs 0.044 s directly, and every WordPress page
took 2.6 s instead of 70 ms (any web server is affected).

Not attempted (research): kTLS (needs the sendfile path plus the ktls
crate; TLS 1.3 key updates are an open question there); tokio interval
knobs expected low single digits; h2 already at 0.4.20 (lock and HPACK
fixes). The sendfile experiment (#12) carries a vendored hyper: drop
`vendor/` and the `[patch.crates-io]` entries if hyper ships its own
file-body API.
