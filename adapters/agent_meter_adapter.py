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
import selectors
import subprocess
import sys
import tempfile
import time
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
            try:
                parsed = dt.datetime.fromisoformat(timestamp.replace("Z", "+00:00"))
            except ValueError:
                val = float(timestamp)
                if val > 1e11:
                    val /= 1000.0
                parsed = dt.datetime.fromtimestamp(val, tz=dt.UTC)
        else:
            val = float(timestamp)
            if val > 1e11:
                val /= 1000.0
            parsed = dt.datetime.fromtimestamp(val, tz=dt.UTC)
    except (TypeError, ValueError, OverflowError, OSError):
        return None, None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=dt.UTC)
    local = parsed.astimezone()
    return parsed.astimezone(dt.UTC).isoformat().replace("+00:00", "Z"), local.strftime("%a, %b %-d, %Y · %-I:%M %p %Z")


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


def _read_lines_with_deadline(
    stream: Any,
    deadline: float,
    timeout_message: str = "Codex app server timed out",
):
    sel = selectors.DefaultSelector()
    sel.register(stream, selectors.EVENT_READ)
    buffer = bytearray()
    try:
        while True:
            newline_idx = buffer.find(b"\n")
            if newline_idx != -1:
                line = buffer[:newline_idx].decode("utf-8", errors="replace")
                del buffer[:newline_idx + 1]
                yield line
                continue
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(timeout_message)
            events = sel.select(timeout=max(0.0, remaining))
            if not events:
                raise TimeoutError(timeout_message)
            chunk = os.read(stream.fileno(), 4096)
            if not chunk:
                if buffer:
                    yield buffer.decode("utf-8", errors="replace")
                    buffer.clear()
                break
            buffer.extend(chunk)
    finally:
        sel.close()


