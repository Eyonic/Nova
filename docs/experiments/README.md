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
| 1 | mimalloc allocator | malloc/free ~5% of worker CPU; work stealing frees across threads | Unraid, local, memory | +1-3% (Unraid), +3-7% (local, h2 most); RSS 58 vs 29 MB | kept |
| 2 | FastCGI keep-alive pool | nginx `fastcgi_keep_conn`; connect/accept per request | local, Unraid, integration | PHP +17% local (above nginx+fpm), +7% Unraid, p99 lower | kept |
| 3 | Thread-per-core runtimes + SO_REUSEPORT | removes work-stealing wake-ups (monoio/tako research) | local, Unraid | h2 +15% but PHP -10% locally; Unraid within noise, p99 worse (uneven spreading) | rejected, patch in `thread-per-core.patch` |
| 4 | 1 s path-lookup cache | `open_file_cache`; stat + readlink per request, blocking-pool hop | Unraid, local, integration | Unraid static x2.2-4.7 (FUSE), local h2 +45%, small +19% | kept |

Not attempted (research): sendfile needs a hyper change (hyper#3026,
PR #4214 closed pending a HIP); kTLS depends on it; tokio interval knobs
expected low single digits; h2 already at 0.4.20 (lock and HPACK fixes).
