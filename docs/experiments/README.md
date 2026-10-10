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

Considered and skipped this round: `Link: rel=preload` hints without 103
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
