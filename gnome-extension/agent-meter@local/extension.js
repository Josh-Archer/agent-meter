import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import St from 'gi://St';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

const POLL_SECONDS = 15;
const PROGRESS_WIDTH = 150;

function runtimeStateFile() {
    return Gio.File.new_build_filenamev([GLib.get_user_runtime_dir(), 'agent-meter', 'state.json']);
}

function widgetConfigPath() {
    return GLib.build_filenamev([GLib.get_user_config_dir(), 'agent-meter', 'position.json']);
}

function readWidgetConfig() {
    try {
        const file = Gio.File.new_for_path(widgetConfigPath());
        const [ok, bytes] = file.load_contents(null);
        if (ok) {
            const data = JSON.parse(new TextDecoder().decode(bytes));
            if (data && typeof data === 'object' && !Array.isArray(data))
                return data;
        }
    } catch (e) {
        // fallback
    }
    return {};
}

function writeWidgetConfig(data, what) {
    try {
        const configDir = GLib.build_filenamev([GLib.get_user_config_dir(), 'agent-meter']);
        GLib.mkdir_with_parents(configDir, 0o700);
        GLib.chmod(configDir, 0o700);
        const file = Gio.File.new_for_path(widgetConfigPath());
        file.replace_contents(
            JSON.stringify(data),
            null,
            false,
            Gio.FileCreateFlags.REPLACE_DESTINATION,
            null
        );
    } catch (e) {
        console.debug(`Could not save ${what}: ${e.message}`);
    }
}

function loadSavedPosition() {
    const pos = readWidgetConfig();
    if (typeof pos.x === 'number' && typeof pos.y === 'number')
        return [Math.max(0, pos.x), Math.max(28, pos.y)];
    return [36, 60];
}

function loadDesktopVisible() {
    const {desktopVisible} = readWidgetConfig();
    return typeof desktopVisible === 'boolean' ? desktopVisible : true;
}

function savePosition(x, y) {
    const config = readWidgetConfig();
    config.x = Math.round(x);
    config.y = Math.round(y);
    writeWidgetConfig(config, 'widget position');
}

function saveDesktopVisible(visible) {
    const config = readWidgetConfig();
    config.desktopVisible = Boolean(visible);
    writeWidgetConfig(config, 'desktop widget switch');
}

function quotaWindows(provider) {
    return (provider.windows ?? []).filter(window =>
        typeof window.remaining_percent === 'number' &&
        Number.isFinite(window.remaining_percent) &&
        !['availability', 'account'].includes(window.id));
}

function iconFor(extension, symbol) {
    if (symbol === 'agy')
        symbol = 'antigravity';
    const allowed = new Set(['codex', 'antigravity', 'grok', 'copilot', 'claude', 'generic']);
    const name = allowed.has(symbol) ? symbol : 'generic';
    const iconsDir = extension.dir.get_child('icons');
    const symbolicFile = iconsDir.get_child(`${name}-symbolic.svg`);
    if (symbolicFile.query_exists(null)) {
        return new Gio.FileIcon({file: symbolicFile});
    }
    return new Gio.FileIcon({file: iconsDir.get_child(`${name}.svg`)});
}

const COLOR_STOPS = [
    { p: 0,  rgb: [255, 59, 86] },   // 0%: Red #ff3b56
    { p: 10, rgb: [255, 102, 68] },  // 10%: Red-Orange #ff6644
    { p: 20, rgb: [255, 136, 51] },  // 20%: Orange #ff8833
    { p: 30, rgb: [246, 196, 69] },  // 30%: Yellow #f6c445
    { p: 40, rgb: [56, 212, 114] },  // 40%+: Green #38d472
];

