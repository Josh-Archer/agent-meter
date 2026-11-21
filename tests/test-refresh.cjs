const assert = require('node:assert/strict');
const fs = require('node:fs');
const source = fs.readFileSync('gnome-extension/agent-meter@local/extension.js', 'utf8');
const monitorStart = source.indexOf('    _startStateMonitor() {');
const monitorEnd = source.indexOf('    _readState() {', monitorStart);
const start = source.indexOf('    _refresh() {');
const end = source.indexOf('    _renderState(state) {', start);
const controlStart = source.indexOf('    _startControl(action, provider = null) {');
const controlEnd = source.indexOf('    _rebuildDesktop(state) {', controlStart);
let timeoutCallbacks = [];
let fileTestImpl = () => false;
let findProgramImpl = () => null;
let spawnedArgvs = [];
const GLib = {
    get_home_dir: () => '/home/test',
    get_user_runtime_dir: () => '/run/user/1000',
    build_filenamev: parts => parts.join('/'),
    file_test: (path, test) => fileTestImpl(path, test),
    find_program_in_path: name => findProgramImpl(name),
    FileTest: {IS_EXECUTABLE: 1},
    PRIORITY_DEFAULT: 0,
    SOURCE_REMOVE: false,
    timeout_add: (priority, interval, fn) => {
        timeoutCallbacks.push(fn);
        return timeoutCallbacks.length;
    },
};
let subprocessShouldThrow = true;
let monitorShouldThrow = false;
let monitoredPath = null;
let monitorChangedCallback = null;
const Gio = {
    FileMonitorFlags: {NONE: 0},
    File: {
        new_build_filenamev: parts => ({
            monitor_directory: (flags, cancellable) => {
                assert.equal(flags, 0);
                assert.equal(cancellable, null);
                if (monitorShouldThrow)
                    throw new Error('monitor failed');
                monitoredPath = parts.join('/');
                return {
                    connect: (signal, cb) => {
                        assert.equal(signal, 'changed');
                        monitorChangedCallback = cb;
                        return 99;
                    },
                    disconnect: () => {},
                    cancel: () => {},
                };
            },
        }),
    },
    SubprocessFlags: {NONE: 0},
    Subprocess: {
        new: argv => {
            spawnedArgvs.push(argv);
            if (subprocessShouldThrow)
                throw new Error('spawn failed');
            return {};
        },
    },
};
const Refresh = eval(`(class {
${source.slice(monitorStart, monitorEnd)}
${source.slice(start, end)}
${source.slice(controlStart, controlEnd)}
})`);
const widget = new Refresh();
widget._refreshPending = false;
let state = {generated_at: '2026-09-12T10:00:00Z', providers: [{id: 'grok', status: 'fresh', windows: [{remaining_percent: 4}]}]};
let renders = 0;
let isSpinning = false;
widget._readState = () => structuredClone(state);
widget._renderState = () => { renders++; isSpinning = Boolean(widget._refreshPending); };
widget._refresh();
for (let i = 0; i < 20; i++) widget._refresh();
assert.equal(renders, 1, 'Unchanged polling must preserve actors');
state.providers[0].windows[0].remaining_percent = 3;
widget._refresh();
assert.equal(renders, 2);
state.generated_at = '2026-09-12T10:05:00Z';
widget._refresh();
assert.equal(renders, 3, 'Update timestamp remains current');
widget._refreshPending = true;
widget._refresh();
widget._refresh();
assert.equal(renders, 4, 'Spinner starts once and is not recreated');
assert.equal(isSpinning, true);
widget._refreshPending = false;
widget._refresh();
assert.equal(renders, 5, 'Spinner clears even when data is unchanged');
assert.equal(isSpinning, false);
widget._desktopVisible = false;
widget._refresh();
assert.equal(renders, 6, 'Menu reflects keyboard visibility changes');
state = null;
widget._dragState = {};
widget._refresh();
assert.equal(renders, 6, 'Drag still defers rendering');
widget._dragState = null;
widget._refresh();
widget._refresh();
assert.equal(renders, 7, 'Missing data renders once');
state = {providers: []};
widget._renderState = () => { throw new Error('render failure'); };
assert.throws(() => widget._refresh());
widget._renderState = () => { renders++; isSpinning = Boolean(widget._refreshPending); };
widget._refresh();
assert.equal(renders, 8, 'Failed rendering must be retried');

