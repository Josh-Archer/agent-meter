// Open each provider usage_url from the panel menu in the default browser.
const assert = require('node:assert/strict');
const fs = require('node:fs');

const source = fs.readFileSync('gnome-extension/agent-meter@local/extension.js', 'utf8');
const usageStart = source.indexOf('function usagePageUrl(');
const openStart = source.indexOf('function openUsagePage(', usageStart);
const openEnd = source.indexOf('\nconst AgentMeterIndicator', openStart);
assert.ok(usageStart >= 0 && openStart > usageStart && openEnd > openStart, 'usage helpers must stay extractable');

const usagePageUrl = eval(`(${source.slice(usageStart, openStart)})`);
const openUsagePage = eval(`(${source.slice(openStart, openEnd)})`);

const accepted = [
    'https://chatgpt.com/#settings/usage',
    'http://github.com/settings/copilot/features',
    'HTTPS://claude.ai/settings/usage',
    'https://localhost:8443/usage?view=week',
    'https://[::1]:8080',
    'https://chatgpt.com/@user',
    'https://example.com:1/',
    'https://example.com:65535/',
];
for (const url of accepted)
    assert.equal(usagePageUrl(`  ${url}  `), url);

const rejected = [
    null,
    undefined,
    12,
    '',
    '   ',
    'javascript:alert(1)',
    'JAVASCRIPT:alert(1)',
    'file:///etc/passwd',
    'data:text/html,hi',
    'vscode://x',
    'ftp://example.com/usage',
    'https:///etc/passwd',
    'https://',
    'https:///',
    'https://host with space/usage',
    'https://host\u2028name/usage',
    'not a url',
    '/relative',
    '//host/x',
    'https://user:secret@chatgpt.com/usage',
    'https://user@host',
    'https://a.com\\@b.com',
    // Blocker: control characters [\x00-\x1f\x7f] anywhere in the URL (host, path, query, fragment)
    'https://a\x01b.com/',
    'https://a.com/\x00x',
    'https://a.com/\x7f',
    'https://a.com/\x1f',
    'https://a.com/?q=\x05',
    'https://a.com/#\x02',
    // Nits: empty host
    'https://:80/',
    'https://:80',
    'https://:',
    'https://?query',
    'https://#fragment',
    // Nits: ports outside 1-65535
    'https://example.com:99999/',
    'https://example.com:99999',
    'https://example.com:0/',
    'https://example.com:0',
    'https://example.com:65536/',
    'https://[::1]:99999',
    'https://[::1]:0',
    // Nits: backslashes in host
    'https://good.com\\evil.com',
    'https://good.com\\evil.com/',
    'https://a\\b.com/',
    'https://\\evil.com',
];
for (const value of rejected)
    assert.equal(usagePageUrl(value), null, `rejected ${JSON.stringify(value)}`);

const launched = [];
let launchError = null;
global.create_app_launch_context = (timestamp, workspace) => ({timestamp, workspace});
const Gio = {
    AppInfo: {
        launch_default_for_uri(url, context) {
            if (launchError)
                throw launchError;
            launched.push({url, context});
            return true;
        },
    },
};

assert.equal(openUsagePage('javascript:alert(1)'), false);
assert.equal(launched.length, 0, 'Rejected schemes never reach the browser');
assert.equal(openUsagePage('  https://chatgpt.com/#settings/usage  '), true);
assert.deepEqual(launched, [{
    url: 'https://chatgpt.com/#settings/usage',
    context: {timestamp: 0, workspace: -1},
}]);

launched.length = 0;
Gio.AppInfo.launch_default_for_uri = () => false;
const warnings = [];
const originalWarn = console.warn;
console.warn = message => warnings.push(message);
try {
    assert.equal(openUsagePage('https://claude.ai/settings/usage'), false);
    launchError = new Error('no handler');
    Gio.AppInfo.launch_default_for_uri = () => { throw launchError; };
    assert.equal(openUsagePage('https://grok.com/#settings/usage'), false);
} finally {
    console.warn = originalWarn;
}
assert.ok(warnings.some(message => message.includes('no default application')));
assert.ok(warnings.some(message => message.includes('no handler')));

