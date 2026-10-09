# NOVA

A Docker-native web runtime: its own Rust server core, real PHP through a
supervised PHP-FPM, and a built-in image optimization engine.

One container that serves static sites, executes PHP applications,
optimizes images to AVIF/WebP with responsive variants, connects each site to
its own MariaDB database, isolates sites with per-site uids and Landlock
kernel sandboxes, adds partial page updates to plain HTML/PHP (NOVA Live),
survives restarts, and shuts down gracefully.

## Quick start

```sh
cp .env.example .env          # set real passwords (letters and digits)
docker compose up -d --build --wait
open http://localhost:8080/   # or NOVA_HTTP_PORT from .env
docker compose logs -f nova
docker compose down           # keeps volumes; add -v to wipe data
```

Second site: `curl -H 'Host: second.localhost' http://localhost:8080/`.

## What is running

```
compose
├── nova (read-only FS, minimal caps)
│   ├── nova serve          supervisor (root, 6 caps): ownership, process lifecycle
│   ├── nova worker         uid 10001, no caps, Landlock: HTTP, routing, static, optimizer
│   └── php-fpm × site      one master per site, own uid, Landlock sandbox, FastCGI over unix sockets
└── db (MariaDB 11.8)       one database + user per site
volumes: nova-state (variants, sessions), db-data
```

| Endpoint | Purpose |
|---|---|
| `/_nova/health/live` | process is up |
| `/_nova/health/ready` | PHP pools answer, databases reachable, not draining |
| `/_nova/metrics` | Prometheus metrics |
| `/_nova/optimize/status` | optimizer manifest summary per site |

Images: request `/images/hero.jpg` and NOVA serves AVIF or WebP when the
browser accepts it, and `?w=640` selects a responsive width. In the demo the
hero image goes from 477 KB to 43 KB (AVIF), or 15 KB at 640 px (WebP).

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
tests/integration/run.sh              # end-to-end suite (throwaway stack on :18088)
tests/browser/run.sh                  # NOVA Live in headless Chromium (needs a running stack)
docker compose run --rm nova check --php   # validate config, print generated FPM config
docker compose run --rm nova optimize      # one optimization pass
```

Sites and `config/nova.toml` are bind-mounted in `compose.yaml`, so PHP and
static edits apply immediately; config changes need `docker compose restart nova`.

## Layout

```
crates/
  nova-config        versioned nova.toml schema + validation
  nova-http          connections, graceful drain, safe paths, static files
  nova-runtime-php   FastCGI client, CGI env, PHP-FPM supervisor
  nova-optimize      image pipeline, manifest, negotiation
  nova-core          sites, request lifecycle, health, metrics, startup/shutdown
  nova-security      Landlock sandboxes, ownership helpers
  nova-cli           the `nova` binary
config/nova.toml     platform configuration
sites/               demo sites (example, second)
docker/              toolchain image, MariaDB init
tests/integration/   end-to-end acceptance suite
tests/browser/       NOVA Live browser tests (Playwright/Chromium)
docs/architecture/   specification
```

See [docs/architecture/spec.md](docs/architecture/spec.md) for module
contracts, the request lifecycle, the PHP integration design, the manifest
format, the test strategy and current limitations.
