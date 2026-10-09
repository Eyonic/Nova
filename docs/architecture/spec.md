# NOVA technical specification

Status: implemented and covered by `tests/integration/run.sh`.

## 1. Module contracts

| Crate | Owns | Depends on | Must not |
|---|---|---|---|
| `nova-config` | `nova.toml` schema, defaults, validation | — | touch the filesystem beyond reading the file |
| `nova-http` | accept loop, connection limits, header timeouts, graceful drain, safe path lexing, static file responses (ETag, 304, Range, HEAD) | hyper (protocol only) | know about sites, PHP or images |
| `nova-runtime-php` | FastCGI client, CGI environment, PHP-FPM config generation and supervision | tokio | depend on hyper or config types |
| `nova-optimize` | discover → analyze → plan → transform → validate → publish → serve; manifest format; object store | image, libwebp, rav1e (ravif) | block a request on encoding |
| `nova-security` | Landlock sandbox policies, directory ownership helpers | landlock, libc | know about sites or PHP |
| `nova-core` | site registry, dispatch (request lifecycle), isolation planning, supervisor/worker lifecycle, health, metrics | all of the above | contain protocol or codec code |
| `nova-cli` | `nova serve / check / optimize / health`, internal `worker` and `sandbox` | core | — |

Mature engines are used behind each boundary (hyper for HTTP parsing, the
official PHP-FPM for PHP, libwebp/rav1e for codecs). NOVA owns the
integration, configuration, lifecycle and the developer-facing behavior.

## 2. Configuration (`config/nova.toml`)

* `version = 1` is required; unknown keys are errors (`deny_unknown_fields`).
* Validation collects **all** errors (`nova check` prints them).
* Secrets never appear in the file: `password_env` / `env_from` name
  environment variables resolved at startup; a missing one aborts startup.
* `NOVA_MODE` overrides `mode`; `NOVA_LOG` sets the log filter;
  `NOVA_LOG_FORMAT=json|text` (default: json in production).

Sections: `[server]`, `[paths]`, `[php]`, `[optimize]`,
`[services.database.<name>]`, and one `[[site]]` per site with optional
`[site.php]`, `[site.database]`, `[site.env]`, `[site.env_from]`.
`nova check --php` prints the generated PHP-FPM configuration.

## 3. Request lifecycle (`nova-core/src/dispatch.rs`)

1. `/_nova/*` internal endpoints: `live.js` and `live/events` (NOVA Live, see
   [live.md](live.md)), `health/live`, `health/ready`
   (PHP pools ping + database TCP + not draining), `metrics` (Prometheus),
   `optimize/status`.
2. Site lookup by `Host` / `:authority` (port and trailing dot ignored),
   falling back to the `default` site.
3. Lexical path check: percent-decode, reject `..`, NUL, backslash, invalid
   UTF-8 (400); dot-segments other than `.well-known` are 404.
4. Target resolution, in order: existing file → directory (301 to add the
   slash, then `index.html`, `index.htm`, `index.php`) → `/x.php/path-info`
   → front controller → 404. Every candidate is canonicalized and must stay
   inside the document root (symlink escapes are 404).
5. Execution: `.php` → PHP (404 if the site has PHP disabled, so source is
   never served); optimizable image → negotiated variant or original;
   anything else → static file (GET/HEAD only, else 405).
6. Every response gets `server: nova` and `x-request-id`; one structured
   access-log line and metrics per request.

## 4. PHP integration

* **One PHP-FPM master per site** (one pool each), `pm = ondemand`, listening
  on `/run/nova/php/<site>/fpm.sock`. Classic request lifecycle: no
  application state survives a request; `pm.max_requests` recycles workers.
* NOVA speaks FastCGI itself (`fastcgi.rs`, `client.rs`): one connection per
  request, request body streamed into STDIN concurrently with reading STDOUT
  (no deadlock on scripts that write before reading input), CGI headers
  parsed (`Status`, `Location` → 302), body streamed to the client, stderr
  forwarded to the log. Chunked uploads are buffered (bounded) because PHP
  needs `CONTENT_LENGTH`. `Proxy` is never forwarded (httpoxy).
* Errors map to 503 (pool unavailable), 504 (timeout), 502 (bad response);
  details are shown only in development mode.
* Startup: config is checked with `php-fpm -t`; readiness = every pool
  answers a FastCGI ping. Crash → restart with exponential backoff.
  Shutdown: SIGQUIT, SIGKILL after the grace period.
* Each master is launched as `nova sandbox <policy> -- php-fpm ...` under the
  site's uid (see §4a). Its stdout/stderr pipe is created by NOVA and handed
  to the site uid, because FPM reopens it through `/proc/self/fd/2`.
* Per-pool hardening (defense in depth on top of §4a): `clear_env`, `open_basedir` = project dir + the site's
  own state dir, per-site tmp/upload/session dirs, `disable_functions`
  (exec family by default), `security.limit_extensions = .php`,
  `expose_php = off`, admin-locked memory/time/upload limits.
* Secrets: values are passed via the FPM master's environment under
  generated names and referenced as `env[KEY] = $NOVA_POOL_<SITE>_<KEY>`,
  so no secret is written to disk and each pool sees only its own.

## 4a. Site isolation

Configured under `[isolation]` (global) and `[site.isolation]` (per site).
`mode = "auto"` picks **strict** when NOVA starts as root, otherwise **shared**.

```text
tini
└─ nova serve       supervisor, uid 0, only CHOWN DAC_OVERRIDE FOWNER SETUID SETGID KILL
   ├─ nova worker   uid 10001, no capabilities, Landlock; HTTP + optimizer; never sees secrets
   └─ php-fpm × N   one master per site, site uid, no capabilities, Landlock
```