function quotaColor(percent, isFresh = true) {
    if (!isFresh) {
        return '#8b949e';
    }
    const p = Math.max(0, Math.min(100, Number(percent) || 0));
    if (p >= 40) {
        return '#38d472';
    }
    for (let i = 0; i < COLOR_STOPS.length - 1; i++) {
        const lower = COLOR_STOPS[i];
        const upper = COLOR_STOPS[i + 1];
        if (p >= lower.p && p <= upper.p) {
            const factor = (p - lower.p) / (upper.p - lower.p);
            const r = Math.round(lower.rgb[0] + (upper.rgb[0] - lower.rgb[0]) * factor);
            const g = Math.round(lower.rgb[1] + (upper.rgb[1] - lower.rgb[1]) * factor);
            const b = Math.round(lower.rgb[2] + (upper.rgb[2] - lower.rgb[2]) * factor);
            return `#${r.toString(16).padStart(2, '0')}${g.toString(16).padStart(2, '0')}${b.toString(16).padStart(2, '0')}`;
        }
    }
    return '#ff3b56';
}

function limitText(provider) {
    if (['unavailable', 'error'].includes(provider.status))
        return 'Unavailable';
    if (provider.status === 'stale')
        return 'Usage unavailable (cached data expired)';
    const windows = provider.windows ?? [];
    const lowest = windows.reduce((current, entry) =>
        current === null || entry.remaining_percent < current.remaining_percent ? entry : current, null);
    if (!lowest)
        return 'No usage data';
    if (lowest.id === 'account')
        return lowest.reset_label ?? 'Active';
    return `${Math.round(lowest.remaining_percent)}% · ${lowest.reset_label ?? 'reset unknown'}`;
}