// Failed refresh start must clear pending flag and not leave a permanent spinner
state = {generated_at: '2026-09-12T10:10:00Z', providers: [{id: 'grok', status: 'fresh', windows: [{remaining_percent: 4}]}]};
widget._refresh();
assert.equal(isSpinning, false);
assert.equal(widget._refreshPending, false);

const warnings = [];
const originalWarn = console.warn;
console.warn = msg => warnings.push(msg);
try {
    widget._startControl('refresh');
} finally {
    console.warn = originalWarn;
}
assert.equal(widget._refreshPending, false, 'Pending flag must be cleared when subprocess start throws');
assert.equal(isSpinning, false, 'No permanent spinner remains when subprocess start throws');
assert.equal(timeoutCallbacks.length, 0, 'No refresh timeouts should be scheduled when spawn fails');
assert.ok(warnings.some(msg => msg.includes('Agent Meter could not refresh: spawn failed')), 'Error should be logged');

// Subsequent polling must not resurrect the spinner
widget._refresh();
assert.equal(isSpinning, false, 'Subsequent polling does not display spinner');
assert.equal(widget._refreshPending, false);

// Successful spawn sets pending flag and clears upon completion
subprocessShouldThrow = false;
widget._startControl('refresh');
assert.equal(widget._refreshPending, true, 'Pending flag is set when subprocess starts successfully');
assert.equal(isSpinning, true, 'Spinner is displayed while refresh is pending');
assert.equal(timeoutCallbacks.length, 2);

// Completion timer clears pending flag and spinner
timeoutCallbacks[0]();
assert.equal(widget._refreshPending, false, 'Pending flag is cleared when refresh completes');
assert.equal(isSpinning, false, 'Spinner is cleared when refresh completes');

// Async completion error also clears pending flag
timeoutCallbacks = [];
widget._startControl('refresh');
assert.equal(widget._refreshPending, true);
widget._renderState = () => { throw new Error('render error in async completion'); };
console.warn = msg => warnings.push(msg);
try {
    timeoutCallbacks[0]();
} finally {
    console.warn = originalWarn;
}
assert.equal(widget._refreshPending, false, 'Pending flag is cleared on async completion error');

// Runtime directory monitor refreshes on state.json / usage-today.json and ignores unrelated files
widget._renderState = () => { renders++; isSpinning = Boolean(widget._refreshPending); };
let refreshCalls = 0;
const origRefresh = widget._refresh.bind(widget);
widget._refresh = () => {
    refreshCalls++;
    origRefresh();
};

widget._startStateMonitor();
assert.equal(monitoredPath, '/run/user/1000/agent-meter');
assert.equal(widget._stateMonitorChangedId, 99);
assert.ok(typeof monitorChangedCallback === 'function');

const rendersBeforeStateChange = renders;
widget._refreshPending = true;
state = {generated_at: '2026-09-12T10:15:00Z', providers: [{id: 'grok', status: 'fresh', windows: [{remaining_percent: 2}]}]};
monitorChangedCallback(widget._stateMonitor, {get_basename: () => 'state.json'}, null, 0);
assert.equal(refreshCalls, 1, 'state.json change triggers _refresh()');
assert.equal(widget._refreshPending, false, 'state.json change clears _refreshPending');
assert.equal(isSpinning, false, 'state.json change clears spinner');
assert.equal(renders, rendersBeforeStateChange + 1, 'state.json change renders updated state');

