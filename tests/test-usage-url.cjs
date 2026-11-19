// Panel menu per-provider connect/refresh action buttons and hover hints.
const assert = require('node:assert/strict');
const fs = require('node:fs');

const source = fs.readFileSync('gnome-extension/agent-meter@local/extension.js', 'utf8');
const actionStart = source.indexOf('function providerMenuAction(');
const actionEnd = source.indexOf('\nconst AgentMeterIndicator', actionStart);
assert.ok(actionStart >= 0 && actionEnd > actionStart, 'providerMenuAction helper must stay extractable');

const providerMenuAction = eval(`(${source.slice(actionStart, actionEnd)})`);

assert.deepEqual(providerMenuAction({id: 'muse', label: 'Muse', status: 'unavailable'}), {
    action: 'connect',
    iconName: 'insert-link-symbolic',
    hint: 'Connect Muse',
});
assert.deepEqual(providerMenuAction({id: 'muse', label: 'Muse', status: 'fresh'}), {
    action: 'refresh',
    iconName: 'view-refresh-symbolic',
    hint: 'Refresh Muse limits',
});
assert.deepEqual(providerMenuAction({id: 'grok', label: 'Grok Build', status: 'stale'}), {
    action: 'open',
    iconName: 'view-refresh-symbolic',
    hint: 'Open Grok Build to refresh',
});
assert.deepEqual(providerMenuAction({id: 'copilot', label: 'GitHub Copilot', status: 'error'}), {
    action: 'refresh',
    iconName: 'view-refresh-symbolic',
    hint: 'Refresh GitHub Copilot limits',
});

function Actor(props) {
    Object.assign(this, props);
    this._signals = {};
    this.children = [];
}
Actor.prototype.add_child = function(child) {
    this.children.push(child);
};
Actor.prototype.remove_child = function(child) {
    this.children = this.children.filter(c => c !== child);
};
Actor.prototype.connect = function(signal, callback) {
    this._signals[signal] = callback;
    return 1;
};
Actor.prototype.emit = function(signal, ...args) {
    return this._signals[signal]?.(this, ...args);
};
Actor.prototype.show = function() {
    this.visible = true;
};
Actor.prototype.hide = function() {
    this.visible = false;
};
Actor.prototype.destroy = function() {
    this.destroyed = true;
};
Actor.prototype.get_transformed_position = function() {
    return [200, 120];
};
Actor.prototype.get_transformed_size = function() {
    return [28, 28];
};
Actor.prototype.get_preferred_width = function() {
    return [0, 120];
};
Actor.prototype.get_preferred_height = function() {
    return [0, 24];
};
Actor.prototype.set_position = function(x, y) {
    this.x = x;
    this.y = y;
};

const St = {BoxLayout: Actor, Button: Actor, Icon: Actor, Label: Actor};
const Clutter = {ActorAlign: {CENTER: 1}};
const Main = {
    uiGroup: new Actor({}),
    layoutManager: {
        primaryMonitor: {x: 0, y: 0, width: 1920, height: 1080},
    },
};
const PopupMenu = {
    PopupMenuItem: function(text, params) {
        this.text = text;
        this.params = params;
    },
    PopupBaseMenuItem: function(params) {
        this.params = params;
        this.children = [];
    },
    PopupSeparatorMenuItem: function() {
        this.separator = true;
    },
    PopupSwitchMenuItem: function(text, active) {
        this.text = text;
        this.active = active;
    },
};
for (const name of Object.keys(PopupMenu)) {
    PopupMenu[name].prototype.connect = function(signal, callback) {
        this[`on_${signal}`] = callback;
        return 1;
    };
    PopupMenu[name].prototype.add_child = function(child) {
        this.children = this.children ?? [];
        this.children.push(child);
    };
}

function quotaWindows(provider) {
    return (provider.windows ?? []).filter(window => typeof window.remaining_percent === 'number');
}
function providerRemainingPercent(provider) {
    const windows = quotaWindows(provider);
    return windows.length > 0 ? Math.min(...windows.map(w => w.remaining_percent)) : 100;
}
function iconFor() {
    return {};
}
function limitText(provider) {
    return provider.status ?? '';
}
function quotaColor() {
    return '#38d472';
}
function savePosition() {}