// Provider state is local JSON. Only an http(s) page with a valid host,
// no embedded userinfo, no control characters, no backslashes, and a valid port (1-65535)
// may be handed to the default browser.
function usagePageUrl(value) {
    if (typeof value !== 'string')
        return null;
    const url = value.trim();
    if (/[\s\x00-\x1f\x7f\\]/.test(url))
        return null;
    if (!/^https?:\/\//i.test(url))
        return null;

    const rest = url.replace(/^https?:\/\//i, '');
    const authorityEnd = rest.search(/[/?#]/);
    const authority = authorityEnd === -1 ? rest : rest.slice(0, authorityEnd);
    if (!authority || authority.includes('@'))
        return null;

    let host;
    let portStr;
    if (authority.startsWith('[')) {
        const closeBracket = authority.indexOf(']');
        if (closeBracket <= 1)
            return null;
        host = authority.slice(1, closeBracket);
        const afterHost = authority.slice(closeBracket + 1);
        if (afterHost.length > 0) {
            if (!afterHost.startsWith(':'))
                return null;
            portStr = afterHost.slice(1);
        }
    } else {
        const colonIndex = authority.lastIndexOf(':');
        if (colonIndex !== -1) {
            host = authority.slice(0, colonIndex);
            portStr = authority.slice(colonIndex + 1);
            if (host.includes(':'))
                return null;
        } else {
            host = authority;
        }
    }

    if (!host || host.includes('[') || host.includes(']'))
        return null;

    if (portStr !== undefined) {
        if (!/^\d+$/.test(portStr))
            return null;
        const port = Number(portStr);
        if (port < 1 || port > 65535)
            return null;
    }

    return url;
}

function openUsagePage(url) {
    const target = usagePageUrl(url);
    if (!target)
        return false;
    try {
        const context = global.create_app_launch_context(0, -1);
        if (!Gio.AppInfo.launch_default_for_uri(target, context))
            throw new Error('no default application for the usage page');
        return true;
    } catch (error) {
        console.warn(`Agent Meter could not open usage page: ${error.message}`);
        return false;
    }
}

const AgentMeterIndicator = GObject.registerClass(
class AgentMeterIndicator extends PanelMenu.Button {
    _init(extension) {
        super._init(0.0, 'Agent Meter');
        this._extension = extension;
        this._desktopVisible = loadDesktopVisible();
        this._keyboardShown = false;
        this._dragState = null;
        this._dragGrab = null;
        this._dragEventId = null;
        this._dragHandle = null;
        this._clampIdle = null;
        this._refreshPending = false;
        this._renderKey = null;
        this._spinnerActor = null;
        this._spinnerSource = null;
        this._spinnerAngle = 0;

        this._icons = new St.BoxLayout({style_class: 'agent-meter-icons'});
        this.add_child(this._icons);

        this._desktop = new St.BoxLayout({
            vertical: true,
            reactive: true,
            can_focus: true,
            track_hover: true,
            style_class: 'agent-meter-desktop',
        });

        const [posX, posY] = loadSavedPosition();
        this._desktop.set_position(posX, posY);

        Main.layoutManager.addChrome(this._desktop, {trackFullscreen: false});
        this._focusChangedId = global.display.connect('notify::focus-window', () => {
            this._keyboardShown = false;
            this._syncDesktopVisibility();
        });

        this._refresh();
        this._syncDesktopVisibility();
        this._timeout = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, POLL_SECONDS, () => {
            this._refresh();
            return GLib.SOURCE_CONTINUE;
        });
    }

    _monitorAt(x, y) {
        return Main.layoutManager.monitors.find(monitor =>
            x >= monitor.x && x < monitor.x + monitor.width &&
            y >= monitor.y && y < monitor.y + monitor.height) ??
            Main.layoutManager.primaryMonitor;
    }

    _clampedPosition(x, y, pointerX = null, pointerY = null) {
        const [currentX, currentY] = this._desktop.get_position();
        const width = Math.max(1, this._desktop.width);
        const height = Math.max(1, this._desktop.height);
        const monitor = this._monitorAt(
            pointerX ?? currentX + width / 2,
            pointerY ?? currentY + height / 2
        );
        const primary = Main.layoutManager.primaryMonitor;
        const hasPanel = monitor.x === primary.x && monitor.y === primary.y &&
            monitor.width === primary.width && monitor.height === primary.height;
        const minX = monitor.x;
        const minY = monitor.y + (hasPanel ? Main.panel.height : 0);
        const maxX = Math.max(minX, monitor.x + monitor.width - width);
        const maxY = Math.max(minY, monitor.y + monitor.height - height);
        return [
            Math.max(minX, Math.min(maxX, x)),
            Math.max(minY, Math.min(maxY, y)),
        ];
    }

    _queueClamp() {
        if (this._clampIdle)
            return;
        this._clampIdle = GLib.idle_add(GLib.PRIORITY_DEFAULT_IDLE, () => {
            this._clampIdle = null;
            if (!this._desktop)
                return GLib.SOURCE_REMOVE;
            const [oldX, oldY] = this._desktop.get_position();
            const [x, y] = this._clampedPosition(oldX, oldY);
            this._desktop.set_position(x, y);
            if (x !== oldX || y !== oldY)
                savePosition(x, y);
            return GLib.SOURCE_REMOVE;
        });
    }

    _finishDrag(persist = true) {
        if (this._dragEventId) {
            this._dragHandle.disconnect(this._dragEventId);
            this._dragEventId = null;
        }
        this._dragHandle?.remove_style_pseudo_class('active');
        this._dragHandle = null;
        this._dragState = null;
        if (this._dragGrab) {
            this._dragGrab.dismiss();
            this._dragGrab = null;
        }
        if (persist && this._desktop) {
            const [x, y] = this._desktop.get_position();
            savePosition(x, y);
        }
    }

    _attachDragHandle(handle) {
        handle.reactive = true;
        handle.track_hover = true;
        handle.connect('button-press-event', (_actor, event) => {
            if (event.get_button() !== Clutter.BUTTON_PRIMARY)
                return Clutter.EVENT_PROPAGATE;
            this._finishDrag(false);
            const [pointerX, pointerY] = event.get_coords();
            const [actorX, actorY] = this._desktop.get_position();
            this._dragState = {pointerX, pointerY, actorX, actorY};
            this._dragHandle = handle;
            handle.add_style_pseudo_class('active');
            this._dragGrab = global.stage.grab(handle);
            // Pointer grabs bypass capture; handle normal events on the grabbed actor.
            this._dragEventId = handle.connect('event', (_actor, dragEvent) => {
                if (dragEvent.type() === Clutter.EventType.MOTION)
                    return this._moveDrag(dragEvent);
                if (dragEvent.type() === Clutter.EventType.BUTTON_RELEASE &&
                    dragEvent.get_button() === Clutter.BUTTON_PRIMARY) {
                    this._finishDrag(true);
                    return Clutter.EVENT_STOP;
                }
                return Clutter.EVENT_PROPAGATE;
            });
            return Clutter.EVENT_STOP;
        });
    }

    _moveDrag(event) {
        if (!this._dragState)
            return Clutter.EVENT_PROPAGATE;
        const [pointerX, pointerY] = event.get_coords();
        const {actorX, actorY} = this._dragState;
        const x = actorX + pointerX - this._dragState.pointerX;
        const y = actorY + pointerY - this._dragState.pointerY;
        const [clampedX, clampedY] = this._clampedPosition(x, y, pointerX, pointerY);
        this._desktop.set_position(clampedX, clampedY);
        return Clutter.EVENT_STOP;
    }

    _syncDesktopVisibility() {
        if (this._desktop)
            this._desktop.visible = this._keyboardShown ||
                (this._desktopVisible && global.display.focus_window === null);
    }

    _setDesktopVisible(active) {
        this._desktopVisible = Boolean(active);
        saveDesktopVisible(this._desktopVisible);
        this._syncDesktopVisibility();
    }

    toggleFromKeyboard() {
        if (this._desktop.visible) {
            this._keyboardShown = false;
            this._setDesktopVisible(false);
        } else {
            this._keyboardShown = true;
            this._setDesktopVisible(true);
        }
        this._queueClamp();
    }

    _readState() {
        try {
            const [ok, bytes] = runtimeStateFile().load_contents(null);
            if (!ok)
                return null;
            return JSON.parse(new TextDecoder().decode(bytes));
        } catch (error) {
            console.debug(`Agent Meter has no readable state: ${error.message}`);
            return null;
        }
    }

    _stopSpinner() {
        if (this._spinnerSource) {
            GLib.Source.remove(this._spinnerSource);
            this._spinnerSource = null;
        }
        this._spinnerActor = null;
        this._spinnerAngle = 0;
    }

    _addSpinner(container) {
        this._stopSpinner();
        const spinner = new St.Icon({
            icon_name: 'process-working-symbolic',
            style_class: 'system-status-icon agent-meter-spinner',
            accessible_name: 'Refreshing Agent Meter usage',
        });
        spinner.set_pivot_point(0.5, 0.5);
        this._spinnerActor = spinner;
        this._spinnerSource = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 80, () => {
            if (!this._spinnerActor)
                return GLib.SOURCE_REMOVE;
            this._spinnerAngle = (this._spinnerAngle + 24) % 360;
            this._spinnerActor.rotation_angle_z = this._spinnerAngle;
            return GLib.SOURCE_CONTINUE;
        });
        container.add_child(spinner);
    }

    _refresh() {
        // Rebuilding the header destroys the active drag handle and its grab.
        if (this._dragState)
            return;
        const state = this._readState();
        // Keep existing actors, menu state, and spinner when nothing visible changed.
        // generated_at is retained so the displayed update time still advances.
        const key = JSON.stringify([state, this._refreshPending, this._desktopVisible]);
        if (key === this._renderKey)
            return;
        this._renderState(state);
        this._renderKey = key;
    }

    _renderState(state) {
        this._icons.destroy_all_children();
        this.menu.removeAll();

        if (!state?.providers?.length) {
            this._addSpinner(this._icons);
            this.menu.addMenuItem(new PopupMenu.PopupMenuItem('Loading Agent Meter data…', {reactive: false}));
            this._rebuildDesktop(null);
            return;
        }

        if (this._refreshPending) {
            this._addSpinner(this._icons);
            this.menu.addMenuItem(new PopupMenu.PopupMenuItem('Refreshing usage…', {reactive: false}));
            this._rebuildDesktop(state);
            return;
        }
        this._stopSpinner();

        // Top bar: Each provider gets its symbol + remaining percent / active status
        for (const provider of state.providers) {
            const isFresh = provider.status === 'fresh';
            const isFailure = ['unavailable', 'error'].includes(provider.status);
            const windows = quotaWindows(provider);
            const hasNumericQuota = windows.length > 0;
            const percentage = hasNumericQuota ? Math.min(...windows.map(w => w.remaining_percent)) : 100;
            const shouldShowQuota = isFresh && hasNumericQuota;

            let pctText = '—';
            if (shouldShowQuota)
                pctText = `${Math.round(percentage)}%`;
            else if (isFresh)
                pctText = 'Active';

            const color = isFailure ? '#8b949e' : quotaColor(percentage, isFresh);

            const pill = new St.BoxLayout({
                style_class: isFailure ? 'agent-meter-pill agent-meter-pill-failed' : 'agent-meter-pill',
                reactive: false,
            });

            const icon = new St.Icon({
                gicon: iconFor(this._extension, provider.icon ?? provider.id),
                style_class: 'system-status-icon agent-meter-panel-icon',
                style: `color: ${color};`,
                accessible_name: `${provider.label}: ${limitText(provider)}`,
            });
            icon.opacity = isFailure ? 115 : 255;
            pill.add_child(icon);

            // Label beside icon
            const pctLabel = new St.Label({
                text: pctText,
                style_class: 'agent-meter-pill-label',
                style: `color: ${color};`,
                y_align: Clutter.ActorAlign.CENTER,
            });
            pill.add_child(pctLabel);

            this._icons.add_child(pill);

            // Menu item
            const item = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
            const menuIcon = new St.Icon({
                gicon: iconFor(this._extension, provider.icon ?? provider.id),
                style_class: 'popup-menu-icon',
                style: `color: ${color};`,
            });
            menuIcon.opacity = isFailure ? 115 : 255;
            item.add_child(menuIcon);
            const labels = new St.BoxLayout({vertical: true, x_expand: true});
            labels.add_child(new St.Label({text: provider.label, style_class: 'agent-meter-provider'}));
            labels.add_child(new St.Label({text: limitText(provider), style_class: 'agent-meter-limit'}));
            item.add_child(labels);
            this.menu.addMenuItem(item);

            const usageUrl = usagePageUrl(provider.usage_url);
            if (usageUrl) {
                const openUsage = new PopupMenu.PopupMenuItem(`  → Open ${provider.label} usage…`);
                openUsage.connect('activate', () => openUsagePage(usageUrl));
                this.menu.addMenuItem(openUsage);
            }

            // A stale snapshot means the provider was previously available but
            // has not emitted a newer usage record. It is not an OAuth failure,
            // so do not steer the user into a needless sign-in flow.
            if (provider.status === 'unavailable') {
                const connect = new PopupMenu.PopupMenuItem(`  → Connect ${provider.label}…`);
                connect.connect('activate', () => this._startControl('connect', provider.id));
                this.menu.addMenuItem(connect);
            }
            if (provider.id === 'grok' && provider.status === 'stale') {
                const refreshGrok = new PopupMenu.PopupMenuItem('  → Open Grok Build to refresh…');
                refreshGrok.connect('activate', () => this._startControl('open', provider.id));
                this.menu.addMenuItem(refreshGrok);
            }
        }

        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

        // Desktop widget toggle switch
        const desktopToggle = new PopupMenu.PopupSwitchMenuItem('Desktop Widget', this._desktopVisible);
        desktopToggle.connect('toggled', (_item, active) => {
            this._setDesktopVisible(active);
        });
        this.menu.addMenuItem(desktopToggle);

        const resetPos = new PopupMenu.PopupMenuItem('Reset Widget Position');
        resetPos.connect('activate', () => {
            savePosition(36, 60);
            if (this._desktop)
                this._desktop.set_position(36, 60);
            this._queueClamp();
        });
        this.menu.addMenuItem(resetPos);

        const refresh = new PopupMenu.PopupMenuItem('Refresh usage now');
        refresh.connect('activate', () => this._startControl('refresh'));
        this.menu.addMenuItem(refresh);

        const timeStr = new Date(state.generated_at).toLocaleTimeString([], {hour: '2-digit', minute: '2-digit'});
        this.menu.addMenuItem(new PopupMenu.PopupMenuItem(`Updated ${timeStr}`, {reactive: false}));

        this._rebuildDesktop(state);
    }

    _startControl(action, provider = null) {
        const home = GLib.get_home_dir();
        const localConnect = GLib.build_filenamev([home, '.local', 'bin', 'agent-meter-connect']);
        const providerCommand = ['connect', 'open'].includes(action);
        const executable = providerCommand
            ? (GLib.file_test(localConnect, GLib.FileTest.IS_EXECUTABLE)
                ? localConnect
                : (GLib.find_program_in_path('agent-meter-connect') ?? 'agent-meter-connect'))
            : 'systemctl';
        const argv = providerCommand
            ? [executable, ...(action === 'open' ? ['--mode', 'open'] : []), provider]
            : [executable, '--user', 'restart', 'agent-meter.service'];
        try {
            Gio.Subprocess.new(argv, Gio.SubprocessFlags.NONE);
            if (action === 'refresh') {
                this._refreshPending = true;
                this._refresh();
            }
            GLib.timeout_add(GLib.PRIORITY_DEFAULT, 1500, () => {
                try {
                    if (action === 'refresh')
                        this._refreshPending = false;
                    this._refresh();
                } catch (error) {
                    if (action === 'refresh')
                        this._refreshPending = false;
                    console.warn(`Agent Meter refresh completion failed: ${error.message}`);
                }
                return GLib.SOURCE_REMOVE;
            });
            GLib.timeout_add(GLib.PRIORITY_DEFAULT, 3500, () => {
                try {
                    this._refresh();
                } catch (error) {
                    console.warn(`Agent Meter refresh follow-up failed: ${error.message}`);
                }
                return GLib.SOURCE_REMOVE;
            });
        } catch (error) {
            if (action === 'refresh')
                this._refreshPending = false;
            console.warn(`Agent Meter could not ${action}: ${error.message}`);
            if (action === 'refresh') {
                try {
                    this._refresh();
                } catch (refreshError) {
                    console.warn(`Agent Meter refresh recovery failed: ${refreshError.message}`);
                }
            }
        }
    }

    _rebuildDesktop(state) {
        this._finishDrag(false);
        this._desktop.destroy_all_children();

        const header = new St.BoxLayout({style_class: 'agent-meter-desktop-header agent-meter-drag-handle'});
        header.add_child(new St.Label({text: '⠿  Agent Meter', style_class: 'agent-meter-desktop-title', x_expand: true}));
        if (state?.generated_at) {
            const timeStr = new Date(state.generated_at).toLocaleTimeString([], {hour: '2-digit', minute: '2-digit'});
            header.add_child(new St.Label({text: `Updated ${timeStr}`, style_class: 'agent-meter-desktop-time'}));
        }
        this._attachDragHandle(header);
        this._desktop.add_child(header);

        if (!state?.providers?.length) {
            this._desktop.add_child(new St.Label({text: 'Waiting for local usage data…', style_class: 'agent-meter-limit'}));
            this._syncDesktopVisibility();
            this._queueClamp();
            return;
        }

        for (const provider of state.providers) {
            const isFresh = provider.status === 'fresh';
            const isFailure = ['unavailable', 'error'].includes(provider.status);
            const isStale = provider.status === 'stale';
            const windows = quotaWindows(provider);
            const hasNumericQuota = windows.length > 0;
            const shouldShowQuota = isFresh && hasNumericQuota;

            const cardClass = isFailure ? 'agent-meter-desktop-card agent-meter-desktop-card-failed' :
                (isFresh ? 'agent-meter-desktop-card' : 'agent-meter-desktop-card agent-meter-desktop-card-unavailable');
            const card = new St.BoxLayout({vertical: true, style_class: cardClass});

            const heading = new St.BoxLayout({});
            const percentage = hasNumericQuota ? Math.min(...windows.map(w => w.remaining_percent)) : 100;
            const color = isFailure ? '#8b949e' : quotaColor(percentage, isFresh);

            const icon = new St.Icon({
                gicon: iconFor(this._extension, provider.icon ?? provider.id),
                style_class: 'agent-meter-desktop-icon',
                style: `color: ${color};`,
            });
            icon.opacity = isFailure ? 115 : 255;
            heading.add_child(icon);
            heading.add_child(new St.Label({text: provider.label, x_expand: true, style_class: 'agent-meter-provider'}));

            if (isFailure || isStale) {
                heading.add_child(new St.Label({text: '—', style_class: 'agent-meter-failure-mark'}));
            } else if (!hasNumericQuota) {
                heading.add_child(new St.Label({text: 'Active', style_class: 'agent-meter-badge-fresh'}));
            }
            card.add_child(heading);

            if (shouldShowQuota) {
                for (const window of windows) {
                    const row = new St.BoxLayout({style_class: 'agent-meter-desktop-window'});
                    row.add_child(new St.Label({text: window.label, style_class: 'agent-meter-window-label'}));

                    const bar = new St.Widget({style_class: 'agent-meter-progress', width: PROGRESS_WIDTH});
                    const percent = Math.max(0, Math.min(100, window.remaining_percent));
                    const windowColor = quotaColor(percent, isFresh);

                    bar.add_child(new St.Widget({
                        style_class: isFresh ? 'agent-meter-progress-fill' : 'agent-meter-progress-fill agent-meter-progress-stale',
                        style: `background-color: ${windowColor};`,
                        width: Math.round(PROGRESS_WIDTH * percent / 100),
                    }));
                    row.add_child(bar);

                    const staleSuffix = !isFresh ? ' · stale' : '';
                    const valueText = `${Math.round(percent)}%  ${window.reset_label ?? 'reset unknown'}${staleSuffix}`;
                    row.add_child(new St.Label({
                        text: valueText,
                        style_class: 'agent-meter-window-value',
                        style: `color: ${windowColor};`,
                    }));
                    card.add_child(row);
                }
                // Plan names and "N of M used" live in provider.detail. Bars do not replace that line.
                if (typeof provider.detail === 'string' && provider.detail.length > 0) {
                    const detailRow = new St.BoxLayout({style_class: 'agent-meter-desktop-window'});
                    detailRow.add_child(new St.Label({
                        text: provider.detail,
                        style_class: 'agent-meter-limit',
                        x_expand: true,
                    }));
                    card.add_child(detailRow);
                }
            } else {
                const detailRow = new St.BoxLayout({style_class: 'agent-meter-desktop-window'});
                const detailText = isStale ? 'Usage unavailable; cached data expired.' :
                    (provider.detail ?? 'Signed in and operational');
                detailRow.add_child(new St.Label({text: detailText, style_class: 'agent-meter-limit', x_expand: true}));
                card.add_child(detailRow);
            }

            this._desktop.add_child(card);
        }

        this._syncDesktopVisibility();
        this._queueClamp();
    }

    destroy() {
        if (this._timeout) {
            GLib.Source.remove(this._timeout);
            this._timeout = null;
        }
        if (this._clampIdle) {
            GLib.Source.remove(this._clampIdle);
            this._clampIdle = null;
        }
        this._stopSpinner();
        this._finishDrag(false);
        if (this._focusChangedId) {
            global.display.disconnect(this._focusChangedId);
            this._focusChangedId = null;
        }
        if (this._desktop) {
            Main.layoutManager.removeChrome(this._desktop);
            this._desktop.destroy();
            this._desktop = null;
        }
        super.destroy();
    }
});

export default class AgentMeterExtension extends Extension {
    enable() {
        this._indicator = new AgentMeterIndicator(this);
        Main.panel.addToStatusArea(this.uuid, this._indicator, 0, 'right');
        Main.wm.addKeybinding('toggle-widget', this.getSettings(),
            Meta.KeyBindingFlags.NONE, Shell.ActionMode.NORMAL,
            () => this._indicator.toggleFromKeyboard());
    }

    disable() {
        Main.wm.removeKeybinding('toggle-widget');
        this._indicator?.destroy();
        this._indicator = null;
    }
}
