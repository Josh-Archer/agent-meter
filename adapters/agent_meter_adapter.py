"""Credential-safe local adapters for Agent Meter.

The programs emit one normalized ProviderState JSON document to stdout. They
never write an upstream response to disk and deliberately omit tokens, account
IDs, project IDs, and reset-credit IDs.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import subprocess
import sys
import tempfile
from typing import Any


def unavailable(identifier: str, label: str, icon: str, detail: str) -> dict[str, Any]:
    return {
        "id": identifier,
        "label": label,
        "icon": icon,
        "windows": [
            {
                "id": "availability",
                "label": "Availability",
                "remaining_percent": 0.0,
                "reset_label": "unavailable",
            }
        ],
        "status": "unavailable",
        "detail": detail,
    }


def reset_label(timestamp: int | float | str | None) -> tuple[str | None, str | None]:
    if timestamp is None:
        return None, None
    try:
        if isinstance(timestamp, str):
            parsed = dt.datetime.fromisoformat(timestamp.replace("Z", "+00:00"))
        else:
            parsed = dt.datetime.fromtimestamp(float(timestamp), tz=dt.UTC)
    except (TypeError, ValueError, OverflowError, OSError):
        return None, None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=dt.UTC)
    local = parsed.astimezone()
    return parsed.astimezone(dt.UTC).isoformat().replace("+00:00", "Z"), local.strftime("%a %-I:%M %p")


def codex_window(name: str, payload: dict[str, Any]) -> dict[str, Any]:
    duration = payload.get("windowDurationMins")
    labels = {300: "5 hours", 10_080: "Weekly"}
    label = labels.get(duration, f"{duration} min" if isinstance(duration, int) else name.title())
    used = payload.get("usedPercent")
    if not isinstance(used, (int, float)):
        raise ValueError(f"Codex {name} window has no usable percentage")
    reset_at, label_text = reset_label(payload.get("resetsAt"))
    result: dict[str, Any] = {
        "id": "five_hour" if duration == 300 else "weekly" if duration == 10_080 else name,
        "label": label,
        "remaining_percent": max(0.0, min(100.0, 100.0 - float(used))),
    }
    if reset_at:
        result["resets_at"] = reset_at
    if label_text:
        result["reset_label"] = label_text
    return result


def normalize_codex(result: dict[str, Any]) -> dict[str, Any]:
    buckets = result.get("rateLimitsByLimitId")
    snapshot = buckets.get("codex") if isinstance(buckets, dict) else None
    if not isinstance(snapshot, dict):
        snapshot = result.get("rateLimits")
    if not isinstance(snapshot, dict):
        raise ValueError("Codex returned no rate-limit snapshot")
    windows = [
        codex_window(name, payload)
        for name in ("primary", "secondary")
        if isinstance((payload := snapshot.get(name)), dict)
    ]
    if not windows:
        raise ValueError("Codex returned no rate-limit windows")
    plan = snapshot.get("planType")
    detail = f"Plan: {plan}" if isinstance(plan, str) else None
    return {
        "id": "codex",
        "label": "Codex",
        "icon": "codex",
        "windows": windows,
        "status": "fresh",
        "detail": detail,
        "usage_url": "https://chatgpt.com/#settings/usage",
    }


def codex() -> dict[str, Any]:
    command = os.environ.get("AGENT_METER_CODEX_BIN", "codex")
    try:
        child = subprocess.Popen(
            [command, "app-server", "--stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
        )
        assert child.stdin is not None and child.stdout is not None
        child.stdin.write(json.dumps({
            "id": 1,
            "method": "initialize",
            "params": {"clientInfo": {"name": "agent-meter", "version": "0.1.0"}},
        }) + "\n")
        child.stdin.flush()
        # Receive the initialization response before issuing account requests.
        for line in child.stdout:
            if json.loads(line).get("id") == 1:
                break
        child.stdin.write(json.dumps({"method": "initialized"}) + "\n")
        child.stdin.write(json.dumps({"id": 2, "method": "account/rateLimits/read", "params": None}) + "\n")
        child.stdin.flush()
        for line in child.stdout:
            message = json.loads(line)
            if message.get("id") == 2:
                if "error" in message:
                    raise RuntimeError("Codex did not authorize a rate-limit read")
                return normalize_codex(message["result"])
        raise RuntimeError("Codex app server ended before returning rate limits")
    except (OSError, ValueError, KeyError, RuntimeError, json.JSONDecodeError) as error:
        return unavailable("codex", "Codex", "codex", f"Codex usage unavailable: {error}")
    finally:
        if "child" in locals():
            if child.poll() is None:
                child.kill()
            child.wait()


def agy_window(group: str, bucket: dict[str, Any]) -> dict[str, Any] | None:
    remaining = bucket.get("remaining_pct")
    if remaining is None and isinstance(bucket.get("remaining_fraction"), (int, float)):
        remaining = float(bucket["remaining_fraction"]) * 100.0
    if not isinstance(remaining, (int, float)):
        return None
    window_name = str(bucket.get("window") or bucket.get("display_name") or "Quota")
    reset_at, label = reset_label(bucket.get("reset_time"))
    result: dict[str, Any] = {
        "id": f"{group.lower().replace(' ', '-')}-{window_name.lower().replace(' ', '-')}",
        "label": f"{window_name} · {group}",
        "remaining_percent": max(0.0, min(100.0, float(remaining))),
    }
    if reset_at:
        result["resets_at"] = reset_at
    if label:
        result["reset_label"] = label
    return result


def normalize_agy(result: dict[str, Any]) -> dict[str, Any]:
    command = result.get("command")
    if isinstance(command, dict) and command.get("name") == "usage":
        summary = command.get("data")
    else:
        summary = result.get("quota_summary")
    if not isinstance(summary, dict):
        raise ValueError(result.get("quota_summary_error") or "agy-usage returned no quota summary")
    windows: list[dict[str, Any]] = []
    for group in summary.get("groups", []):
        if not isinstance(group, dict):
            continue
        group_name = str(group.get("name") or group.get("display_name") or "Antigravity")
        for bucket in group.get("buckets", []):
            if isinstance(bucket, dict) and not bucket.get("disabled"):
                window = agy_window(group_name, bucket)
                if window:
                    windows.append(window)
    if not windows:
        raise ValueError("agy-usage returned no remaining quota windows")
    return {
        "id": "antigravity",
        "label": "Antigravity",
        "icon": "antigravity",
        "windows": windows,
        "status": "fresh",
        "detail": "Antigravity CLI /usage quota",
    }


def claude() -> dict[str, Any]:
    state_home = os.environ.get("XDG_STATE_HOME", os.path.expanduser("~/.local/state"))
    path = os.path.join(state_home, "agent-meter", "claude.json")
    if not os.path.exists(path):
        return unavailable(
            "claude", "Claude Code", "claude",
            "No Claude Code status-line data received yet. "
            "Configure Claude Code statusLine command to ~/.local/bin/agent-meter-claude-statusline",
        )
    try:
        with open(path, encoding="utf-8") as f:
            payload = json.load(f)
        # Saved state is already normalized (written by claude_statusline()).
        if isinstance(payload, dict) and payload.get("id") == "claude" and "windows" in payload:
            state = payload
        else:
            # Re-normalize raw status-line JSON that ended up in the file.
            state = normalize_claude(payload)

        age_seconds = max(0.0, dt.datetime.now().timestamp() - os.path.getmtime(path))
        now = dt.datetime.now(dt.UTC)
        expired = False
        for window in state.get("windows", []):
            reset_at = window.get("resets_at") if isinstance(window, dict) else None
            try:
                parsed = dt.datetime.fromisoformat(str(reset_at).replace("Z", "+00:00"))
                expired = expired or parsed <= now
            except (TypeError, ValueError):
                pass
        if age_seconds > 1800 or expired:
            state = dict(state)
            state["status"] = "stale"
            state["detail"] = "Claude usage is cached; run Claude Code once to refresh its status line."
        return state
    except (OSError, ValueError, TypeError, json.JSONDecodeError) as error:
        return unavailable("claude", "Claude Code", "claude", f"Claude Code state unreadable: {error}")


def agy() -> dict[str, Any]:
    command = os.environ.get("AGENT_METER_AGY_BIN", "agy")
    try:
        output = subprocess.run(
            [command, "-p", "/usage", "--output-format", "json", "--print-timeout", "20s"],
            check=True, capture_output=True, text=True, timeout=30,
        )
        return normalize_agy(json.loads(output.stdout))
    except (OSError, ValueError, KeyError, json.JSONDecodeError, subprocess.SubprocessError) as error:
        return unavailable(
            "antigravity", "Antigravity", "antigravity",
            f"Antigravity usage unavailable; run agy and sign in if prompted: {error}",
        )


def normalize_grok(records: list[Any], now: dt.datetime | None = None) -> dict[str, Any]:
    """Read only Grok's structured billing snapshots, never its auth state."""
    current = now or dt.datetime.now(dt.UTC)
    candidates: list[tuple[dt.datetime, dict[str, Any]]] = []
    for record in records:
        if not isinstance(record, dict) or record.get("msg") != "billing: fetched credits config":
            continue
        ctx = record.get("ctx")
        config = ctx.get("config") if isinstance(ctx, dict) else None
        # Grok's CLI names this field creditUsagePercent, but renders it in the
        # TUI as the percentage remaining. Preserve the provider's displayed
        # value instead of treating it as percentage consumed.
        remaining = _number(config.get("creditUsagePercent")) if isinstance(config, dict) else None
        if remaining is None or not 0 <= remaining <= 100:
            continue
        try:
            observed = dt.datetime.fromisoformat(str(record.get("ts")).replace("Z", "+00:00"))
            period_end = dt.datetime.fromisoformat(str(config.get("billingPeriodEnd")).replace("Z", "+00:00"))
            if observed.tzinfo is None:
                observed = observed.replace(tzinfo=dt.UTC)
            if period_end.tzinfo is None:
                period_end = period_end.replace(tzinfo=dt.UTC)
        except (TypeError, ValueError):
            continue
        if period_end > current:
            candidates.append((observed, config))
    if not candidates:
        raise ValueError("no current Grok billing snapshot; start Grok Build once to refresh it")
    observed, config = max(candidates, key=lambda item: item[0])
    reset_at, reset_text = reset_label(config["billingPeriodEnd"])
    window: dict[str, Any] = {
        "id": "weekly",
        "label": "Weekly",
        "remaining_percent": round(float(config["creditUsagePercent"]), 1),
    }
    if reset_at:
        window["resets_at"] = reset_at
    if reset_text:
        window["reset_label"] = reset_text
    return {
        "id": "grok", "label": "Grok Build", "icon": "grok",
        "windows": [window], "status": "fresh",
        "detail": f"Grok weekly usage snapshot from {observed.astimezone().strftime('%a %-I:%M %p')}",
        "usage_url": "https://grok.com/#settings/usage",
    }


