#!/bin/sh
# Build both release binaries (amd64 + arm64) inside Ubuntu 24.04 containers,
# so the binaries run on Ubuntu 24.04+, Debian 13+, Fedora 40+, Arch, ...
# arm64 runs under qemu emulation (needs qemu-user-static + binfmt): slow but simple.
# Usage: scripts/dist.sh [amd64|arm64]...   (default: both)
set -eu

engine=$(command -v podman || command -v docker) || { echo "needs podman or docker" >&2; exit 1; }
cd "$(dirname "$0")/.."
mkdir -p dist

for arch in ${*:-amd64 arm64}; do
    echo "== building linux/$arch"
    "$engine" run --rm --platform "linux/$arch" \
        -v "$PWD:/src:ro" -v "$PWD/dist:/out" \
        docker.io/library/ubuntu:24.04 sh -euc '
            export DEBIAN_FRONTEND=noninteractive
            apt-get update -qq
            apt-get install -y -qq --no-install-recommends \
                ca-certificates curl build-essential pkg-config \
                libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev >/dev/null
            curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal
            . "$HOME/.cargo/env"
            cp -r /src /build && cd /build && rm -rf target dist
            sh scripts/package.sh
            cp dist/* /out/
        '
done
ls -l dist