const renderStart = source.indexOf('    _renderState(state) {');
const renderEnd = source.indexOf('    _startControl(action, provider = null) {', renderStart);
const hintStart = source.indexOf('    _showActionHint(actor, text) {');
const hintEnd = source.indexOf('    destroy() {', hintStart);
assert.ok(renderStart >= 0 && renderEnd > renderStart);
assert.ok(hintStart >= 0 && hintEnd > hintStart);
const Indicator = eval(`(class {
${source.slice(renderStart, renderEnd)}
${source.slice(hintStart, hintEnd)}
})`);
const widget = new Indicator();
const menu = {
    items: [],
    removeAll() {
        this.items = [];
    },
    addMenuItem(item) {
        this.items.push(item);
    },
};
const controls = [];
widget.menu = menu;
widget._icons = {destroy_all_children() {}, add_child() {}};
widget._stopSpinner = () => {};
widget._addSpinner = () => {};
widget._rebuildDesktop = () => {};
widget._startControl = (action, provider) => controls.push({action, provider});
widget._syncDesktopVisibility = () => {};
widget._queueClamp = () => {};
widget._desktop = {set_position() {}};
widget._desktopVisible = true;
widget._extension = {};
widget._refreshPending = false;
widget._actionHint = null;
widget._actionHintActor = null;

const state = {
    generated_at: '2026-09-16T20:45:00Z',
    providers: [
        {id: 'codex', label: 'Codex', status: 'fresh', windows: [{remaining_percent: 72}], usage_url: 'https://chatgpt.com/#settings/usage'},
        {id: 'muse', label: 'Muse', status: 'fresh', windows: [{remaining_percent: 80}]},
        {id: 'claude', label: 'Claude Code', status: 'unavailable', windows: [{id: 'availability', remaining_percent: 0}], usage_url: 'https://claude.ai/settings/usage'},
        {id: 'grok', label: 'Grok Build', status: 'stale', windows: [{remaining_percent: 40}], usage_url: 'https://grok.com/#settings/usage'},
        {id: 'copilot', label: 'GitHub Copilot', status: 'error', windows: [{remaining_percent: 1}], usage_url: 'http://github.com/settings/copilot/features'},
    ],
};

widget._renderState(state);

const texts = menu.items.map(item => item.text).filter(Boolean);
assert.equal(texts.some(text => text.includes('→ Open') && text.includes('usage…')), false, 'No usage link sub-rows');
assert.equal(texts.some(text => text.includes('→ Connect')), false, 'No separate Connect sub-rows');
assert.equal(texts.some(text => text.includes('Open Grok Build to refresh')), false, 'No separate Grok sub-row');
assert.equal(texts.includes('Refresh usage now'), false, 'No Refresh usage now button at bottom');
assert.ok(texts.includes('Desktop Widget'));
assert.ok(texts.includes('Reset Widget Position'));

const providerRows = menu.items.filter(item => item.children && item.children.length === 3);
assert.equal(providerRows.length, state.providers.length, 'Each provider renders a single row with icon, labels, and action button');

const buttons = providerRows.map(row => row.children[2]);
assert.deepEqual(buttons.map(btn => btn.child.icon_name), [
    'view-refresh-symbolic',
    'view-refresh-symbolic',
    'insert-link-symbolic',
    'view-refresh-symbolic',
    'view-refresh-symbolic',
]);
assert.deepEqual(buttons.map(btn => btn._hintText), [
    'Refresh Codex limits',
    'Refresh Muse limits',
    'Connect Claude Code',
    'Open Grok Build to refresh',
    'Refresh GitHub Copilot limits',
]);

// Hover shows hint tooltip and unhover hides it
buttons[2].hover = true;
buttons[2].emit('notify::hover');
assert.ok(widget._actionHint, 'Tooltip label created on hover');
assert.equal(widget._actionHint.visible, true);
assert.equal(widget._actionHint.text, 'Connect Claude Code');

buttons[2].hover = false;
buttons[2].emit('notify::hover');
assert.equal(widget._actionHint.visible, false);

// Clicking buttons triggers per-provider action
for (const btn of buttons)
    btn.emit('clicked');
assert.deepEqual(controls, [
    {action: 'refresh', provider: 'codex'},
    {action: 'refresh', provider: 'muse'},
    {action: 'connect', provider: 'claude'},
    {action: 'open', provider: 'grok'},
    {action: 'refresh', provider: 'copilot'},
]);

widget._refreshPending = true;
widget._renderState(state);
assert.equal(menu.items.filter(item => item.children?.length === 3).length, 0, 'Refresh placeholder replaces provider rows');
widget._refreshPending = false;
widget._renderState({providers: []});
assert.equal(menu.items.filter(item => item.children?.length === 3).length, 0);

console.log('Panel menu action button and hover hint checks passed');