def grok() -> dict[str, Any]:
    log_path = os.environ.get("AGENT_METER_GROK_LOG", os.path.expanduser("~/.grok/logs/unified.jsonl"))
    try:
        records: list[Any] = []
        with open(log_path, encoding="utf-8") as stream:
            for line in stream:
                try:
                    records.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
        return normalize_grok(records)
    except (OSError, ValueError, TypeError) as error:
        return unavailable(
            "grok", "Grok Build", "grok",
            f"Grok usage unavailable; start Grok Build once after signing in: {error}",
        )


def unavailable_provider(name: str) -> dict[str, Any]:
    messages = {
        "grok": ("Grok Build", "grok", "Grok Build CLI has no supported local quota or status-line export."),
        "copilot": ("GitHub Copilot", "copilot", "No supported local remaining-limit adapter is configured."),
        "claude": ("Claude Code", "claude", "No supported local remaining-limit adapter is configured."),
    }
    label, icon, detail = messages.get(name, (name.title(), "generic", "No supported local remaining-limit adapter is configured."))
    return unavailable(name, label, icon, detail)

def claude_window(identifier: str, label: str, bucket: Any) -> dict[str, Any] | None:
    """Normalize Claude Code status-line rate-limit buckets."""
    if not isinstance(bucket, dict):
        return None
    used = bucket.get("used_percentage")
    if not isinstance(used, (int, float)) or isinstance(used, bool) or not 0 <= float(used) <= 100:
        return None
    result: dict[str, Any] = {
        "id": identifier,
        "label": label,
        "remaining_percent": 100.0 - float(used),
    }
    reset_at, reset_text = reset_label(bucket.get("resets_at"))
    if reset_at:
        result["resets_at"] = reset_at
    if reset_text:
        result["reset_label"] = reset_text
    return result