| Layer | Mechanism | Stops |
|---|---|---|
| Identity | per-site uid (`base_uid + fnv1a(name) % 30000`, or explicit `uid`; collisions rejected) | reading other sites' state (0700), `/proc/<pid>/*` of other sites, signals |
| Socket access | `/run/nova/php/<site>/` owned by site, group worker, mode 0710 | site A connecting to site B's FPM socket (FastCGI hop) |
| Filesystem | Landlock: read system dirs + own project; write own state + own socket dir; nothing else | reading other sites' code even when world-readable, writing anywhere else, executing from writable dirs |
| Network | Landlock TCP: connect only to the site's database port(s) + `allow_connect`; bind nothing | SSRF to NOVA or internal services, outbound internet, opening listeners |
| Process | Landlock signal + abstract-socket scoping; no capabilities; `no-new-privileges` | signalling or reaching processes outside the sandbox |
| Secrets | per-site env only in that site's FPM master; worker env is scrubbed | credential leaks between sites or to the network-facing process |
| Database | one user per site with grants on its own database only | cross-site SQL access |

**shared** mode (non-root start): no uid separation, but Landlock still
applies to every site. `require_landlock = true` (default) refuses to start
PHP on kernels without Landlock.

Verified by `tests/integration/run.sh`: the PHP-level probe (19 attacks)
and a kernel-level section that runs plain shell commands as a site's uid
inside its policy, so PHP's own restrictions play no part.

## 5. NOVA Optimize

* **Discover** JPEG/PNG under each document root (no symlinks, no dot-dirs).
* **Incremental**: unchanged size+mtime → reuse; changed mtime but same
  BLAKE3 hash → metadata refresh only; failed files are retried only when
  their mtime changes.
* **Analyze**: dimensions (pixel limit enforced before decode), EXIF
  orientation, real transparency, animated PNG detection (skipped).
* **Plan**: configured widths below the source width + the source width;
  AVIF and WebP at every width, the source format at resized widths only.
* **Transform/Validate**: Lanczos3 resize; libwebp, rav1e, image's JPEG/PNG
  encoders; outputs re-checked (dimensions, or the AVIF `ftyp` brand).
* **Publish**: objects are immutable and *derivation-addressed*
  (`hash(source hash, profile, width, format)`), which deduplicates identical
  images across paths and sites; the manifest is replaced atomically.
* **Serve**: one URL per image. `Accept` selects AVIF/WebP, `?w=` selects the
  smallest variant at least that wide; the smallest acceptable file wins and
  the original is used whenever it is smaller. `Vary: Accept` is always set.
  A stale manifest entry (source changed) falls back to the original.
* Workers are bounded by `optimize.workers`; AVIF encoding is single-threaded
  per job. Requests are never blocked on encoding.

### Manifest format (`<state>/optimize/sites/<site>/manifest.json`)

```json
{
  "version": 1,
  "site": "example",
  "profile": "3f1c…",                 // hash of widths/formats/quality/speed
  "generated_at": 1791549520,
  "assets": {
    "images/hero.jpg": {
      "source": { "hash": "<blake3>", "bytes": 477417, "mtime_ns": 1791…,
                  "width": 2400, "height": 1600, "format": "jpeg", "alpha": false },
      "variants": [
        { "width": 640, "height": 427, "format": "avif",
          "object": "ab/ab12…ef.avif", "bytes": 9912 }
      ]
    }
  },
  "errors":   { "broken.png": { "message": "decode: …", "mtime_ns": 1791… } },
  "warnings": { "images/huge.jpg": "612000 bytes after optimization exceeds budget of 512000 bytes" }
}
```

Objects live in `<state>/optimize/objects/<2 hex>/<32 hex>.<ext>`.

## 6. Docker model

* Multi-stage build; final image = official `php:<ver>-fpm-trixie` + `tini` +
  `nova`. Rust and PHP versions pinned via build args.
* Supervisor starts as root with 6 capabilities; worker (uid 10001) and PHP (site uids) have none. Read-only root filesystem,
  `no-new-privileges`, CPU/memory/pid limits, tmpfs for `/run/nova` and `/tmp`.
* Persistent state only in volumes: `nova-state` (variants, sessions, tmp)
  and `db-data`. The Docker socket is never mounted.
* `HEALTHCHECK` runs `nova health` (built-in HTTP probe, no curl needed).
* MariaDB init creates one database and one user per site.

## 7. Test strategy

| Layer | Where | What |
|---|---|---|
| Unit | `cargo test` (via `scripts/cargo.sh test`) | config validation, FastCGI encoding, CGI params, path security, ranges, dispatch resolution order and symlink escapes, planning, negotiation, encoder output validation, incremental scans |
| Browser | `tests/browser/run.sh` | 14 NOVA Live tests in headless Chromium |
| Integration | `tests/integration/run.sh` | 102 checks against a real Compose stack: static, PHP, uploads, limits, images, DB, isolation probe, graceful shutdown, restart persistence, metrics/logs |
| Next | `tests/compatibility`, `tests/performance` | WordPress/Laravel/Symfony suites, load tests (phase 11) |

## 8. Known limitations

* Landlock restricts TCP by **port**, not host: a site allowed to reach
  port 3306 could reach any host on 3306. Database credentials are per site,
  so this exposes no data, but host-level egress rules need network
  namespaces (future).
* Application code is mounted read-only and readable by the worker; the
  worker is trusted (Rust, sandboxed, no secrets, no capabilities).
* Generated objects are never garbage-collected yet.
* Change detection is polling (`scan_interval_secs`), not inotify.
* Animated images and GIF are served unmodified.
