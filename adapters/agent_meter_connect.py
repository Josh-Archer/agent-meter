"""Launch a provider-owned sign-in flow without handling credentials ourselves."""

from __future__ import annotations

import argparse
import os
import shlex
import shutil
import subprocess
import sys


COMMANDS = {
    "codex": ["codex", "login"],
    # agy has no separate login subcommand; its first interactive launch owns
    # the browser sign-in flow when the current session is unauthenticated.
    "agy": ["agy"],
    "antigravity": ["agy"],
    "grok": ["grok", "login"],
    # The SDK-bundled Copilot CLI owns its GitHub OAuth/keychain state.
    "copilot": [os.path.expanduser("~/.local/share/agent-meter/copilot-sdk/node_modules/.bin/copilot")],
    # Claude Code's /login is an interactive slash command, so start its TUI.
    "claude": ["claude"],
}


def terminal_command(command: list[str]) -> list[str] | None:
    preferred = os.environ.get("TERMINAL")
    if preferred and shutil.which(preferred):
        return [preferred, "-e", *command]
    if shutil.which("ghostty"):
        return ["ghostty", "-e", *command]
    if shutil.which("kgx"):
        return ["kgx", "--", *command]
    if shutil.which("gnome-terminal"):
        return ["gnome-terminal", "--", *command]
    if shutil.which("x-terminal-emulator"):
        return ["x-terminal-emulator", "-e", *command]
    return None


def main() -> None:
    parser = argparse.ArgumentParser(description="Launch a provider's own sign-in flow")
    parser.add_argument("provider", choices=COMMANDS)
    provider = parser.parse_args().provider
    command = COMMANDS[provider]
    if not shutil.which(command[0]):
        print(f"{command[0]} is not installed; Agent Meter cannot start its sign-in flow.", file=sys.stderr)
        raise SystemExit(1)

    # Wrap the command so that when sign-in finishes, agent-meter daemon immediately refreshes
    shell_cmd = shlex.join(command) + "; systemctl --user restart agent-meter.service"
    wrapped = ["bash", "-c", shell_cmd]
    terminal = terminal_command(wrapped)
    if terminal is None:
        terminal = terminal_command(command)
    if terminal is None:
        print("No supported terminal emulator was found.", file=sys.stderr)
        raise SystemExit(1)
    # The provider command owns all credential prompts and browser redirects.
    # Agent Meter receives neither typed values nor an OAuth token.
    subprocess.Popen(terminal, start_new_session=True)


if __name__ == "__main__":
    main()