def normalize_claude(payload: dict[str, Any]) -> dict[str, Any]:
    limits = payload.get("rate_limits")
    if not isinstance(limits, dict):
        raise ValueError("Claude status line has no rate_limits object")
    windows = []
    for identifier, label in (("five_hour", "5 hours"), ("seven_day", "Weekly")):
        window = claude_window(identifier, label, limits.get(identifier))
        if window:
            windows.append(window)
    if not windows:
        raise ValueError("Claude status line has no usable rate-limit windows")
    return {
        "id": "claude", "label": "Claude Code", "icon": "claude",
        "windows": windows, "status": "fresh",
        "detail": "Claude Code status-line rate limits",
        "usage_url": "https://claude.ai/settings/usage",
    }


def write_state_file(path: str, state: dict[str, Any]) -> None:
    """Atomically publish state, retaining no raw status-line input."""
    destination = os.path.abspath(os.path.expanduser(path))
    os.makedirs(os.path.dirname(destination), mode=0o700, exist_ok=True)
    os.chmod(os.path.dirname(destination), 0o700)
    fd, temporary = tempfile.mkstemp(prefix=".agent-meter-", dir=os.path.dirname(destination), text=True)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(state, stream, separators=(",", ":"))
            stream.write("\n")
            stream.flush()
            os.fchmod(stream.fileno(), 0o600)
        os.replace(temporary, destination)
    except BaseException:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass
        raise


