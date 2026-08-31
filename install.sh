#!/usr/bin/env bash
set -euo pipefail

source_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
if ! command -v cargo >/dev/null 2>&1 && [[ -x "$HOME/.cargo/bin/cargo" ]]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi
if ! command -v cargo >/dev/null 2>&1; then
    echo "Rust cargo is required; install Rust and reopen your terminal." >&2
    exit 1
fi
data_dir=${XDG_DATA_HOME:-"$HOME/.local/share"}/agent-meter
config_dir=${XDG_CONFIG_HOME:-"$HOME/.config"}/agent-meter
extension_dir="$HOME/.local/share/gnome-shell/extensions/agent-meter@local"
bin_dir="$HOME/.local/bin"

cd "$source_dir"
cargo build --release --features desktop

install -Dm755 target/release/agent-meterd "$bin_dir/agent-meterd"
install -Dm755 target/release/agent-meter "$bin_dir/agent-meter"
install -Dm755 adapters/agent-meter-adapter "$bin_dir/agent-meter-adapter"
install -Dm755 adapters/agent-meter-claude-statusline "$bin_dir/agent-meter-claude-statusline"
install -Dm644 adapters/agent_meter_adapter.py "$bin_dir/agent_meter_adapter.py"
install -Dm755 adapters/agent-meter-connect "$bin_dir/agent-meter-connect"
install -Dm644 adapters/agent_meter_connect.py "$bin_dir/agent_meter_connect.py"
install -Dm644 systemd/user/agent-meter.service "$HOME/.config/systemd/user/agent-meter.service"
mkdir -p "$extension_dir"
cp -a gnome-extension/agent-meter@local/. "$extension_dir/"

# GitHub's SDK bundles the matching Copilot CLI runtime and owns its OAuth flow.
copilot_sdk_dir="$data_dir/copilot-sdk"
mkdir -p "$copilot_sdk_dir"
install -Dm644 adapters/copilot_quota.mjs "$copilot_sdk_dir/copilot_quota.mjs"
if [[ ! -d "$copilot_sdk_dir/node_modules/@github/copilot-sdk" ]]; then
    npm install --prefix "$copilot_sdk_dir" --no-audit --no-fund \
        @github/copilot-sdk@1.0.11 @github/copilot@1.0.82
fi

if [[ ! -f "$config_dir/sources.json" ]]; then
    mkdir -p "$config_dir"
    claude_state_dir=${XDG_STATE_HOME:-$HOME/.local/state}/agent-meter
    install -d -m700 "$claude_state_dir"
    "$bin_dir/agent-meter-adapter" claude > "$claude_state_dir/claude.json"
    chmod 600 "$claude_state_dir/claude.json"
    printf '{\n  "refresh_seconds": 300,\n  "sources": [\n' > "$config_dir/sources.json"
    printf '    {"kind":"command","program":"%s/agent-meter-adapter","args":["codex"]},\n' "$bin_dir" >> "$config_dir/sources.json"
    printf '    {"kind":"command","program":"%s/agent-meter-adapter","args":["agy"]},\n' "$bin_dir" >> "$config_dir/sources.json"
    printf '    {"kind":"command","program":"%s/agent-meter-adapter","args":["grok"]},\n' "$bin_dir" >> "$config_dir/sources.json"
    printf '    {"kind":"command","program":"%s/agent-meter-adapter","args":["copilot"]},\n' "$bin_dir" >> "$config_dir/sources.json"
    printf '    {"kind":"file","path":"%s/claude.json"}\n' "${XDG_STATE_HOME:-$HOME/.local/state}/agent-meter" >> "$config_dir/sources.json"
    printf '  ]\n}\n' >> "$config_dir/sources.json"
fi

systemctl --user daemon-reload
systemctl --user enable agent-meter.service
systemctl --user restart agent-meter.service
if gnome-extensions list --user | grep -qx 'agent-meter@local'; then
    # Keep the extension enabled. GNOME Shell 46 caches ES modules, so an
    # updated JavaScript file takes effect at the next login.
    gnome-extensions enable agent-meter@local
else
    echo "Agent Meter is installed, but GNOME will discover it after your next log in."
fi

printf 'Agent Meter installed. Log out and back in to load extension JavaScript updates.\n'
