// Persist the Desktop Widget switch in position.json without dropping coordinates.
const assert = require('node:assert/strict');
const fs = require('node:fs');

const source = fs.readFileSync('gnome-extension/agent-meter@local/extension.js', 'utf8');
const start = source.indexOf('function widgetConfigPath()');
const end = source.indexOf('function quotaWindows(', start);
assert.ok(start >= 0 && end > start, 'widget config helpers must stay extractable');

const files = new Map();
const mkdirModes = [];
const chmodModes = [];
let replaceError = null;
let readError = null;

const GLib = {
    get_user_config_dir: () => '/cfg',
    build_filenamev: parts => parts.join('/'),
    mkdir_with_parents: (_path, mode) => { mkdirModes.push(mode); },
    chmod: (_path, mode) => { chmodModes.push(mode); },
};
const Gio = {
    FileCreateFlags: {REPLACE_DESTINATION: 1},
    File: {
        new_for_path: path => ({
            load_contents() {
                if (readError)
                    throw readError;
                if (!files.has(path))
                    throw new Error('not found');
                const [ok, bytes] = files.get(path);
                return [ok, bytes];
            },
            replace_contents(contents) {
                if (replaceError)
                    throw replaceError;
                const text = typeof contents === 'string' ? contents : new TextDecoder().decode(contents);
                files.set(path, [true, new TextEncoder().encode(text)]);
                return [true, null];
            },
        }),
    },
};

const api = eval(`${source.slice(start, end)}
({loadSavedPosition, loadDesktopVisible, savePosition, saveDesktopVisible, readWidgetConfig})`);

const configPath = '/cfg/agent-meter/position.json';
const stored = () => JSON.parse(new TextDecoder().decode(files.get(configPath)[1]));

assert.deepEqual(api.loadSavedPosition(), [36, 60], 'Missing file keeps the default position');
assert.equal(api.loadDesktopVisible(), true, 'Missing file leaves the desktop widget on');

files.set(configPath, [true, new TextEncoder().encode(JSON.stringify({x: 12, y: 80}))]);
assert.deepEqual(api.loadSavedPosition(), [12, 80]);
assert.equal(api.loadDesktopVisible(), true, 'Older position files stay visible');

api.saveDesktopVisible(false);
assert.equal(api.loadDesktopVisible(), false);
assert.deepEqual(stored(), {x: 12, y: 80, desktopVisible: false});
assert.deepEqual(mkdirModes, [0o700]);
assert.deepEqual(chmodModes, [0o700]);

api.savePosition(10.6, 40.2);
assert.deepEqual(stored(), {x: 11, y: 40, desktopVisible: false}, 'Moving the card keeps the switch');
assert.equal(api.loadDesktopVisible(), false);
assert.deepEqual(api.loadSavedPosition(), [11, 40]);

api.saveDesktopVisible(true);
assert.deepEqual(stored(), {x: 11, y: 40, desktopVisible: true}, 'The switch keeps the saved position');
assert.equal(api.loadDesktopVisible(), true);

files.set(configPath, [true, new TextEncoder().encode(JSON.stringify({
    x: -4, y: 10, desktopVisible: false, extra: 'keep',
}))]);
assert.deepEqual(api.loadSavedPosition(), [0, 28], 'Saved coordinates stay clamped');
assert.equal(api.loadDesktopVisible(), false);
api.savePosition(3, 90);
assert.deepEqual(stored(), {x: 3, y: 90, desktopVisible: false, extra: 'keep'});

files.clear();
api.saveDesktopVisible(false);
assert.deepEqual(stored(), {desktopVisible: false});
assert.deepEqual(api.loadSavedPosition(), [36, 60], 'Switch-only file still uses the default position');
assert.equal(api.loadDesktopVisible(), false);

for (const body of ['[]', 'null', '"false"', 'nope', JSON.stringify({desktopVisible: 'false'})]) {
    files.set(configPath, [true, new TextEncoder().encode(body)]);
    assert.equal(api.loadDesktopVisible(), true, `Unusable value defaults on: ${body}`);
    assert.deepEqual(api.loadSavedPosition(), [36, 60], `Unusable position defaults: ${body}`);
}
files.set(configPath, [false, new Uint8Array()]);
assert.equal(api.loadDesktopVisible(), true, 'Unreadable file defaults the switch on');

const debug = [];
const originalDebug = console.debug;
console.debug = message => debug.push(message);
try {
    replaceError = new Error('disk full');
    api.saveDesktopVisible(false);
    api.savePosition(1, 40);
    readError = new Error('unreadable');
    assert.deepEqual(api.readWidgetConfig(), {});
    assert.equal(api.loadDesktopVisible(), true);
} finally {
    console.debug = originalDebug;
    replaceError = null;
    readError = null;
}
assert.deepEqual(debug, [
    'Could not save desktop widget switch: disk full',
    'Could not save widget position: disk full',
]);

assert.match(source, /this\._desktopVisible = loadDesktopVisible\(\)/);
assert.match(source, /PopupSwitchMenuItem\('Desktop Widget', this\._desktopVisible\)/);
assert.match(source, /_setDesktopVisible\(active\)/);
assert.match(source, /saveDesktopVisible\(this\._desktopVisible\)/);
assert.equal(source.includes('this._desktopVisible = true'), false, 'The switch must not always start on');

console.log('Desktop widget switch persistence checks passed');
