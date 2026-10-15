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

## Screenshots

The compact top bar shows each provider's icon and remaining quota. Low quota
turns orange or red; unavailable usage appears as an em dash.

![Agent Meter top bar showing five provider icons and remaining quota](docs/screenshots/top-bar.png)

Open the provider menu for reset dates, refresh controls, and desktop-widget
settings. These screenshots show version 0.1.6; percentages are illustrative
snapshots, not live usage.

![Agent Meter provider menu with quota reset dates and widget controls](docs/screenshots/provider-menu.png)

## Design

```text
local adapter(s) -> agent-meterd (Rust) -> $XDG_RUNTIME_DIR/agent-meter/state.json
                                      |                    |
                                      |                    +-- GNOME extension: panel + desktop card
                                      +-- $XDG_STATE_HOME/agent-meter/state.json (stable JSON)
                                      +-- GTK desktop app (optional alternate surface)
```

The daemon binary is `agent-meterd`. The systemd user unit that runs it is
`agent-meter.service` (`systemctl --user restart agent-meter.service`).
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

Press **Super+Alt+U** to show or hide the widget, even over an active app.
Switching focus to another app restores desktop-only visibility. Drag the
**Agent Meter** header to reposition it.

## JSON output (`--json`) and stable state file

`agent-meter --json` prints the merged provider state to stdout in a stable,
documented JSON format. The daemon also atomically writes this JSON to
`$XDG_STATE_HOME/agent-meter/state.json` (defaults to
`~/.local/state/agent-meter/state.json`) on every refresh. This path survives
reboots, unlike the runtime state under `$XDG_RUNTIME_DIR`.

**Schema version 1** — the `schema_version` field will only increment when
breaking changes are made.

```json
{
  "schema_version": 1,
  "providers": [
    {
      "provider": "codex",
      "windows": [
        {
          "name": "five_hour",
          "remaining_percent": 72.0,
          "resets_at": "2026-09-16T22:00:00+00:00"
        },
        {
          "name": "weekly",
          "remaining_percent": 51.0,
          "resets_at": "2026-09-20T14:00:00+00:00"
        }
      ],
      "updated_at": "2026-09-16T20:45:00+00:00",
      "available": true,
      "stale": false
    },
    {
      "provider": "grok",
      "windows": [
        {
          "name": "weekly",
          "remaining_percent": 45.0,
          "resets_at": "2026-09-20T01:45:30+00:00"
        }
      ],
      "updated_at": "2026-09-16T20:45:00+00:00",
      "available": true,
      "stale": true
    }
  ]
}
```

| Field | Type | Description |
| --- | --- | --- |
| `schema_version` | `int` | Always `1` for this schema revision. |
| `providers[].provider` | `string` | Lowercase provider identifier (e.g. `codex`, `grok`, `muse`). |
| `providers[].windows[].name` | `string` | Stable window name: `five_hour`, `weekly`, `session`, `monthly`, etc. |
| `providers[].windows[].remaining_percent` | `float` | Remaining quota from 0.0 to 100.0. |
| `providers[].windows[].resets_at` | `string?` | ISO 8601 datetime with timezone offset. Absent when the provider does not report a trustworthy reset time. Copilot is excluded from the expectation that every window has this field: the Copilot SDK `account.getQuota` returns `resetDate` equal to the fetch time, so no trustworthy reset instant exists. |
| `providers[].updated_at` | `string` | ISO 8601 timestamp of the last successful refresh. |
| `providers[].available` | `bool` | `false` only when the provider status is `unavailable` or `error`. Fresh and stale providers are available. |
| `providers[].stale` | `bool` | `true` only when the provider status is `stale`. Unavailable providers are not stale. |
| `providers[].windows[].forecast` | `object?` | Quota-exhaustion forecast (see below). Absent when forecasting is disabled or no history exists yet. |

## Quota forecasting

The daemon can keep a bounded, credential-free local usage history and
forecast whether a quota will run out before its reset. Forecasting is
**opt-in** via the `forecast` section of `sources.json`:

```json
{
  "refresh_seconds": 300,
  "forecast": {
    "enabled": true,
    "notify_thresholds": [25.0, 10.0, 5.0]
  },
  "sources": []
}
```