def claude_statusline() -> int:
    parser = argparse.ArgumentParser(description="Consume Claude Code status-line JSON")
    parser.add_argument("--output", default=os.path.join(os.environ.get("XDG_STATE_HOME", os.path.expanduser("~/.local/state")), "agent-meter", "claude.json"))
    args = parser.parse_args(sys.argv[1:])
    try:
        payload = json.load(sys.stdin)
        state = normalize_claude(payload)
    except (OSError, ValueError, TypeError, json.JSONDecodeError) as error:
        state = unavailable("claude", "Claude Code", "claude", f"Claude Code usage unavailable: {error}")
    write_state_file(args.output, state)
    return 0


def copilot_json(command: list[str]) -> Any:
    result = subprocess.run(command, check=True, capture_output=True, text=True, timeout=20)
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError:
        return result.stdout.strip()


def _number(value: Any) -> float | None:
    return float(value) if isinstance(value, (int, float)) and not isinstance(value, bool) else None


def normalize_copilot(payload: Any, allowance: float | None = None) -> dict[str, Any]:
    """Normalize GitHub billing usage."""
    used = None
    unit = "AI credits"
    # 1. Check for usageItems array from /settings/billing/usage
    if isinstance(payload, dict) and "usageItems" in payload:
        items = payload.get("usageItems") or []
        copilot_items = [
            item for item in items
            if isinstance(item, dict) and (
                "copilot" in str(item.get("product", "")).lower()
                or "copilot" in str(item.get("sku", "")).lower()
            )
        ]
        if copilot_items:
            by_month: dict[str, float] = {}
            undated = 0.0
            for item in copilot_items:
                date_str = str(item.get("date", ""))[:7]  # YYYY-MM
                qty = next((value for key in ("grossQuantity", "quantity", "netQuantity")
                            if (value := _number(item.get(key))) is not None), 0.0)
                if date_str:
                    by_month[date_str] = by_month.get(date_str, 0.0) + qty
                else:
                    undated += qty
                if "request" in str(item.get("unitType", "")).lower() or \
                        "request" in str(item.get("sku", "")).lower():
                    unit = "premium requests"
            if by_month:
                latest_month = max(by_month.keys())
                used = by_month[latest_month]
            elif undated:
                used = undated

    # 2. Check for aggregate total_credits / used_credits in dict / list
    if used is None:
        candidates = payload if isinstance(payload, list) else [payload]
        for item in candidates:
            if not isinstance(item, dict):
                continue
            for key in ("total_credits", "total_premium_requests", "used_credits", "used"):
                used = _number(item.get(key))
                if used is not None:
                    break
            if used is None and isinstance(item.get("usage"), dict):
                usage = item["usage"]
                for key in ("total_credits", "total_premium_requests", "used_credits", "used"):
                    used = _number(usage.get(key))
                    if used is not None:
                        break
            if used is not None:
                break

    # 3. Check for generic usage rows
    if used is None:
        rows = payload.get("usage") if isinstance(payload, dict) else payload
        if isinstance(rows, list):
            quantities = []
            for row in rows:
                if not isinstance(row, dict):
                    continue
                value = next((_number(row.get(key)) for key in ("quantity", "credits", "premium_requests", "requests", "total") if _number(row.get(key)) is not None), None)
                if value is not None:
                    quantities.append(value)
            if quantities:
                used = sum(quantities)

    if used is None:
        raise ValueError("GitHub billing response has no credit usage")

    if allowance is None:
        return unavailable(
            "copilot", "GitHub Copilot", "copilot",
            f"GitHub reports {used:.1f} {unit} used this month. Set an explicit "
            "allowance in ~/.config/agent-meter/copilot.json to calculate remaining usage.",
        )
    if allowance <= 0:
        raise ValueError("Copilot allowance must be greater than zero")

    remaining_percent = max(0.0, min(100.0, 100.0 - (used / allowance * 100.0)))
    detail = f"{used:.1f} of {allowance:g} {unit} used this month"
    return {
        "id": "copilot",
        "label": "GitHub Copilot",
        "icon": "copilot",
        "windows": [
            {
                "id": "monthly",
                "label": "Monthly",
                "remaining_percent": round(remaining_percent, 1),
                "reset_label": "resets 1st",
            }
        ],
        "status": "fresh",
        "detail": detail,
        "usage_url": "https://github.com/settings/billing",
    }


