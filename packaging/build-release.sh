#!/usr/bin/env bash
set -euo pipefail

if ! command -v cargo >/dev/null 2>&1 && [[ -x "$HOME/.cargo/bin/cargo" ]]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi
if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo is required to build release packages" >&2
    exit 1
fi

version=${1:-0.1.0}
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]]; then
    echo "version must look like 1.2.3" >&2
    exit 2
fi

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
dist="$root/dist"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
arch=$(dpkg --print-architecture 2>/dev/null || echo amd64)
name="agent-meter_${version}_${arch}"
debroot="$stage/$name"

cd "$root"
cargo build --release --features desktop
install -Dm755 target/release/agent-meterd "$debroot/usr/bin/agent-meterd"
install -Dm755 target/release/agent-meter "$debroot/usr/bin/agent-meter"
install -Dm755 adapters/agent-meter-adapter "$debroot/usr/bin/agent-meter-adapter"
install -Dm755 adapters/agent-meter-claude-statusline "$debroot/usr/bin/agent-meter-claude-statusline"
install -Dm755 adapters/agent-meter-connect "$debroot/usr/bin/agent-meter-connect"
# The tiny Python launchers import from their own directory, so keep their
# credential-safe implementation modules beside them.
install -Dm644 adapters/agent_meter_adapter.py "$debroot/usr/bin/agent_meter_adapter.py"
install -Dm644 adapters/agent_meter_connect.py "$debroot/usr/bin/agent_meter_connect.py"
install -Dm644 adapters/copilot_quota.mjs "$debroot/usr/lib/agent-meter/copilot_quota.mjs"
sed 's|ExecStart=%h/.local/bin/agent-meterd|ExecStart=/usr/bin/agent-meterd|' \
    systemd/user/agent-meter.service > "$stage/agent-meter.service"
install -Dm644 "$stage/agent-meter.service" "$debroot/usr/lib/systemd/user/agent-meter.service"
install -Dm755 packaging/agent-meter-setup "$debroot/usr/bin/agent-meter-setup"
mkdir -p "$debroot/usr/share/gnome-shell/extensions"
cp -a gnome-extension/agent-meter@local "$debroot/usr/share/gnome-shell/extensions/"
find "$debroot/usr/share/gnome-shell/extensions/agent-meter@local" -type d -exec chmod 755 {} +
find "$debroot/usr/share/gnome-shell/extensions/agent-meter@local" -type f -exec chmod 644 {} +
install -Dm644 README.md "$debroot/usr/share/doc/agent-meter/README.md"
install -Dm644 LICENSE "$debroot/usr/share/doc/agent-meter/LICENSE"

mkdir -p "$debroot/DEBIAN"
cat > "$debroot/DEBIAN/control" <<EOF
Package: agent-meter
Version: $version
Section: utils
Priority: optional
Architecture: $arch
Maintainer: Agent Meter contributors
Description: Local GNOME usage meter for coding-agent harnesses
 A credential-safe Ubuntu desktop widget and GNOME top-bar indicator.
Depends: python3, nodejs, npm, libgtk-4-1
EOF
mkdir -p "$dist"
dpkg-deb --root-owner-group --build "$debroot" "$dist/$name.deb" >/dev/null

tar -C "$debroot" -czf "$dist/$name.tar.gz" .
echo "Created $dist/$name.deb and $dist/$name.tar.gz"