const openSource = source.slice(openStart, openEnd);
assert.equal(openSource.includes('xdg-open'), false);
assert.equal(openSource.includes('Subprocess'), false);
assert.ok(openSource.includes('launch_default_for_uri'));

function Actor(props) {
    Object.assign(this, props);
}
Actor.prototype.add_child = function() {};
const St = {BoxLayout: Actor, Icon: Actor, Label: Actor};
const Clutter = {ActorAlign: {CENTER: 1}};
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
assert.ok(renderStart >= 0 && renderEnd > renderStart);
const Indicator = eval(`(class {${source.slice(renderStart, renderEnd)}})`);
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
widget.menu = menu;
widget._icons = {destroy_all_children() {}, add_child() {}};
widget._stopSpinner = () => {};
widget._addSpinner = () => {};
widget._rebuildDesktop = () => {};
widget._startControl = () => {};
widget._syncDesktopVisibility = () => {};
widget._queueClamp = () => {};
widget._desktop = {set_position() {}};
widget._desktopVisible = true;
widget._extension = {};
widget._refreshPending = false;

const state = {
    generated_at: '2026-09-16T20:45:00Z',
    providers: [
        {id: 'codex', label: 'Codex', status: 'fresh', windows: [{remaining_percent: 72}], usage_url: '  https://chatgpt.com/#settings/usage  '},
        {id: 'muse', label: 'Muse', status: 'fresh', windows: [{remaining_percent: 80}]},
        {id: 'bad', label: 'Bad', status: 'fresh', windows: [{remaining_percent: 10}], usage_url: 'javascript:alert(1)'},
        {id: 'claude', label: 'Claude Code', status: 'unavailable', windows: [{id: 'availability', remaining_percent: 0}], usage_url: 'https://claude.ai/settings/usage'},
        {id: 'grok', label: 'Grok Build', status: 'stale', windows: [{remaining_percent: 40}], usage_url: 'https://grok.com/#settings/usage'},
        {id: 'local', label: 'Local', status: 'fresh', windows: [{remaining_percent: 5}], usage_url: 'file:///tmp/usage.html'},
        {id: 'copilot', label: 'GitHub Copilot', status: 'error', windows: [{remaining_percent: 1}], usage_url: 'http://github.com/settings/copilot/features'},
    ],
};

launched.length = 0;
launchError = null;
Gio.AppInfo.launch_default_for_uri = (url, context) => {
    launched.push({url, context});
    return true;
};
widget._renderState(state);

const texts = menu.items.map(item => item.text).filter(Boolean);
const usageItems = () => menu.items.filter(item => item.text?.includes('→ Open') && item.text.includes('usage…'));
assert.deepEqual(texts.filter(text => text.includes('→ Open') && text.includes('usage…')), [
    '  → Open Codex usage…',
    '  → Open Claude Code usage…',
    '  → Open Grok Build usage…',
    '  → Open GitHub Copilot usage…',
]);
assert.ok(texts.includes('  → Connect Claude Code…'));
assert.ok(texts.includes('  → Open Grok Build to refresh…'));
assert.ok(texts.includes('Desktop Widget'));
assert.ok(texts.includes('Refresh usage now'));

for (const item of usageItems())
    item.on_activate();
assert.deepEqual(launched.map(entry => entry.url), [
    'https://chatgpt.com/#settings/usage',
    'https://claude.ai/settings/usage',
    'https://grok.com/#settings/usage',
    'http://github.com/settings/copilot/features',
]);
assert.ok(launched.every(entry => entry.context.timestamp === 0 && entry.context.workspace === -1));

widget._refreshPending = true;
widget._renderState(state);
assert.equal(usageItems().length, 0, 'Refresh placeholder replaces usage items');
widget._refreshPending = false;
widget._renderState({providers: []});
assert.equal(usageItems().length, 0);

console.log('Provider usage page menu checks passed');
