#!/bin/sh
# Build the release binary and pack it as dist/sshpilot-rs-<version>-linux-<arch>.tar.gz
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
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name/share/applications" "$stage/$name/share/icons/hicolor/scalable/apps"
install -m 755 "$target/release/sshpilot-rs" "$stage/$name/"
install -m 644 README.md LICENSE "$stage/$name/"
install -m 644 data/io.github.emad_ramlawi.SshPilotRs.desktop "$stage/$name/share/applications/"
install -m 644 data/icons/io.github.emad_ramlawi.SshPilotRs.svg "$stage/$name/share/icons/hicolor/scalable/apps/"

mkdir -p dist
tar -C "$stage" --owner=0 --group=0 --numeric-owner -czf "dist/$name.tar.gz" "$name"
(cd dist && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "dist/$name.tar.gz"
