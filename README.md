# NOVA

A Docker-native web runtime: its own Rust server core, real PHP through a
supervised PHP-FPM, and a built-in image optimization engine.

One container that serves static sites and PHP applications over HTTP/1.1,
HTTP/2 and HTTP/3 with automatic HTTPS, optimizes images to AVIF/WebP and
minifies and precompresses scripts in the background, connects each site to
its own MariaDB database, runs scheduled tasks and queue workers, isolates
sites with per-site uids and Landlock kernel sandboxes, adds partial page
updates to plain HTML/PHP (NOVA Live), reloads its configuration without
dropping a request, and shuts down gracefully.

| | |
|---|---|
| **Protocols** | HTTP/1.1, HTTP/2, HTTP/3 (QUIC), TLS 1.2/1.3, PROXY protocol v1/v2 |
| **Certificates** | Let's Encrypt (ACME TLS-ALPN-01), your own PEM files, persistent self-signed for local hosts |
| **Speed** | brotli/zstd/gzip, precompressed assets, immutable caching of fingerprinted builds (Vite, Mix), OPcache/JIT tuning, AVIF/WebP + responsive images, JS/CSS minification |
| **Site rules** | redirects, canonical host, HTTPS redirect + HSTS, security headers, custom headers, error pages, basic auth, cache rules |
| **Protection** | per-site uid + Landlock sandbox (fails closed), per-IP rate limits and connection caps (TCP and QUIC), upload idle timeout, admin endpoints restricted, explicit trusted proxies |
| **Operations** | zero-downtime reload (SIGHUP), cron tasks + supervised workers per site, Prometheus metrics, structured access logs, health checks |

## What NOVA serves (and what it does not)

| Application | Supported | How |
|---|---|---|
| Static sites, SPAs, build output (Vite, Mix, Hugo, Astro static) | **yes** | served directly, optimized |
| PHP (Laravel, Symfony, WordPress, plain PHP) | **yes** | supervised PHP-FPM per site over FastCGI |
| Node.js, Python (WSGI/ASGI), Go, Ruby, Java or any other HTTP app server | **no** | there is no generic reverse proxy (`proxy_pass`) yet |
| WebSocket or gRPC backends | **no** | NOVA Live uses server-sent events served by NOVA itself |

For a non-PHP backend today, run it next to NOVA and put a reverse proxy
(Traefik, Caddy, nginx) in front that routes by host or path; NOVA then
sits behind that proxy (see `trusted_proxies` in
[http.md](docs/architecture/http.md)). A built-in `[site.proxy]` upstream is
on the roadmap ([vision.md](docs/architecture/vision.md)).

## Quick start

```sh
cp .env.example .env          # set real passwords (letters and digits)
docker compose up -d --build --wait
open http://localhost:8080/   # or NOVA_HTTP_PORT from .env
open https://localhost:8443/  # HTTP/2 + HTTP/3, self-signed for local hosts
docker compose logs -f nova
docker compose kill -s HUP nova   # apply nova.toml changes without downtime
docker compose down           # keeps volumes; add -v to wipe data
```

Second site: `curl -H 'Host: second.localhost' http://localhost:8080/`.

## What is running

```
compose
├── nova (read-only FS, minimal caps)
│   ├── nova serve          supervisor (root, 6 caps): ownership, process lifecycle
│   ├── nova worker         uid 10001, no caps, Landlock: HTTP(S)/QUIC, routing, static, optimizer, ACME
│   ├── php-fpm × site      one master per site, own uid, Landlock sandbox, FastCGI over unix sockets
│   └── tasks/workers       per site: cron tasks and queue workers, same uid and sandbox as its PHP
└── db (MariaDB 11.8)       one database + user per site
volumes: nova-state (variants, sessions), db-data
```

| Endpoint | Purpose |
|---|---|
| `/_nova/health/live` | process is up |
| `/_nova/health/ready` | PHP pools answer, databases reachable, not draining |
| `/_nova/metrics` | Prometheus metrics (private networks only, `server.admin_allow`) |
| `/_nova/optimize/status` | image and Script Optimizer report per site (same restriction) |

Images: request `/images/hero.jpg` and NOVA serves AVIF or WebP when the
browser accepts it, and `?w=640` selects a responsive width. In the demo the
hero image goes from 477 KB to 43 KB (AVIF), or 15 KB at 640 px (WebP).

Scripts and styles are minified (conservatively, validated) and stored as
brotli/zstd/gzip in the background; `docker compose run --rm nova optimize`
prints the savings plus duplicate and unreferenced files to review.

Everything about TLS, proxies, compression, caching, site rules, limits,
tasks and reloads: [docs/architecture/http.md](docs/architecture/http.md).

## Background tasks

```toml
[[site.task]]
name = "scheduler"
schedule = "* * * * *"
command = ["php", "artisan", "schedule:run"]

[[site.worker]]
name = "queue"
command = ["php", "artisan", "queue:work"]
```

## NOVA Live

Partial page updates for plain HTML and PHP, no framework:

```html
<script src="/_nova/live.js" defer></script>
<div id="products" data-nova-live> … </div>
<a href="/products?c=lamps" data-nova-target="#products">Lamps</a>
```

Forms, Back/Forward, polling (`data-nova-poll`) and server-pushed refreshes
(`data-nova-subscribe` + PHP `header('Nova-Publish: notes')`) are included.
Demo: `http://showcase.localhost:8088/live.php`. Reference:
[docs/architecture/live.md](docs/architecture/live.md).

## Development

No local Rust needed; the toolchain runs in a container:

```sh
scripts/cargo.sh test                 # unit tests
scripts/cargo.sh clippy --all-targets
scripts/cargo.sh fmt --all
tests/integration/run.sh              # end-to-end suite, 174 checks (throwaway stack on :18088/:18443)
tests/browser/run.sh                  # NOVA Live in headless Chromium (needs a running stack)
REF=1 tests/performance/run.sh        # load tests vs stock nginx + PHP-FPM (needs oha)
cargo audit                           # RustSec advisories (cargo install cargo-audit)
docker compose run --rm nova check --php   # validate config, print generated FPM config
docker compose run --rm nova optimize      # one optimization pass
```

Sites and `config/nova.toml` are bind-mounted in `compose.yaml`, so PHP and
static edits apply immediately (optimized assets follow within a second);
config changes apply with `docker compose kill -s HUP nova`.

## Layout

```
crates/
  nova-config        versioned nova.toml schema + validation
  nova-http          HTTP/1.1, HTTP/2, HTTP/3, TLS, PROXY protocol, drain, safe paths, static files, compression
  nova-runtime-php   FastCGI client, CGI env, PHP-FPM supervisor
  nova-optimize      image pipeline, Script Optimizer, manifests, negotiation, GC
  nova-core          sites and rules, request lifecycle, TLS/ACME, rate limits, tasks, reload, health, metrics
  nova-security      Landlock sandboxes, ownership helpers
  nova-cli           the `nova` binary
config/nova.toml     platform configuration
sites/               demo sites (example, second)
docker/              toolchain image, MariaDB init
tests/integration/   end-to-end acceptance suite
tests/browser/       NOVA Live browser tests (Playwright/Chromium)
tests/performance/   load tests and regression baseline
docs/architecture/   specification
```

See [docs/architecture/spec.md](docs/architecture/spec.md) for module
contracts, the request lifecycle, the PHP integration design, the manifest
format, the test strategy and current limitations.