def codex(timeout: float | None = None) -> dict[str, Any]:
    command = os.environ.get("AGENT_METER_CODEX_BIN", "codex")
    if timeout is None:
        timeout = float(os.environ.get("AGENT_METER_CODEX_TIMEOUT", "30"))
    deadline = time.monotonic() + timeout
    try:
        child = subprocess.Popen(
            [command, "app-server", "--stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        assert child.stdin is not None and child.stdout is not None
        child.stdin.write(json.dumps({
            "id": 1,
            "method": "initialize",
            "params": {"clientInfo": {"name": "agent-meter", "version": "0.1.0"}},
        }).encode("utf-8") + b"\n")
        child.stdin.flush()
        lines = _read_lines_with_deadline(child.stdout, deadline)
        # Receive the initialization response before issuing account requests.
        initialized = False
        for line in lines:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if msg.get("id") == 1:
                initialized = True
                break
        if not initialized:
            raise RuntimeError("Codex app server ended before returning initialization response")
        child.stdin.write(json.dumps({"method": "initialized"}).encode("utf-8") + b"\n")
        child.stdin.write(json.dumps({"id": 2, "method": "account/rateLimits/read", "params": None}).encode("utf-8") + b"\n")
        child.stdin.flush()
        for line in lines:
            line = line.strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            if message.get("id") == 2:
                if "error" in message:
                    raise RuntimeError("Codex did not authorize a rate-limit read")
                return normalize_codex(message["result"])
        raise RuntimeError("Codex app server ended before returning rate limits")
    except (OSError, ValueError, KeyError, RuntimeError, json.JSONDecodeError, TimeoutError) as error:
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
            if isinstance(window, dict):
                _, label = reset_label(reset_at)
                window["reset_label"] = label or "Reset unknown"
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


def _grok_max_age_seconds() -> float:
    try:
        return float(os.environ.get("AGENT_METER_GROK_MAX_AGE_SECONDS", "1800"))
    except ValueError:
        return 1800.0


def normalize_grok(records: list[Any], now: dt.datetime | None = None) -> dict[str, Any]:
    """Read only Grok's structured billing snapshots, never its auth state.

    Staleness comes from the billing record's own ``ts``. The newest snapshot
    is kept even after its billing period ends; an ended period is stale and
    does not keep a reset instant that is already in the past. This path does
    not consult OMP.
    """
    current = now or dt.datetime.now(dt.UTC)
    if current.tzinfo is None:
        current = current.replace(tzinfo=dt.UTC)
    candidates: list[tuple[dt.datetime, dt.datetime, dict[str, Any]]] = []
    for record in records:
        if not isinstance(record, dict) or record.get("msg") != "billing: fetched credits config":
            continue
        ctx = record.get("ctx")
        config = ctx.get("config") if isinstance(ctx, dict) else None
        # Grok's local billing record reports credits *used*. Convert that to
        # the normalized remaining percentage used by the widget.
        used_percent = _number(config.get("creditUsagePercent")) if isinstance(config, dict) else None
        if used_percent is None or not 0 <= used_percent <= 100:
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
        candidates.append((observed, period_end, config))
    if not candidates:
        raise ValueError("no Grok billing snapshot; start Grok Build once to refresh it")
    observed, period_end, config = max(candidates, key=lambda item: item[0])
    snapshot_age = max(0.0, (current - observed).total_seconds())
    period_ended = period_end <= current
    age_stale = snapshot_age > _grok_max_age_seconds()
    reset_at, reset_text = reset_label(config["billingPeriodEnd"])
    window: dict[str, Any] = {
        "id": "weekly",
        "label": "Weekly",
        "remaining_percent": round(100.0 - float(config["creditUsagePercent"]), 1),
    }
    # A reset instant that has already passed is not the next reset.
    if reset_at and not period_ended:
        window["resets_at"] = reset_at
    if reset_text and not period_ended:
        window["reset_label"] = reset_text
    if period_ended:
        detail = (
            "Grok usage snapshot is from a billing period that ended; "
            "start Grok Build to refresh"
        )
        stale = True
    elif age_stale:
        detail = (
            f"Grok usage snapshot is {snapshot_age / 60:.0f} minutes old; "
            "start Grok Build to refresh"
        )
        stale = True
    else:
        detail = f"Grok weekly usage snapshot from {observed.astimezone().strftime('%a %-I:%M %p')}"
        stale = False
    return {
        "id": "grok", "label": "Grok Build", "icon": "grok",
        "windows": [window], "status": "stale" if stale else "fresh",
        "detail": detail,
        "usage_url": "https://grok.com/#settings/usage",
    }


def normalize_omp_grok_usage(payload: dict[str, Any], now: dt.datetime | None = None) -> dict[str, Any]:
    """Normalize OMP's credential-safe xAI OAuth usage report.

    OMP owns its OAuth session and refreshes it internally. Agent Meter only
    consumes its usage report on stdout; it never reads OMP's auth store.

    A successful OMP read is a live report. ``now`` stamps that fetch. Payload
    clocks are not an observation age; Grok Build log staleness is separate.
    """
    # Stamp the live fetch. Callers pass ``now`` as the fetch time; the report
    # is fresh relative to that instant, so payload timestamps are ignored.
    fetched_at = now or dt.datetime.now(dt.UTC)
    if fetched_at.tzinfo is None:
        fetched_at = fetched_at.replace(tzinfo=dt.UTC)
    reports = payload.get("reports")
    if not isinstance(reports, list):
        raise ValueError("OMP returned no usage reports")
    choices: list[dict[str, Any]] = []
    selected_report: dict[str, Any] | None = None
    for report in reports:
        if not isinstance(report, dict) or report.get("provider") != "xai-oauth":
            continue
        limits = report.get("limits")
        if not isinstance(limits, list):
            continue
        for limit in limits:
            if not isinstance(limit, dict) or limit.get("status") not in ("ok", "warning", "exhausted"):
                continue
            identifier = str(limit.get("id", ""))
            if identifier == "xai-oauth:credits:1w":
                choices.append(limit)
                selected_report = report
    if not choices or selected_report is None:
        raise ValueError("OMP returned no shared Grok usage limit")
    selected = choices[0]
    amount = selected.get("amount")
    window = selected.get("window")
    if not isinstance(amount, dict) or not isinstance(window, dict):
        raise ValueError("OMP Grok usage limit has no amount or reset window")
    fraction = _number(amount.get("remainingFraction"))
    remaining = fraction * 100.0 if fraction is not None else _number(amount.get("remaining"))
    if remaining is None or not 0 <= remaining <= 100:
        raise ValueError("OMP Grok usage limit has no usable remaining percentage")
    reset_value = _number(window.get("resetsAt"))
    reset_at, reset_text = reset_label(reset_value)
    usage_window: dict[str, Any] = {
        "id": "weekly", "label": "Shared weekly",
        "remaining_percent": round(remaining, 1),
    }
    if reset_at:
        usage_window["resets_at"] = reset_at
    if reset_text:
        usage_window["reset_label"] = reset_text

    # ``fetched_at`` is the observation time. Age is zero for a report that
    # OMP just returned, including when the payload carries an older clock.
    if fetched_at.tzinfo is None:  # pragma: no cover - normalized above
        raise ValueError("OMP fetch stamp must be timezone-aware")
    return {
        "id": "grok", "label": "Grok", "icon": "grok",
        "windows": [usage_window], "status": "fresh",
        "detail": "Grok shared weekly credits via OMP OAuth",
        "usage_url": "https://grok.com/#settings/usage",
    }


def _grok_from_omp(now: dt.datetime | None = None) -> dict[str, Any] | None:
    """Return a live OMP report stamped with the fetch time, or None."""
    omp = os.environ.get("AGENT_METER_OMP_BIN", "omp")
    if omp == "omp":
        bun_omp = os.path.expanduser("~/.bun/bin/omp")
        if os.path.isfile(bun_omp) and os.access(bun_omp, os.X_OK):
            omp = bun_omp
    try:
        environment = os.environ.copy()
        omp_dir = os.path.dirname(omp)
        if omp_dir:
            environment["PATH"] = f"{omp_dir}:{environment.get('PATH', '')}"
        output = subprocess.run(
            [omp, "usage", "--json", "--provider", "xai-oauth"],
            check=True, capture_output=True, text=True, timeout=30, env=environment,
        )
        fetched_at = now or dt.datetime.now(dt.UTC)
        return normalize_omp_grok_usage(json.loads(output.stdout), now=fetched_at)
    except (OSError, ValueError, json.JSONDecodeError, subprocess.SubprocessError):
        return None


def _grok_from_log(now: dt.datetime | None = None) -> dict[str, Any]:
    """Normalize the Grok Build log. This path does not invoke OMP."""
    log_path = os.environ.get("AGENT_METER_GROK_LOG", os.path.expanduser("~/.grok/logs/unified.jsonl"))
    try:
        records: list[Any] = []
        with open(log_path, encoding="utf-8") as stream:
            for line in stream:
                try:
                    records.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
        return normalize_grok(records, now=now)
    except (OSError, ValueError, TypeError) as error:
        return unavailable(
            "grok", "Grok Build", "grok",
            f"Grok usage unavailable; start Grok Build once after signing in: {error}",
        )


def grok(now: dt.datetime | None = None) -> dict[str, Any]:
    """Prefer a live OMP report; otherwise read the Grok Build log on its own."""
    omp_state = _grok_from_omp(now)
    if omp_state is not None:
        return omp_state
    return _grok_from_log(now)


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
                "reset_label": "Reset unknown",
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


def normalize_copilot_quota(payload: Any, now: dt.datetime | None = None) -> dict[str, Any]:
    now = now or dt.datetime.now(dt.UTC)
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
        # Observed SDK reset dates can coincide with the fetch time.
        # An elapsed timestamp cannot describe the next quota reset.
        if reset_at and dt.datetime.fromisoformat(reset_at.replace("Z", "+00:00")) <= now:
            reset_at, reset_text = None, None
        window: dict[str, Any] = {
            "id": str(identifier),
            "label": labels.get(str(identifier), str(identifier).replace("_", " ").title()),
            "remaining_percent": max(0.0, min(100.0, round(remaining, 1))),
            "reset_label": "Reset unknown",
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


_MUSE_CLIENT_VERSION = "0.2.0"
_MUSE_NOT_REPORTED = (
    "Muse has not reported subscription usage yet; "
    "Muse only reports usage after a model call (run muse once)"
)


def _muse_max_age_seconds() -> float:
    try:
        return float(os.environ.get("AGENT_METER_MUSE_MAX_AGE_SECONDS", "1800"))
    except ValueError:
        return 1800.0


def _muse_timeout_seconds() -> float:
    try:
        return float(os.environ.get("AGENT_METER_MUSE_TIMEOUT", "20"))
    except ValueError:
        return 20.0


def _muse_cache_path() -> str:
    state_home = os.environ.get("XDG_STATE_HOME", os.path.expanduser("~/.local/state"))
    return os.path.join(state_home, "agent-meter", "muse-usage.json")


def _read_muse_cache() -> dict[str, Any] | None:
    try:
        with open(_muse_cache_path(), encoding="utf-8") as stream:
            payload = json.load(stream)
    except (OSError, json.JSONDecodeError):
        return None
    if not isinstance(payload, dict):
        return None
    payload.pop("tier", None)
    if isinstance(payload.get("window"), dict) or isinstance(payload.get("weekly"), dict):
        return payload
    return None


def _coerce_muse_usage(payload: dict[str, Any]) -> dict[str, Any] | None:
    """Accept a SubscriptionUsage object or a ``{"usage": SubscriptionUsage}`` wrapper.

    ``tier`` is an account id and is never kept.
    """
    if not isinstance(payload, dict):
        return None
    usage = payload.get("usage") if isinstance(payload.get("usage"), dict) else payload
    if not isinstance(usage, dict) or not usage:
        return None
    if not (
        isinstance(usage.get("window"), dict)
        or isinstance(usage.get("weekly"), dict)
        or "observedAtMs" in usage
    ):
        return None
    cleaned = dict(usage)
    cleaned.pop("tier", None)
    return cleaned


def _muse_duration_label(bucket: dict[str, Any]) -> str:
    mins = bucket.get("windowDurationMins")
    if isinstance(mins, bool) or not isinstance(mins, (int, float)):
        return "Session"
    whole = int(mins)
    if whole > 0 and whole % 60 == 0:
        return f"{whole // 60}h"
    if whole > 0:
        return f"{whole}m"
    return "Session"


def _muse_window(
    identifier: str,
    label: str,
    bucket: Any,
    now: dt.datetime,
) -> tuple[dict[str, Any] | None, bool]:
    if not isinstance(bucket, dict):
        return None, False
    used = _number(bucket.get("usedPercent"))
    if used is None:
        return None, False
    # usedPercent may exceed 100. Remaining is clamped to 0..100.
    remaining = max(0.0, min(100.0, 100.0 - float(used)))
    window: dict[str, Any] = {
        "id": identifier,
        "label": label,
        "remaining_percent": round(remaining, 1),
    }
    reset_ms = _number(bucket.get("resetsAtMs"))
    if reset_ms is None:
        return window, False
    try:
        reset_dt = dt.datetime.fromtimestamp(float(reset_ms) / 1000.0, tz=dt.UTC)
    except (OverflowError, OSError, ValueError):
        return window, False
    if reset_dt <= now:
        window["remaining_percent"] = 100.0
        return window, True
    reset_at, reset_text = reset_label(reset_ms)
    if reset_at:
        window["resets_at"] = reset_at
    if reset_text:
        window["reset_label"] = reset_text
    return window, False


def normalize_muse(payload: dict[str, Any], now: dt.datetime | None = None) -> dict[str, Any]:
    """Normalize a Muse SubscriptionUsage observation.

    ``window`` becomes the ``session`` window (its label comes from
    ``windowDurationMins``, for example ``5h``). ``weekly`` stays ``weekly``.
    ``tier`` is omitted. A reset instant that has already passed reports 100%
    remaining, drops ``resets_at``, and marks the provider stale. Age is taken
    from ``observedAtMs``.
    """
    current = now or dt.datetime.now(dt.UTC)
    if current.tzinfo is None:
        current = current.replace(tzinfo=dt.UTC)
    usage = _coerce_muse_usage(payload)
    if usage is None:
        raise ValueError("Muse returned no subscription usage")
    windows: list[dict[str, Any]] = []
    reset_passed = False
    session_bucket = usage.get("window")
    if isinstance(session_bucket, dict):
        window, passed = _muse_window("session", _muse_duration_label(session_bucket), session_bucket, current)
        if window:
            windows.append(window)
            reset_passed = reset_passed or passed
    weekly_bucket = usage.get("weekly")
    if isinstance(weekly_bucket, dict):
        window, passed = _muse_window("weekly", "Weekly", weekly_bucket, current)
        if window:
            windows.append(window)
            reset_passed = reset_passed or passed
    if not windows:
        raise ValueError("Muse returned no usable usage windows")

    observed_ms = _number(usage.get("observedAtMs"))
    age_seconds: float | None
    if observed_ms is None:
        age_stale = True
        age_seconds = None
    else:
        observed = dt.datetime.fromtimestamp(float(observed_ms) / 1000.0, tz=dt.UTC)
        age_seconds = max(0.0, (current - observed).total_seconds())
        age_stale = age_seconds > _muse_max_age_seconds()
    stale = age_stale or reset_passed
    if reset_passed:
        detail = "Muse usage window has reset; run muse once to refresh"
    elif age_stale:
        minutes = 0.0 if age_seconds is None else age_seconds / 60.0
        detail = f"Muse usage snapshot is {minutes:.0f} minutes old; run muse once to refresh"
    else:
        detail = "Muse subscription usage"
    state = {
        "id": "muse",
        "label": "Muse",
        "icon": "generic",
        "windows": windows,
        "status": "stale" if stale else "fresh",
        "detail": detail,
    }
    encoded = json.dumps(state)
    if "tier" in state or "REDACTED_TIER" in encoded:
        raise ValueError("Muse tier must not be included in provider output")
    return state


def _muse_requests() -> list[dict[str, Any]]:
    return [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"clientInfo": {"name": "agent_meter", "version": _MUSE_CLIENT_VERSION}},
        },
        {"jsonrpc": "2.0", "method": "initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "usage/read"},
    ]


def _live_muse_usage(
    result: dict[str, Any] | None,
    notification: dict[str, Any] | None,
) -> dict[str, Any] | None:
    """Prefer usage/read's SubscriptionUsage, else a prior usage/changed params."""
    if isinstance(result, dict) and isinstance(result.get("usage"), dict):
        usage = _coerce_muse_usage(result)
        if usage is not None and (
            isinstance(usage.get("window"), dict) or isinstance(usage.get("weekly"), dict)
        ):
            return usage
    if isinstance(notification, dict):
        usage = _coerce_muse_usage(notification)
        if usage is not None and (
            isinstance(usage.get("window"), dict) or isinstance(usage.get("weekly"), dict)
        ):
            return usage
    return None


def _query_muse(command: str, timeout: float) -> tuple[dict[str, Any] | None, dict[str, Any] | None]:
    """Run ``muse serve --no-session-log`` and read the usage/read result.

    Notifications are skipped, except ``usage/changed`` whose params use the
    SubscriptionUsage shape and are kept if they arrive before the response.
    Returns ``(result, notification)``. ``result`` is ``{}`` when the host has
    observed nothing. A captured trace whose usage result id is not 2 is still
    accepted when stdout ends, so fixture replay works.
    """
    deadline = time.monotonic() + timeout
    with tempfile.TemporaryDirectory(prefix="agent-meter-muse-") as workdir:
        child = subprocess.Popen(
            [command, "serve", "--no-session-log"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            cwd=workdir,
        )
        notification: dict[str, Any] | None = None
        result: dict[str, Any] | None = None
        try:
            if child.stdin is None or child.stdout is None:
                raise RuntimeError("muse serve did not provide stdio pipes")
            for payload in _muse_requests():
                child.stdin.write(json.dumps(payload, separators=(",", ":")).encode("utf-8") + b"\n")
            child.stdin.flush()
            fallback: dict[str, Any] | None = None
            saw_response = False
            for line in _read_lines_with_deadline(child.stdout, deadline, "Muse serve timed out"):
                line = line.strip()
                if not line:
                    continue
                try:
                    message = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if not isinstance(message, dict):
                    continue
                if message.get("method") == "usage/changed" and isinstance(message.get("params"), dict):
                    notification = message["params"]
                    continue
                if message.get("id") == 2 and "result" in message:
                    if isinstance(message.get("result"), dict):
                        result = message["result"]
                    saw_response = True
                    break
                body = message.get("result")
                if isinstance(body, dict) and isinstance(body.get("usage"), dict):
                    fallback = body
            if not saw_response and result is None:
                result = fallback
            return result, notification
        finally:
            if child.stdin is not None:
                try:
                    child.stdin.close()
                except BrokenPipeError:
                    pass
            if child.poll() is None:
                child.kill()
            try:
                child.wait(timeout=2)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()


def muse(now: dt.datetime | None = None) -> dict[str, Any]:
    """Read Muse subscription usage over the MSP stdio protocol."""
    command = os.environ.get("AGENT_METER_MUSE_BIN", "muse")
    try:
        result, notification = _query_muse(command, _muse_timeout_seconds())
    except FileNotFoundError:
        return unavailable(
            "muse", "Muse", "generic",
            "muse is not installed; install Muse and sign in to see usage.",
        )
    except (OSError, TimeoutError, RuntimeError, json.JSONDecodeError) as error:
        return unavailable(
            "muse", "Muse", "generic",
            f"Muse usage unavailable: {error}",
        )

    live = _live_muse_usage(result, notification)
    if live is not None:
        try:
            write_state_file(_muse_cache_path(), live)
        except OSError:
            pass
        try:
            return normalize_muse(live, now=now)
        except (TypeError, ValueError) as error:
            return unavailable(
                "muse", "Muse", "generic",
                f"Muse usage unavailable: {error}",
            )

    # usage/read returned an empty observation. Muse only fills this in after
    # a model call, so a fresh host falls back to the last cached snapshot.
    if result == {}:
        cached = _read_muse_cache()
        if cached is not None:
            try:
                return normalize_muse(cached, now=now)
            except (TypeError, ValueError) as error:
                return unavailable(
                    "muse", "Muse", "generic",
                    f"Muse usage unavailable: {error}",
                )
        return unavailable("muse", "Muse", "generic", _MUSE_NOT_REPORTED)

    return unavailable(
        "muse", "Muse", "generic",
        "Muse usage unavailable: muse serve ended before reporting usage",
    )


def main() -> None:
    parser = argparse.ArgumentParser(description="Emit a normalized Agent Meter provider state")
    parser.add_argument("provider", choices=["codex", "agy", "antigravity", "grok", "copilot", "claude", "muse"])
    provider = parser.parse_args().provider
    functions = {"codex": codex, "agy": agy, "antigravity": agy, "grok": grok, "copilot": copilot, "claude": claude, "muse": muse}
    print(json.dumps(functions[provider](), separators=(",", ":")))


if __name__ == "__main__":
    main()