| Field | Default | Description |
| --- | --- | --- |
| `enabled` | `false` | Record usage history and compute forecasts. |
| `max_observations_per_window` | `288` | Cap on stored observations per provider window (~24 h at 5-minute cadence). |
| `stale_after_seconds` | `600` | Newest observation older than this marks the forecast `stale`. |
| `notify_thresholds` | `[]` | Remaining-percent thresholds that trigger a notification, e.g. `[25.0, 10.0, 5.0]`. Empty disables notifications. |
| `history_path` | `$XDG_STATE_HOME/agent-meter/usage-history.json` | Override for the history file. |
| `notifications_path` | `$XDG_STATE_HOME/agent-meter/notifications.jsonl` | Override for the notifications log. |

Each refresh records one observation per `fresh` provider window: a
timestamp, the remaining percentage, and the provider-supplied reset time.
Nothing else is stored — no credentials, no raw provider payloads. An upward
jump of 25 percentage points is treated as a quota reset and starts a fresh
cycle, dropping older observations.

Forecasts fit a line through the most recent observations and extrapolate to
zero remaining, then compare against the provider's reset time:

| `forecast.status` | Meaning |
| --- | --- |
| `ok` | Usable forecast; `estimated_exhaustion_at` is set when the quota is depleting, `exhausts_before_reset` compares it with `resets_at`. |
| `insufficient_data` | Fewer than 3 observations, or they span less than 5 minutes. |
| `stale` | The newest observation is older than `stale_after_seconds`; no guess is made. |
| `uncertain` | A trend exists but the linear fit is too noisy (R² < 0.5) to trust. |

`agent-meter --json` attaches the forecast to each window when history is
available:

```json
{
  "name": "five_hour",
  "remaining_percent": 70.0,
  "resets_at": "2026-10-14T22:00:00+00:00",
  "forecast": {
    "status": "ok",
    "estimated_exhaustion_at": "2026-10-14T20:00:00+00:00",
    "exhausts_before_reset": true,
    "consumption_rate_percent_per_hour": 10.0,
    "depleting": true,
    "observation_count": 12
  }
}
```

Notifications fire once per threshold per quota cycle (duplicate suppression):
when remaining drops at or below a threshold, the daemon appends a JSON line
to `notifications.jsonl` and logs a one-line summary to stderr. A quota reset
clears the suppression records so the next cycle notifies again.

To change the shortcut after package installation:

```bash
gsettings --schemadir /usr/share/gnome-shell/extensions/agent-meter@local/schemas \
  set org.gnome.shell.extensions.agent-meter toggle-widget "['<Super><Alt>u']"
```

After an extension JavaScript update, log out and back in to load the new code.

## Resource use

Provider polling remains every five minutes by default. The GNOME extension
checks local state every 15 seconds, but preserves its existing actors when
the data and display state are unchanged. New data, update timestamps, and
manual-refresh state still trigger rendering. Active drags defer rendering.

The Rust daemon is small; external provider CLIs can use substantially more
memory temporarily. A single sequential measurement on an Ubuntu desktop
(September 12, 2026) produced these GNU `time` maximum-RSS results:

| Adapter | Peak RSS | Elapsed |
| --- | ---: | ---: |
| Codex | 119 MiB | 0.60 s |
| Antigravity | 178 MiB | 5.07 s |
| Grok / OMP | 226 MiB | 0.97 s |
| Copilot | 54 MiB | 0.71 s |
| Claude cached state | 14 MiB | 0.02 s |

These are single-run process high-water marks, not simultaneous process-tree
totals or guaranteed budgets. CLI versions and caches affect the results.
The extension shares GNOME Shell's process, so Shell's total RSS is not the
widget's own memory usage. Systemd service memory also includes reclaimable
filesystem cache.

For a local adapter measurement without exposing provider output:

```bash
profile=$(mktemp)
/usr/bin/time -o "$profile" -f 'peak_rss_kib=%M elapsed_s=%e exit=%x' \
  timeout 60 agent-meter-adapter grok >/dev/null 2>/dev/null
cat "$profile"
```

## Progressive Quota Color Scale

Agent Meter uses a continuous, high-contrast color scale to convey remaining quota across all GNOME Shell and GTK surfaces:

| Remaining Quota | Visual Color | Hex Stop | Purpose |
| ---: | --- | :---: | --- |
| **> 40%** | Stable Green | `#38d472` | Normal operational headroom |
| **~30%** | Yellow | `#f6c445` | Usage threshold reached |
| **~20%** | Vivid Orange | `#ff8833` | Quota drawing down |
| **~10%** | Red-Orange | `#ff6644` | Approaching quota exhaustion |
| **0%** | Clear Red | `#ff3b56` | Quota exhausted |
| **Stale / Offline** | Neutral Slate | `#8b949e` | Unchanged cached state |

### Accessibility & Contrast Invariants
* **Dual Indicator**: Numeric percentages are always rendered alongside colors so color vision deficiency does not impede usage tracking.
* **High Foreground Contrast**: All chosen color stops maintain strong contrast (>4.5:1 WCAG AA) against Ubuntu's black top bar (`#000000`) and the dark desktop widget background (`#161920`).
* **Neutral Inactive State**: Stale or disconnected providers remain neutral gray (`#8b949e`) rather than mapping onto the warning/critical spectrum.

## Install on Ubuntu

The packaged release targets Ubuntu 24.04 on `amd64` with GNOME Shell 46.

### Standard APT Repository Installation

Install the cryptographic signing key and add the official Agent Meter repository:

```bash
curl -fsSL https://josh-archer.github.io/agent-meter/agent-meter-archive-keyring.gpg \
  | sudo tee /usr/share/keyrings/agent-meter-archive-keyring.gpg >/dev/null

echo "deb [arch=amd64 signed-by=/usr/share/keyrings/agent-meter-archive-keyring.gpg] https://josh-archer.github.io/agent-meter stable main" \
  | sudo tee /etc/apt/sources.list.d/agent-meter.list >/dev/null

sudo apt update
sudo apt install agent-meter
agent-meter-setup
```

### Manual Package Installation