def copilot_allowance() -> float | None:
    raw = os.environ.get("AGENT_METER_COPILOT_ALLOWANCE")
    if raw is None:
        config_home = os.environ.get("XDG_CONFIG_HOME", os.path.expanduser("~/.config"))
        path = os.path.join(config_home, "agent-meter", "copilot.json")
        try:
            with open(path, encoding="utf-8") as stream:
                value = json.load(stream).get("allowance")
            if isinstance(value, (int, float)) and not isinstance(value, bool):
                raw = str(value)
        except (OSError, AttributeError, TypeError, ValueError, json.JSONDecodeError):
            pass
    if raw is None:
        return None
    allowance = float(raw)
    if allowance <= 0:
        raise ValueError("Copilot allowance must be greater than zero")
    return allowance


def normalize_copilot_quota(payload: Any) -> dict[str, Any]:
    snapshots = payload.get("quotaSnapshots") if isinstance(payload, dict) else None
    if not isinstance(snapshots, dict):
        raise ValueError("Copilot returned no account quota snapshots")
    preferred = snapshots.get("premium_interactions")
    selected = [("premium_interactions", preferred)] if isinstance(preferred, dict) else list(snapshots.items())
    windows: list[dict[str, Any]] = []
    details: list[str] = []
    labels = {
        "premium_interactions": "Premium monthly",
        "chat": "Chat monthly",
        "completions": "Completions monthly",
    }
    for identifier, snapshot in selected:
        if not isinstance(snapshot, dict):
            continue
        remaining = _number(snapshot.get("remainingPercentage"))
        entitlement = _number(snapshot.get("entitlementRequests"))
        used = _number(snapshot.get("usedRequests"))
        if remaining is None:
            continue
        reset_at, reset_text = reset_label(snapshot.get("resetDate"))
        window: dict[str, Any] = {
            "id": str(identifier),
            "label": labels.get(str(identifier), str(identifier).replace("_", " ").title()),
            "remaining_percent": max(0.0, min(100.0, round(remaining, 1))),
        }
        if reset_at:
            window["resets_at"] = reset_at
        if reset_text:
            window["reset_label"] = reset_text
        windows.append(window)
        if entitlement == -1:
            details.append(f"{window['label']}: unlimited")
        elif used is not None and entitlement is not None:
            details.append(f"{window['label']}: {used:g} of {entitlement:g} used")
    if not windows:
        raise ValueError("Copilot returned no usable account quota")
    return {
        "id": "copilot", "label": "GitHub Copilot", "icon": "copilot",
        "windows": windows, "status": "fresh",
        "detail": "; ".join(details) or "GitHub Copilot account quota",
        "usage_url": "https://github.com/settings/copilot/features",
    }


def copilot() -> dict[str, Any]:
    try:
        data_home = os.environ.get("XDG_DATA_HOME", os.path.expanduser("~/.local/share"))
        helper = os.environ.get(
            "AGENT_METER_COPILOT_QUOTA_HELPER",
            os.path.join(data_home, "agent-meter", "copilot-sdk", "copilot_quota.mjs"),
        )
        payload = copilot_json(["node", helper])
        return normalize_copilot_quota(payload)
    except (OSError, ValueError, TypeError, json.JSONDecodeError, subprocess.SubprocessError) as error:
        return unavailable(
            "copilot", "GitHub Copilot", "copilot",
            f"GitHub Copilot quota unavailable; launch Copilot CLI and use /login once: {error}",
        )


def main() -> None:
    parser = argparse.ArgumentParser(description="Emit a normalized Agent Meter provider state")
    parser.add_argument("provider", choices=["codex", "agy", "antigravity", "grok", "copilot", "claude"])
    provider = parser.parse_args().provider
    functions = {"codex": codex, "agy": agy, "antigravity": agy, "grok": grok, "copilot": copilot, "claude": claude}
    print(json.dumps(functions[provider](), separators=(",", ":")))


if __name__ == "__main__":
    main()
