#!/bin/sh
# Build and install for this user: the binary in ~/.local/bin, plus the app
# menu entry and icon (`sshpilot-rs --install`), so desktops show the real icon.
# Usage: scripts/install.sh [--uninstall]
set -eu

cd "$(dirname "$0")/.."
bin="$HOME/.local/bin/sshpilot-rs"

if [ "${1:-}" = --uninstall ]; then
    [ -x "$bin" ] && "$bin" --uninstall
    rm -f "$bin"
    echo "Removed $bin"
    exit
fi

cargo build --release --locked
install -Dm 755 target/release/sshpilot-rs "$bin"
"$bin" --install