Alternatively, download the `.deb` directly from the [latest Agent Meter release](https://github.com/Josh-Archer/agent-meter/releases/latest) and install it:

```bash
sudo apt install ./agent-meter_*_amd64.deb
agent-meter-setup
```

### Setup and Verification

`apt` installs the `agent-meterd` binary, the GTK surface, provider adapters,
the systemd user unit `agent-meter.service`, and the GNOME extension.
`agent-meter-setup` then creates `~/.config/agent-meter/sources.json`, installs
GitHub's pinned Copilot SDK in your user data directory, enables
`agent-meter.service`, and enables the extension.
It never copies provider credentials into the package or repository.

Log out and back in once so GNOME Shell discovers the extension, then verify
the installation:

```bash
systemctl --user status agent-meter.service
systemctl --user restart agent-meter.service
gnome-extensions info agent-meter@local
```

`agent-meter.service` is the user unit. It starts the `agent-meterd` binary.
Restart that unit; there is no `agent-meterd` unit.

To update through standard system package management:

```bash
sudo apt update && sudo apt upgrade
```

To remove the installed package:

```bash
systemctl --user disable --now agent-meter.service
sudo apt remove agent-meter
```

The release also includes a filesystem tarball for inspection and manual
packaging workflows. Most Ubuntu desktop users should choose the `.deb`.

## Build from source

### Build prerequisites

Source builds need these development dependencies before the GTK application
can compile:

```bash
sudo apt install libgtk-4-dev build-essential pkg-config
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Restart the terminal after installing Rust. The GNOME extension itself needs
only the already-installed GNOME Shell and GJS. The Rust daemon and core tests
do **not** need GTK4 headers.

### Build and install locally

From this directory:

```bash
cargo test
./install.sh
```

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

### Visual states and provider marks

Each provider uses a bundled, high-contrast monochrome vector replica of its
recognizable product mark. These are local UI identifiers, not vendor-provided
brand assets or an endorsement by the respective providers. While a manual
refresh is running, the top bar shows an animated spinner. A provider command
failure is shown as a muted gray icon and `-`; stale data instead keeps its last
known percentage so it is not mistaken for a sign-in failure.

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

Copilot windows do not include `resets_at`. `account.getQuota` returns
`resetDate` equal to the fetch time, so that timestamp is not a quota reset.
The window's reset label is `Reset unknown`. Copilot is excluded from the rule
that every provider window carries an ISO 8601 `resets_at`.

### Antigravity

Antigravity quota is read directly from the provider-owned command
`agy -p /usage --output-format json`. It returns the real five-hour and weekly
remaining fractions and reset times without reading OAuth storage. The
**Connect** button launches `agy` so its browser sign-in can complete.

### Grok

When OMP is installed and signed in, Agent Meter polls its OAuth usage report
and displays only **Shared weekly** credits. Product-specific Build, Imagine,
and Chat quotas are not substituted for this shared pool. Low and exhausted
quotas remain visible as valid percentages, including 0%, rather than being
treated as connection failures. OMP manages its own session refresh; Agent
Meter never reads its credential store. That report is live, so the adapter
stamps it with the fetch time and does not judge its age from a payload clock.

Without a usable OMP report, the Grok Build log path runs on its own and does
not depend on OMP. Grok Build writes a structured weekly billing snapshot to
`~/.grok/logs/unified.jsonl` after the CLI starts. The adapter reads only
records named `billing: fetched credits config` and keeps the newest snapshot,
including one whose billing period has already ended. It never opens
`~/.grok/auth.json` or reads tokens, identity, prompts, or model output.
The provider's `creditUsagePercent` field is usage consumed, so Agent Meter
converts it to the remaining percentage shown in the widget.
Staleness uses that record's own `ts`. Snapshots older than 30 minutes are
marked `stale` while their last known percentage remains visible. A snapshot
from a billing period that has ended is also `stale`: the percentage from the
snapshot is kept, and a reset instant already in the past is omitted. Start
Grok Build to refresh it. You can tune the age boundary with
`AGENT_METER_GROK_MAX_AGE_SECONDS` (default 1800). A log with no billing
records is `unavailable`.
Stale means usage data needs refreshing, not that the saved Grok sign-in has
expired; Agent Meter only offers **Connect** when a provider is actually
unavailable.

### Codex

Codex is implemented via its local app server (`codex app-server --stdio`). The
adapter communicates over its JSON-RPC stdio protocol and requests
`account/rateLimits/read`. It never reads private account APIs or stored tokens.
Use a status exporter you control from the locally available `/status` endpoint
or usage-dashboard information if you need an alternative source.

### Muse

Muse 1.4.7 has no `usage` subcommand and no passive place to read subscription
usage. Checked against Muse 1.4.7:

- Subscription usage lives only in the memory of the host process that made a
  model call. It arrives as a frame on the model stream
  (`/v1/chat/completions`). After `muse exec "Reply with just: ok"`, a fresh
  `muse serve` `usage/read` returns `{}`.
- The files Muse writes (`session.jsonl`, `cli-*.log`,
  `local-tracing/bootstrap/*.log`, `.msp-view-v1` journal and snapshots,
  `session-index.db`, `session-names.db`, `cron.db`, and `model-catalog/*.json`)
  contain no `usedPercent`, reset, or rate-limit data.
- The binary has no account or usage HTTP endpoint. The only `api.meta.ai`
  paths are `/v1/chat/completions`, `/v1/models`, `/v1/asr/duplex`, `/v1/logs`,
  `/v1/traces`, and `/v1/metrics`. Adapters never read `~/.config/muse/auth.json`.
- `~/.local/share/muse/runtime/muse/ms-*.sock` is a stale peer-messaging
  socket, not a shared host. The TUI and `muse exec` each spawn their own
  in-process host, so a long-lived Agent Meter `muse serve` never sees the
  user's real calls.

There is no passive persistent source. The only way to observe usage is for
Agent Meter's own `muse serve` process to make a model call.

Agent Meter still tries a read-only MSP exchange first. It runs
`muse serve --no-session-log` in a temporary directory, sends `initialize`,
`initialized`, and `usage/read`, and reads stdout until the `usage/read`
response. A `usage/changed` notification that arrives before that response is
the same SubscriptionUsage object. The account `tier` is omitted.

`usage/read` returns `{"usage": ...}` only after a model call in that same
process, and `{}` otherwise. An empty result uses
`$XDG_STATE_HOME/agent-meter/muse-usage.json` (default
`~/.local/state/agent-meter/muse-usage.json`) when Agent Meter has cached an
earlier observation. With neither an observation nor a cache, the provider is
`unavailable`. The detail says that Muse keeps subscription usage only in the
memory of the process that made a model call and writes none to disk. A
missing `muse` binary or a failed `serve` is also `unavailable`.

An opt-in probe can fill that cache. It is off by default. Turn it on with
`AGENT_METER_MUSE_PROBE=1` or with `~/.config/agent-meter/muse.json`:

```json
{"probe": true, "probe_interval_seconds": 3600}
```

When the probe is on, Agent Meter sends one minimal turn on that same
`muse serve` host only when the last probe attempt is older than
`probe_interval_seconds` (default 3600, minimum 900). The gate is
`$XDG_STATE_HOME/agent-meter/muse-probe.json` (default
`~/.local/state/agent-meter/muse-probe.json`): `last_attempt_at` plus the
interval, for every outcome, and for every state of `muse-usage.json`. A
failed probe, a timeout, an empty `usage/read`, and a missing, unreadable, or
corrupt usage cache all wait until that attempt is due again.

Before the probe turn starts, Agent Meter writes `muse-probe.json` with
`last_attempt_at` (ISO 8601 UTC), `outcome` `started`, and a short `detail`.
The write is atomic (temporary file, then rename) and the mode is `0600`.
When the attempt finishes, the same file is updated to `success`, `failure`,
`timeout`, or `empty`, keeping the original `last_attempt_at`. The file has
no tokens and no account tier.

If `muse-probe.json` is missing, a usable `muse-usage.json` that is still
inside the interval suppresses the turn. That keeps a cache written before
attempt tracking from firing a probe on the next refresh. Once
`muse-probe.json` exists, only its attempt time gates the next turn, including
when the usage cache is fresh. If `muse-probe.json` is unreadable or corrupt,
Agent Meter skips the turn, rewrites the file with the current time, `outcome`
`failure`, and detail `probe state unreadable`, and counts that rewrite as an
attempt.

Each finished attempt appends one JSON line to
`$XDG_STATE_HOME/agent-meter/muse-probe.log` (`ts`, `outcome`, `duration_ms`,
`detail`) and prints that same line to stderr. The log keeps the last 200
lines. It has no tokens and no tier id.

The turn is `session/start` with a UUIDv7 `commandId` and a temporary
`workspaceRoot`, then `turn/start` with `reasoningEffort` `minimal` and the
text `Reply with just: ok`. After `usage/changed` or `turn/completed`, it
calls `usage/read` again and, on success, caches the result. The probe waits
at most 120 seconds. `AGENT_METER_MUSE_PROBE_TIMEOUT_SECONDS` overrides that
deadline. An attempt still inside the interval sends no turn.

If the probe is throttled and there is no usable cache, the provider is
`unavailable`. The detail says when the last probe ran, its outcome, and when
the next probe is allowed.

That probe spends quota: one minimal Muse turn per interval, counted from the
last attempt, including attempts that failed, timed out, or returned no usage.
Leave it off if you do not want Agent Meter to call the model.

The rolling window is exposed as id `session` (labeled from
`windowDurationMins`, for example `5h`). The weekly window id is `weekly`.
Remaining percentage is `100 - usedPercent`, clamped to 0 through 100
(`usedPercent` may exceed 100). Reset times are ISO 8601 UTC instants from
`resetsAtMs`. An observation older than `AGENT_METER_MUSE_MAX_AGE_SECONDS`
(default 1800), counted from `observedAtMs`, is `stale`. A window whose reset
time has already passed reports 100% remaining, omits `resets_at`, and marks
the provider `stale`. Override the binary with `AGENT_METER_MUSE_BIN`.

## Validation included here

```bash
python3 tests/check-extension-layout.py
python3 tests/test-adapters.py
```

`check-extension-layout.py` verifies that all six local symbols exist, the panel
loops over all configured providers, and the desktop card uses the desktop-only
focus rule. `test-adapters.py` validates all normalization paths: `normalize_codex`,
`normalize_agy`, `normalize_grok`, `normalize_claude`, `normalize_muse`, Copilot SDK
quota and billing normalization, and the `claude()` state-file reader (both
absent-file and pre-written cases).

After installing Rust, `cargo test` validates the normalized state model, atomic
state-file writes, and the `JsonOutput` public schema conversion. An active GNOME
session is still required for a visual smoke test.

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
