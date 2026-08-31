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
assert "_backgroundGroup" in source or "Main.layoutManager.addChrome" in source
assert "this._icons.add_child" in source
assert "_attachDragHandle" in source
assert "global.stage.grab(handle)" in source
assert "notify::focus-window" in source
assert "Clutter.DragAction" not in source
assert "width: ${percent}%" not in source
print("GNOME extension layout checks passed")
