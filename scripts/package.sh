#!/bin/sh
# Build the release binary as dist/sshpilot-rs-<version>-linux-<arch> (executable),
# with its checksum next to it in dist/sshpilot-rs-<version>-linux-<arch>.sha256.
# Usage: scripts/package.sh   (run from the repo root, on the machine/arch you build for)
set -eu

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
case "$(uname -m)" in
    x86_64) arch=amd64 ;;
    aarch64 | arm64) arch=arm64 ;;
    *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

cargo build --release --locked
target=${CARGO_TARGET_DIR:-target}

name="sshpilot-rs-$version-linux-$arch"
mkdir -p dist
install -m 755 "$target/release/sshpilot-rs" "dist/$name"
(cd dist && sha256sum "$name" > "$name.sha256")
echo "dist/$name"
