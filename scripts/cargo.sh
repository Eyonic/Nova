#!/usr/bin/env bash
# Run cargo inside the pinned NOVA toolchain container.
# Usage: scripts/cargo.sh build | test | clippy --all-targets | fmt ...
set -euo pipefail
cd "$(dirname "$0")/.."
IMAGE=nova-toolchain:1.99
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  docker build -q -t "$IMAGE" -f docker/toolchain.Dockerfile docker >/dev/null
fi
# Named volumes keep the registry and build cache between runs; the source is
# mounted as the calling user so generated files (Cargo.lock) are not root-owned.
for v in nova-cargo-home nova-cargo-target; do
  docker volume inspect "$v" >/dev/null 2>&1 || {
    docker volume create "$v" >/dev/null
    docker run --rm -v "$v:/v" "$IMAGE" chown "$(id -u):$(id -g)" /v
  }
done
exec docker run --rm ${NOVA_TTY:-} \
  --user "$(id -u):$(id -g)" \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target -e CARGO_TERM_COLOR=always \
  -v nova-cargo-home:/cargo -v nova-cargo-target:/target \
  -v "$PWD:/src" -w /src \
  "$IMAGE" cargo "$@"