widget._refreshPending = true;
monitorChangedCallback(widget._stateMonitor, null, {get_basename: () => 'usage-today.json'}, 0);
assert.equal(refreshCalls, 2, 'usage-today.json change via otherFile triggers _refresh()');
assert.equal(widget._refreshPending, true, 'usage-today.json change preserves _refreshPending');

monitorChangedCallback(widget._stateMonitor, {get_basename: () => 'position.json'}, null, 0);
monitorChangedCallback(widget._stateMonitor, null, {get_basename: () => 'other.tmp'}, 0);
monitorChangedCallback(widget._stateMonitor, null, null, 0);
assert.equal(refreshCalls, 2, 'Unrelated file changes must not trigger _refresh()');

monitorShouldThrow = true;
widget._startStateMonitor();
assert.equal(widget._stateMonitor, null, 'Monitor failure resets _stateMonitor');
assert.equal(widget._stateMonitorChangedId, null, 'Monitor failure resets _stateMonitorChangedId');

assert.ok(source.includes('this._startStateMonitor();'), 'Indicator initializes runtime monitor');
assert.ok(source.includes('this._stateMonitor.cancel();'), 'Indicator cancels runtime monitor on destroy');

// Usage-today label changes must trigger a re-render even when state is unchanged
widget._renderState = () => { renders++; isSpinning = Boolean(widget._refreshPending); };
widget._refresh();
let usageToday = null;
widget._readUsageToday = () => usageToday;
const rendersBeforeUsageToday = renders;
widget._refresh();
assert.equal(renders, rendersBeforeUsageToday, 'Null usage-today matches initial null key');
assert.equal(widget._usageTodayLabel, null);
usageToday = 'Today: 12.4k tok';
widget._refresh();
assert.equal(renders, rendersBeforeUsageToday + 1, 'Changing usage-today from null to label triggers re-render');
assert.equal(widget._usageTodayLabel, 'Today: 12.4k tok');
widget._refresh();
assert.equal(renders, rendersBeforeUsageToday + 1, 'Unchanged usage-today label must not re-render');
usageToday = 'Today: 15.0k tok';
widget._refresh();
assert.equal(renders, rendersBeforeUsageToday + 2, 'Updated usage-today label triggers re-render');
assert.equal(widget._usageTodayLabel, 'Today: 15.0k tok');
usageToday = null;
widget._refresh();
assert.equal(renders, rendersBeforeUsageToday + 3, 'Clearing usage-today label triggers re-render');
assert.equal(widget._usageTodayLabel, null);

// Single-provider refresh uses agent-meterd --once --provider <id> when available
widget._renderState = () => { renders++; isSpinning = Boolean(widget._refreshPending); };
spawnedArgvs = [];
timeoutCallbacks = [];
fileTestImpl = path => path === '/home/test/.local/bin/agent-meterd';
widget._startControl('refresh', 'muse');
assert.deepEqual(
    spawnedArgvs[0],
    ['/home/test/.local/bin/agent-meterd', '--once', '--provider', 'muse'],
    'Per-provider refresh spawns local agent-meterd with --once --provider'
);

spawnedArgvs = [];
timeoutCallbacks = [];
fileTestImpl = () => false;
findProgramImpl = name => (name === 'agent-meterd' ? '/usr/local/bin/agent-meterd' : null);
widget._startControl('refresh', 'muse');
assert.deepEqual(
    spawnedArgvs[0],
    ['/usr/local/bin/agent-meterd', '--once', '--provider', 'muse'],
    'Per-provider refresh falls back to PATH agent-meterd when local binary is absent'
);

spawnedArgvs = [];
timeoutCallbacks = [];
findProgramImpl = () => null;
widget._startControl('refresh', 'muse');
assert.deepEqual(
    spawnedArgvs[0],
    ['systemctl', '--user', 'restart', 'agent-meter.service'],
    'Per-provider refresh falls back to systemctl when agent-meterd is unavailable'
);

console.log('Change-driven rendering checks passed');
