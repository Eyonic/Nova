# Development toolchain used by scripts/cargo.sh (no local Rust install required).
ARG RUST_VERSION=1.99
FROM rust:${RUST_VERSION}-trixie
RUN apt-get update \
 && apt-get install -y --no-install-recommends nasm \
 && rm -rf /var/lib/apt/lists/* \
 && rustup component add rustfmt clippy
