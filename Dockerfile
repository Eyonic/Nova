# NOVA runtime image.
#
# Stage 1 compiles the Rust core; stage 2 is the official PHP-FPM image plus
# the `nova` binary. The compiler and build tools never reach the final image.
# Works with the classic builder and BuildKit alike (no cache mounts needed).

ARG RUST_VERSION=1.99
ARG PHP_VERSION=8.5.11

FROM rust:${RUST_VERSION}-trixie AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends nasm \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src

# Dependency layer: build against stub sources so this layer is only
# invalidated when manifests or the lockfile change.
COPY Cargo.toml Cargo.lock ./
# Patched dependencies ([patch.crates-io] in Cargo.toml).
COPY vendor vendor
COPY crates/nova-config/Cargo.toml crates/nova-config/
COPY crates/nova-http/Cargo.toml crates/nova-http/
COPY crates/nova-runtime-php/Cargo.toml crates/nova-runtime-php/
COPY crates/nova-optimize/Cargo.toml crates/nova-optimize/
COPY crates/nova-core/Cargo.toml crates/nova-core/
COPY crates/nova-cli/Cargo.toml crates/nova-cli/
COPY crates/nova-security/Cargo.toml crates/nova-security/
RUN for c in crates/*/; do mkdir -p "$c/src" && touch "$c/src/lib.rs"; done \
 && echo 'fn main() {}' > crates/nova-cli/src/main.rs \
 && cargo build --release --locked -p nova-cli \
 && rm -rf crates/*/src

COPY crates crates
# COPY keeps source mtimes, which may predate the stub build; force a rebuild of our crates.
RUN find crates -name '*.rs' -exec touch {} + \
 && cargo build --release --locked -p nova-cli \
 && cp target/release/nova /usr/local/bin/nova \
 && /usr/local/bin/nova --version


FROM php:${PHP_VERSION}-fpm-trixie AS runtime
# Extensions common PHP applications (Laravel, WordPress) need. Build
# headers are removed afterwards; only the shared libraries the extensions
# link against stay (same technique as the official PHP images).
RUN set -eux \
 && savedAptMark="$(apt-mark showmanual)" \
 && apt-get update \
 && apt-get install -y --no-install-recommends tini \
      libpng-dev libjpeg62-turbo-dev libwebp-dev libfreetype-dev libicu-dev libzip-dev \
 && docker-php-ext-configure gd --with-jpeg --with-webp --with-freetype \
 && docker-php-ext-install -j"$(nproc)" pdo_mysql mysqli gd intl exif zip bcmath \
 && apt-mark auto '.*' > /dev/null \
 && apt-mark manual $savedAptMark tini > /dev/null \
 && find /usr/local -type f -name '*.so*' -exec ldd '{}' ';' \
      | awk '/=>/ { so = $(NF-1); if (index(so, "/usr/local/") == 1) next; gsub("^/(usr/)?", "", so); print so }' \
      | sort -u | xargs -r dpkg-query --search | cut -d: -f1 | sort -u | xargs -r apt-mark manual > /dev/null \
 && apt-get purge -y --auto-remove -o APT::AutoRemove::RecommendsImportant=false \
 && rm -rf /var/lib/apt/lists/* \
 # NOVA generates the FPM configuration itself; drop the image defaults.
 && rm -f /usr/local/etc/php-fpm.d/*.conf \
 && cp "$PHP_INI_DIR/php.ini-production" "$PHP_INI_DIR/php.ini" \
 && groupadd --system --gid 10001 nova \
 && useradd --system --uid 10001 --gid nova --home-dir /var/lib/nova --shell /usr/sbin/nologin nova \
 && mkdir -p /var/lib/nova /run/nova /etc/nova /srv/sites

COPY --from=build /usr/local/bin/nova /usr/local/bin/nova
COPY config/nova.toml /etc/nova/nova.toml
# Production images carry their sites; development mounts ./sites over this.
COPY sites /srv/sites

# No USER: `nova serve` starts as root with a minimal capability set (see
# compose.yaml), prepares ownership, then runs the HTTP worker as `nova`
# (uid 10001) and every site's PHP under its own uid, all without capabilities.
ENV NOVA_CONFIG=/etc/nova/nova.toml
EXPOSE 8080 8443 8443/udp
VOLUME ["/var/lib/nova"]
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=10s --timeout=5s --start-period=20s --retries=3 CMD ["nova", "health"]
# tini reaps orphaned processes and forwards signals; nova owns everything else.
ENTRYPOINT ["tini", "--", "nova"]
CMD ["serve"]
