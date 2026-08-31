# Agent Meter

Agent Meter is a local Ubuntu/GNOME desktop widget for coding-agent quota
visibility. It shows a compact **icon for every configured provider** in the
top bar and an expanded, desktop-only card with separate five-hour and weekly
bars where the provider offers them.

The provider-specific symbols are bundled local SVGs: `codex`, `antigravity`,
`grok`, `copilot`, `claude`, and `generic`. They are lightweight UI symbols,
not downloaded artwork, and they never reveal account information. Every
provider configured in the daemon state appears both in the top bar and in the
expanded desktop card. The panel uses true `-symbolic.svg` variants plus a
high-contrast pill so the symbols remain visible on Ubuntu's black top bar.

## Design

```text
local adapter(s) -> agent-meterd (Rust) -> $XDG_RUNTIME_DIR/agent-meter/state.json
                                      |                    |
                                      |                    +-- GNOME extension: panel + desktop card
                                      +-- GTK desktop app (optional alternate surface)
```

`agent-meterd` only accepts a normalized, credential-free `ProviderState`
document from a fixed executable or local file. It never invokes a shell,
persists provider tokens, stores raw provider responses, or sends data off the
machine. The state file is atomically replaced, so the widget never reads a
partial result.

On GNOME Wayland there is no supported third-party window role that stays
behind every app window. The extension therefore owns the actual desktop card:
it is visible only when no application window has keyboard focus (for example
on an empty workspace or after showing the desktop), and it hides as soon as a
normal app gets focus. Drag the highlighted **Agent Meter** header to move it;
the saved position is clamped to the monitor so the card cannot be lost
off-screen. The top-bar row is always available.

## Install prerequisites

The current machine still needs these development dependencies before the
GTK application can compile:

```bash
sudo apt install libgtk-4-dev build-essential pkg-config
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Restart the terminal after installing Rust. The GNOME extension itself needs
only the already-installed GNOME Shell and GJS. The Rust daemon and core tests
do **not** need GTK4 headers.

## Build and install locally

From this directory:

```bash
cargo test
./install.sh
```

For a tagged Ubuntu release, download the `.deb` from GitHub Releases and
install and configure it with:

```bash
sudo apt install ./agent-meter_VERSION_amd64.deb
agent-meter-setup
```

A portable
tarball is published alongside it. The package installs the daemon, optional
GTK surface, adapters, systemd user unit, and GNOME extension; it does not
include provider credentials or machine-local state.

GNOME Shell only discovers newly copied user extensions when it starts. Log
out and back in once after the first install, then verify with
`gnome-extensions info agent-meter@local`. Run `agent-meter` only if you want
a normal movable GTK window as well; the GNOME-managed desktop card is the
default desktop-only surface.

## Connect providers

Copy [data/sources.example.json](data/sources.example.json) to
`~/.config/agent-meter/sources.json`, then point each source at a small local
adapter. An adapter must write exactly one JSON object matching
[data/provider-state.example.json](data/provider-state.example.json) to
standard output. `kind: "file"` accepts the same object from a local file.

The daemon marks an adapter as unavailable instead of deleting the provider if
the program is missing or returns malformed JSON. A provider state must use a
unique lowercase `id`, one of the bundled `icon` values (or it falls back to
`generic`), and one or more windows. Use `status: "fresh"`, `"stale"`,
`"unavailable"`, or `"error"` to make data quality visible in the UI.

### Claude Code

Claude Code exposes rate limits through its status-line JSON. Configure its
`statusLine` command in your Claude Code settings to point at the bundled helper:

```json
{
  "statusLine": {
    "type": "command",
    "command": "/home/you/.local/bin/agent-meter-claude-statusline"
  }
}
```

If you already have a custom status-line wrapper, pipe its JSON output to this
command or call it from within the wrapper. The helper reads JSON on stdin and
atomically writes `~/.local/state/agent-meter/claude.json`. It retains no raw
status-line input and no credentials. Once at least one payload has been received,
`agent_meter_adapter.py claude` reads that file and emits a normalized state.
Before the first payload arrives, the adapter returns `status: "unavailable"` with
instructions pointing back to this setup step.

### GitHub Copilot

GitHub Copilot uses the official Copilot SDK's `account.getQuota` RPC. It reports
the authenticated user's entitlement, consumed requests, remaining percentage,
and reset date directly, so no hand-entered allowance is required. `install.sh`
keeps the SDK and its matching Copilot CLI runtime under
`~/.local/share/agent-meter/copilot-sdk`.

The **Connect** action launches that provider-owned Copilot CLI. Enter `/login` in
the terminal if it is not already authenticated. Agent Meter never receives or
reads the OAuth credential; the SDK runtime uses the system keychain.

### Antigravity

Antigravity quota is read directly from the provider-owned command
`agy -p /usage --output-format json`. It returns the real five-hour and weekly
remaining fractions and reset times without reading OAuth storage. The
**Connect** button launches `agy` so its browser sign-in can complete.

### Grok

Grok Build writes a structured weekly billing snapshot to its private local log
after the CLI starts. The adapter reads only records named
`billing: fetched credits config`, selecting the newest unexpired period. It never
opens `~/.grok/auth.json` or reads tokens, identity, prompts, or model output.
Start Grok Build once after login to refresh the snapshot.

### Codex

Codex is implemented via its local app server (`codex app-server --stdio`). The
adapter communicates over its JSON-RPC stdio protocol and requests
`account/rateLimits/read`. It never reads private account APIs or stored tokens.
Use a status exporter you control from the locally available `/status` endpoint
or usage-dashboard information if you need an alternative source.

## Validation included here

```bash
python3 tests/check-extension-layout.py
python3 tests/test-adapters.py
```

`check-extension-layout.py` verifies that all six local symbols exist, the panel
loops over all configured providers, and the desktop card uses the desktop-only
focus rule. `test-adapters.py` validates all normalization paths: `normalize_codex`,
`normalize_agy`, `normalize_grok`, `normalize_claude`, Copilot SDK quota and billing
normalization, and the `claude()` state-file reader (both absent-file and
pre-written cases).

After installing Rust, `cargo test` validates the normalized state model and atomic
state-file writes. An active GNOME session is still required for a visual smoke test.

## Privacy and repository safety

Agent Meter is designed to be safe to publish and reuse. Provider credentials,
raw provider responses, prompts, model output, account identifiers, and runtime
logs stay outside the repository. Keep local `sources.json`, provider state,
and exported logs out of commits; `.gitignore` includes the common local-state
patterns. Adapters should emit only the normalized, credential-free schema
illustrated in `data/provider-state.example.json`.

Contributions should run both Python checks and `cargo test --all-features`
before opening a pull request. Never add a real token, private log, or personal
home-directory path to fixtures or documentation.
