// Exercise the extension's actual drag methods without a GNOME session.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const source = fs.readFileSync('gnome-extension/agent-meter@local/extension.js', 'utf8');
const Clutter = {EVENT_STOP: true, EVENT_PROPAGATE: false, BUTTON_PRIMARY: 1,
    EventType: {MOTION: 1, BUTTON_RELEASE: 2}};
let captured, disconnected, dismissed = false, saved;
const global = {display: {focus_window: {}}, stage: {
    grab: () => ({dismiss() { dismissed = true; }}),
}};
const savePosition = (x, y) => { saved = [x, y]; };
const start = source.indexOf('    _finishDrag(');
const end = source.indexOf('    _readState()', start);
const Drag = eval(`(class {${source.slice(start, end)}})`);
const drag = new Drag();
let position = [100, 200];
drag._desktop = {get_position: () => position, set_position: (x, y) => { position = [x, y]; }};
drag._clampedPosition = (x, y) => [Math.max(0, x), Math.max(0, y)];
const handlers = {};
const handle = {connect: (name, callback) => {
    handlers[name] = callback;
    if (name === 'event') captured = callback;
    return 42;
}, disconnect: id => { disconnected = id; },
    add_style_pseudo_class() {}, remove_style_pseudo_class() {}};
drag._attachDragHandle(handle);
const event = (type, x, y, button = 1) => ({type: () => type, get_coords: () => [x, y], get_button: () => button});
assert.equal(handlers['button-press-event'](handle, event(0, 110, 210, 3)), false);
handlers['button-press-event'](handle, event(0, 110, 210));
assert.equal(captured(null, event(1, 310, 410)), true);
assert.deepEqual(position, [300, 400]);
// Releasing outside the header still saves the position and releases capture.
captured(null, event(2, 310, 410));
assert.deepEqual(saved, [300, 400]);
assert.equal(disconnected, 42);
assert.equal(dismissed, true);
assert.equal(drag._dragState, null);
assert.equal(drag._dragEventId, null);
assert.ok(!source.includes("connect('captured-event'"), 'Grabbed pointer events must not use capture');
drag._queueClamp = () => {};
drag._desktopVisible = true;
drag._syncDesktopVisibility();
assert.equal(drag._desktop.visible, false);
drag.toggleFromKeyboard();
assert.equal(drag._desktop.visible, true, 'Shortcut opens over an active app');
drag.toggleFromKeyboard();
assert.equal(drag._desktop.visible, false, 'Shortcut closes the widget');
global.display.focus_window = null;
drag.toggleFromKeyboard();
assert.equal(drag._desktop.visible, true, 'Shortcut also opens on desktop');
assert.ok(source.includes("Main.wm.removeKeybinding('toggle-widget')"));
const refreshBody = source.slice(source.indexOf('    _refresh() {'), source.indexOf('        const state = this._readState();'));
assert.match(refreshBody, /if \(this\._dragState\)\s+return;/);
console.log('Drag movement, outside release, cleanup, and refresh guard passed');
