const assert = require('node:assert/strict');
const fs = require('node:fs');
const source = fs.readFileSync('gnome-extension/agent-meter@local/extension.js', 'utf8');
const start = source.indexOf('    _refresh() {');
const end = source.indexOf('    _renderState(state) {', start);
const controlStart = source.indexOf('    _startControl(action, provider = null) {');
const controlEnd = source.indexOf('    _rebuildDesktop(state) {', controlStart);
let timeoutCallbacks = [];
const GLib = {
    get_home_dir: () => '/home/test',
    build_filenamev: parts => parts.join('/'),
    file_test: () => false,
    find_program_in_path: () => null,
    FileTest: {IS_EXECUTABLE: 1},
    PRIORITY_DEFAULT: 0,
    SOURCE_REMOVE: false,
    timeout_add: (priority, interval, fn) => {
        timeoutCallbacks.push(fn);
        return timeoutCallbacks.length;
    },
};
let subprocessShouldThrow = true;
const Gio = {
    SubprocessFlags: {NONE: 0},
    Subprocess: {
        new: () => {
            if (subprocessShouldThrow)
                throw new Error('spawn failed');
            return {};
        },
    },
};
const Refresh = eval(`(class {
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

console.log('Change-driven rendering checks passed');
