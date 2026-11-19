#!/usr/bin/env python3
"""Small structural validation that does not need an active GNOME session."""
import json
from pathlib import Path

root = Path(__file__).resolve().parents[1]
extension = root / "gnome-extension" / "agent-meter@local"
metadata = json.loads((extension / "metadata.json").read_text())
assert metadata["uuid"] == "agent-meter@local"
source = (extension / "extension.js").read_text()
for name in ("codex", "antigravity", "grok", "copilot", "claude", "muse", "generic"):
    assert (extension / "icons" / f"{name}.svg").is_file(), name
    assert (extension / "icons" / f"{name}-symbolic.svg").is_file(), name
    assert f"'{name}'" in source, name
for symbolic_icon in (extension / "icons").glob("*-symbolic.svg"):
    assert "#f7f9fc" in symbolic_icon.read_text(), symbolic_icon.name
assert "_backgroundGroup" in source or "Main.layoutManager.addChrome" in source
assert "this._icons.add_child" in source
assert "_attachDragHandle" in source
assert "global.stage.grab(handle)" in source
assert "notify::focus-window" in source
assert "Clutter.DragAction" not in source
assert "width: ${percent}%" not in source
assert "agent-meter-progress-stale" in source
assert "if (shouldShowQuota)" in source
assert "process-working-symbolic" in source
assert "rotation_angle_z" in source
assert "agent-meter-pill-failed" in source
assert "let pctText = '—'" in source
assert "const shouldShowQuota = isFresh && hasNumericQuota;" in source
assert "Usage unavailable; cached data expired." in source
assert "Open Grok Build to refresh" in source
assert "['connect', 'open'].includes(action)" in source
assert "function providerMenuAction(" in source
assert "Refresh usage now" not in source
assert "Open ${provider.label} usage" not in source

# Validate progressive quotaColor function in JS
import subprocess
subprocess.run(["node", "tests/test-usage-url.cjs"], check=True, cwd=root)
js_test = """
const source = require('fs').readFileSync('gnome-extension/agent-meter@local/extension.js', 'utf8');
// Extract COLOR_STOPS and quotaColor function
const fnMatch = source.match(/const COLOR_STOPS = [\\s\\S]*?function quotaColor\\([\\s\\S]*?\\n\\}/);
if (!fnMatch) throw new Error('quotaColor function not found in extension.js');
eval(fnMatch[0]);

const asserts = [
  [quotaColor(100, true), '#38d472'],
  [quotaColor(41, true), '#38d472'],
  [quotaColor(40, true), '#38d472'],
  [quotaColor(30, true), '#f6c445'],
  [quotaColor(20, true), '#ff8833'],
  [quotaColor(10, true), '#ff6644'],
  [quotaColor(0, true), '#ff3b56'],
  [quotaColor(50, false), '#8b949e'],
];

for (const [actual, expected] of asserts) {
  if (actual !== expected) {
    throw new Error(`quotaColor mismatch: expected ${expected}, got ${actual}`);
  }
}
console.log('quotaColor progressive scale validated successfully across all stops');
"""
icon_cache_test = r"""
const assert = require('node:assert/strict');
const source = require('fs').readFileSync('gnome-extension/agent-meter@local/extension.js', 'utf8');
const iconStart = source.indexOf('function iconFor(');
const iconEnd = source.indexOf('const COLOR_STOPS =', iconStart);
assert.ok(iconStart >= 0 && iconEnd > iconStart, 'iconFor must be extractable');

let queryExistsCalls = 0;
let fileIconCalls = 0;
const Gio = {
  FileIcon: class FileIcon {
    constructor({file}) {
      fileIconCalls++;
      this.file = file;
    }
  },
};

const iconFor = eval(`(() => {
${source.slice(iconStart, iconEnd)}
return iconFor;
})()`);

function makeExt(symbolicExists = true) {
  return {
    dir: {
      get_child(dirName) {
        assert.equal(dirName, 'icons');
        return {
          get_child(fileName) {
            return {
              fileName,
              query_exists() {
                queryExistsCalls++;
                return symbolicExists;
              },
            };
          },
        };
      },
    },
  };
}

const ext = makeExt(true);
const codex1 = iconFor(ext, 'codex');
const codex2 = iconFor(ext, 'codex');
assert.strictEqual(codex1, codex2, 'Repeated codex calls must return identical cached FileIcon');
assert.equal(queryExistsCalls, 1, 'query_exists called once for codex');
assert.equal(fileIconCalls, 1, 'Gio.FileIcon constructed once for codex');
assert.equal(codex1.file.fileName, 'codex-symbolic.svg');

const agy1 = iconFor(ext, 'agy');
const agy2 = iconFor(ext, 'agy');
const antigravity = iconFor(ext, 'antigravity');
assert.strictEqual(agy1, agy2, 'Repeated agy calls must return identical cached FileIcon');
assert.strictEqual(agy1, antigravity, 'agy and antigravity share normalized cache entry');
assert.equal(queryExistsCalls, 2, 'query_exists called once per normalized symbol');
assert.equal(fileIconCalls, 2, 'Gio.FileIcon constructed once per normalized symbol');
assert.equal(agy1.file.fileName, 'antigravity-symbolic.svg');

const extFallback = makeExt(false);
const fallback1 = iconFor(extFallback, 'codex');
const fallback2 = iconFor(extFallback, 'codex');
assert.strictEqual(fallback1, fallback2, 'Fallback non-symbolic icon must also be cached');
assert.notStrictEqual(fallback1, codex1, 'Cache must be scoped per extension instance');
assert.equal(fallback1.file.fileName, 'codex.svg');
assert.equal(queryExistsCalls, 3);
assert.equal(fileIconCalls, 3);

const unknown1 = iconFor(ext, 'unknown-provider');
const unknown2 = iconFor(ext, 'generic');
assert.strictEqual(unknown1, unknown2, 'Unknown symbols normalize to generic cache entry');
assert.equal(unknown1.file.fileName, 'generic-symbolic.svg');
assert.equal(queryExistsCalls, 4);
assert.equal(fileIconCalls, 4);

console.log('iconFor caching validated successfully');
"""
subprocess.run(["node", "tests/test-quota-card-detail.cjs"], check=True, cwd=root)
subprocess.run(["node", "-e", js_test], check=True, cwd=root)
subprocess.run(["node", "-e", icon_cache_test], check=True, cwd=root)
subprocess.run(["node", "tests/test-drag.cjs"], check=True, cwd=root)
subprocess.run(["node", "tests/test-refresh.cjs"], check=True, cwd=root)
subprocess.run(["node", "tests/test-desktop-visible.cjs"], check=True, cwd=root)
print("GNOME extension layout checks passed")
