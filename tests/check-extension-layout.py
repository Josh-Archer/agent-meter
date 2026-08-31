#!/usr/bin/env python3
"""Small structural validation that does not need an active GNOME session."""
import json
from pathlib import Path

root = Path(__file__).resolve().parents[1]
extension = root / "gnome-extension" / "agent-meter@local"
metadata = json.loads((extension / "metadata.json").read_text())
assert metadata["uuid"] == "agent-meter@local"
source = (extension / "extension.js").read_text()
for name in ("codex", "antigravity", "grok", "copilot", "claude", "generic"):
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
assert "if (hasNumericQuota)" in source
assert source.count("provider.status === 'unavailable'") == 2

# Validate progressive quotaColor function in JS
import subprocess
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
subprocess.run(["node", "-e", js_test], check=True, cwd=root)
print("GNOME extension layout checks passed")
